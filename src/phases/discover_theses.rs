//! `discover_theses` — the thesis fan-out of `moagan discover`.
//!
//! The fan-out is a fixed, ordered list of points (provider pair →
//! cell → temperature → replica → index); point `n` writes
//! `theses/th_{n:04}.json`. A point is skipped iff that file exists,
//! so re-running the phase on the same run dir only redoes the points
//! that were rejected, failed or never ran.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tracing::Instrument;

use crate::discovery::matrix::{ExplorationMatrix, MatrixCell, TemperatureProfile};
use crate::discovery::thesis_prompt::ThesisPromptContext;
use crate::discovery::thesis_retry::retry_thesis_extraction;
use crate::domain::{Thesis, ThesisProvenance};
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::{discover_matrix_system_prompt, system_prompt};
use crate::phases::util::{read_json, write_json};
use crate::phases::{Phase, PhaseOutput, RunContext};

/// File the phase reads the matrix from, or writes it to once built.
pub const EXPLORATION_MATRIX_FILENAME: &str = "exploration_matrix.json";

/// Shortest claim (trimmed bytes) a thesis needs to be kept.
pub const MIN_CLAIM_LEN: usize = 30;

/// Uncached retries after the first (cached) call of a point.
pub const THESIS_RETRIES: u32 = 2;

/// Runs the thesis fan-out; see the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoverThesesPhase;

/// One point of the fan-out.
#[derive(Debug, Clone, PartialEq)]
pub struct FanoutPoint {
    /// Position in the fan-out; the thesis id is `th_{n:04}`.
    pub n: usize,
    /// Provider section the call goes to.
    pub section: String,
    /// Model the call goes to.
    pub model: String,
    /// Matrix cell of the thesis.
    pub cell: MatrixCell,
    /// Sampling temperature of the call.
    pub temperature: f32,
    /// Replica number within the temperature.
    pub replica: usize,
    /// Thesis number within the cell, temperature and replica.
    pub index: usize,
}

impl FanoutPoint {
    /// Canonical thesis id of this point: `th_` plus `n` padded to 4 digits.
    pub fn thesis_id(&self) -> String {
        format!("th_{:04}", self.n)
    }
}

/// What one fan-out point produced.
#[derive(Debug)]
pub enum PointOutcome {
    /// A thesis whose claim is long enough; it must be written.
    Accepted(Box<Thesis>),
    /// A parsed thesis whose trimmed claim is shorter than [`MIN_CLAIM_LEN`].
    Rejected {
        /// Trimmed claim length in bytes.
        claim_len: usize,
    },
    /// No thesis: the call or the parse failed after its retries.
    Failed(Error),
}

/// Every fan-out point in order: provider pair (in the given order) →
/// cell → temperature → replica → index, numbered from 0. Replicas and
/// `theses_per_cell` below 1 count as 1.
pub fn fanout(
    pairs: &[(String, String, TemperatureProfile)],
    cells: &[MatrixCell],
    theses_per_cell: usize,
) -> Vec<FanoutPoint> {
    let per_cell = theses_per_cell.max(1);
    let mut points = Vec::new();
    for (section, model, profile) in pairs {
        let replicas = profile.replicas_per_temperature.max(1);
        for cell in cells {
            for &temperature in &profile.temperatures {
                for replica in 0..replicas {
                    for index in 0..per_cell {
                        points.push(FanoutPoint {
                            n: points.len(),
                            section: section.clone(),
                            model: model.clone(),
                            cell: cell.clone(),
                            temperature,
                            replica,
                            index,
                        });
                    }
                }
            }
        }
    }
    points
}

/// The points of `points` whose `<theses_dir>/<thesis_id>.json` is
/// not a file, in their original order.
pub fn pending(points: Vec<FanoutPoint>, theses_dir: &Path) -> Vec<FanoutPoint> {
    points
        .into_iter()
        .filter(|point| !thesis_path(theses_dir, point).is_file())
        .collect()
}

/// Classify the result of one point: an error is `Failed`, a thesis
/// whose trimmed claim has fewer than [`MIN_CLAIM_LEN`] bytes is
/// `Rejected`, anything else is `Accepted`.
pub fn classify(result: Result<Thesis>) -> PointOutcome {
    match result {
        Err(e) => PointOutcome::Failed(e),
        Ok(thesis) => {
            let claim_len = thesis.claim.trim().len();
            if claim_len < MIN_CLAIM_LEN {
                PointOutcome::Rejected { claim_len }
            } else {
                PointOutcome::Accepted(Box::new(thesis))
            }
        }
    }
}

/// Gate the thesis parsed on attempt `attempt` (0-based, out of
/// `1 + THESIS_RETRIES`). A trimmed claim shorter than
/// [`MIN_CLAIM_LEN`] is an `Error::SchemaViolation` on every attempt
/// but the last, so the point is asked again uncached like an answer
/// that does not parse; on the last attempt the thesis is returned
/// and [`classify`] marks the point `Rejected`. A long enough claim
/// is returned unchanged on any attempt.
pub fn gate_short_claim(thesis: Thesis, attempt: u32) -> Result<Thesis> {
    let claim_len = thesis.claim.trim().len();
    if claim_len < MIN_CLAIM_LEN && attempt < THESIS_RETRIES {
        return Err(Error::SchemaViolation(format!(
            "thesis claim too short: {claim_len} bytes, at least {MIN_CLAIM_LEN} expected"
        )));
    }
    Ok(thesis)
}

/// The run's matrix. When `<run_dir>/exploration_matrix.json` exists
/// it is returned verbatim. Otherwise the matrix is built from the
/// dimensions sidecar, else `ctx.config.discovery_matrix.matrix_spec`,
/// else its `dimensions × facets_per_dimension` pair, else empty; it
/// takes `theses_per_cell`, `temperature_profiles` and
/// `default_profile` from the same config block, migrates legacy
/// profile keys, snaps temperatures to the probed supported set, and is
/// written to `exploration_matrix.json` before it is returned.
pub fn load_or_build_matrix(ctx: &RunContext) -> Result<ExplorationMatrix> {
    let run_dir = ctx.run_dir().root().to_path_buf();
    let path = run_dir.join(EXPLORATION_MATRIX_FILENAME);
    if path.is_file() {
        let matrix: ExplorationMatrix = read_json(&path)?;
        tracing::info!(
            cells = matrix.cells(),
            theses_per_cell = matrix.theses_per_cell,
            "discover_theses: matrix loaded from exploration_matrix.json"
        );
        return Ok(matrix);
    }
    let cfg = &ctx.config.discovery_matrix;
    let mut matrix = build_matrix(&run_dir, cfg.theses_per_cell, cfg)?;
    matrix.temperature_profiles = cfg.temperature_profiles.clone();
    matrix.default_profile = cfg.default_profile.clone().unwrap_or_default();
    let rewritten = matrix.migrate_legacy_keys(&ctx.default_provider, &ctx.default_model);
    if rewritten > 0 {
        tracing::info!(
            rewritten,
            "discover_theses: legacy temperature-profile keys migrated to section::model"
        );
    }
    if let Some(table) = ctx.temperature_table.as_ref() {
        let mut supported_sets = std::collections::HashMap::new();
        for joined in matrix.temperature_profiles.keys() {
            let (section, model) = match joined.split_once("::") {
                Some((s, m)) => (s, m),
                None => (ctx.default_provider.as_str(), joined.as_str()),
            };
            let set = table.supported_for(section, model);
            if !set.is_empty() {
                supported_sets.insert(joined.clone(), set);
            }
        }
        for e in matrix.rewrite_temperatures_to_supported(&supported_sets) {
            tracing::warn!(
                provider_model = %e.provider_model,
                n_clamped = e.n_clamped,
                dropped_count = e.dropped_count,
                requested = ?e.requested,
                clamped_to = ?e.clamped_to,
                "temperature profile rewritten to nearest supported values"
            );
        }
    }
    write_json(&path, &matrix)?;
    tracing::info!(
        cells = matrix.cells(),
        theses_per_cell = matrix.theses_per_cell,
        "discover_theses: matrix built and persisted"
    );
    Ok(matrix)
}

/// Matrix dimensions from, in order: the dimensions sidecar, the
/// config's matrix spec, its dimension × facet counts, or nothing.
fn build_matrix(
    run_dir: &Path,
    theses_per_cell: usize,
    cfg: &crate::config::DiscoveryMatrixConfig,
) -> Result<ExplorationMatrix> {
    if let Some(matrix) = ExplorationMatrix::load_or_derive(run_dir, theses_per_cell)? {
        return Ok(matrix);
    }
    let entries: Vec<String> = cfg
        .matrix_spec
        .iter()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect();
    if !entries.is_empty() {
        let spec = crate::discovery::MatrixSpec::parse_all(entries)?;
        spec.validate()?;
        return Ok(ExplorationMatrix::from_spec(spec, theses_per_cell));
    }
    if let (Some(dims), Some(facets)) = (cfg.dimensions, cfg.facets_per_dimension) {
        let mut spec = crate::discovery::MatrixSpec::default();
        for i in 0..dims.max(1) {
            let facets = (0..facets.max(1))
                .map(|j| crate::discovery::FacetSpec {
                    id: format!("f{}", j + 1),
                    label: format!("F{}", j + 1),
                    description: String::new(),
                })
                .collect();
            spec.dimensions.push(crate::discovery::DimensionSpec {
                id: format!("dim-{i:02}"),
                label: format!("Dimension {i}"),
                facets,
            });
        }
        return Ok(ExplorationMatrix::from_spec(spec, theses_per_cell));
    }
    Ok(ExplorationMatrix::new(Vec::new(), theses_per_cell))
}

#[async_trait]
impl Phase for DiscoverThesesPhase {
    fn name(&self) -> &'static str {
        "discover_theses"
    }

    /// Run every pending point under the run's parallelism cap and
    /// write each accepted thesis. Rejected and failed points leave no
    /// file. Fails only on cancellation or when the matrix or the
    /// prompt inputs cannot be read.
    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let matrix = load_or_build_matrix(ctx)?;
        let pairs: Vec<(String, String, TemperatureProfile)> = matrix
            .active_provider_profiles(&ctx.default_provider, &ctx.default_model)
            .into_iter()
            .filter(|(section, model, _)| {
                let hosted = ctx.has_provider_for(section, model);
                if !hosted {
                    tracing::warn!(
                        section = %section,
                        model = %model,
                        "discover_theses: profile names a pair outside the provider registry; skipped"
                    );
                }
                hosted
            })
            .collect();
        let cells: Vec<MatrixCell> = matrix.iter_cells().collect();
        let points = fanout(&pairs, &cells, matrix.theses_per_cell);
        let total = points.len();
        let theses_dir = ctx.run_dir().theses();
        std::fs::create_dir_all(&theses_dir)?;
        let todo = pending(points, &theses_dir);
        tracing::info!(
            total,
            pending = todo.len(),
            skipped = total - todo.len(),
            "discover_theses: fan-out planned"
        );
        if todo.is_empty() {
            return Ok(PhaseOutput::Theses(Vec::new()));
        }

        let prompt_ctx = Arc::new(ThesisPromptContext::load(ctx.run_dir().root())?);
        let system = Arc::new(discover_matrix_system_prompt().to_owned());
        let shared = Arc::new(ctx.clone());
        let mut join_set: tokio::task::JoinSet<(FanoutPoint, PointOutcome)> =
            tokio::task::JoinSet::new();
        for point in todo {
            if ctx.cancel().is_cancelled() {
                break;
            }
            let span = tracing::trace_span!(
                "iteration",
                n = point.n,
                total,
                cell_dim = %point.cell.dimension_id,
                cell_facet = %point.cell.facet_id,
                temperature = point.temperature,
            );
            let ctx = Arc::clone(&shared);
            let prompt_ctx = Arc::clone(&prompt_ctx);
            let system = Arc::clone(&system);
            join_set.spawn(
                async move {
                    let result = generate(&ctx, &prompt_ctx, &system, &point).await;
                    (point, classify(result))
                }
                .instrument(span),
            );
        }

        let mut written = Vec::new();
        let (mut rejected, mut failed) = (0usize, 0usize);
        while let Some(joined) = join_set.join_next().await {
            let (point, outcome) = match joined {
                Ok(pair) => pair,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(error = %e, "discover_theses: task aborted");
                    continue;
                }
            };
            let label = match outcome {
                PointOutcome::Accepted(thesis) => {
                    let path = thesis_path(&theses_dir, &point);
                    match write_json(&path, &thesis) {
                        Ok(()) => {
                            written.push(path);
                            "accepted"
                        }
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                n = point.n,
                                error = %e,
                                "discover_theses: thesis could not be written"
                            );
                            "error"
                        }
                    }
                }
                PointOutcome::Rejected { claim_len } => {
                    rejected += 1;
                    tracing::warn!(
                        n = point.n,
                        claim_len,
                        "discover_theses: thesis rejected (claim too short)"
                    );
                    "rejected"
                }
                PointOutcome::Failed(e) => {
                    failed += 1;
                    tracing::warn!(
                        n = point.n,
                        error = %e,
                        "discover_theses: thesis failed after retries"
                    );
                    "error"
                }
            };
            emit_iteration(&point, total, label);
        }
        if ctx.cancel().is_cancelled() {
            return Err(ctx.cancel().into_error());
        }
        tracing::info!(
            written = written.len(),
            rejected,
            failed,
            "discover_theses: fan-out finished"
        );
        written.sort();
        Ok(PhaseOutput::Theses(written))
    }
}

/// One point: wait for a parallelism permit, call the model (first
/// attempt cached, retries uncached, 1 + [`THESIS_RETRIES`] attempts),
/// parse the thesis, retry a too-short claim ([`gate_short_claim`])
/// and stamp its id, angle and provenance from the point.
async fn generate(
    ctx: &RunContext,
    prompt_ctx: &ThesisPromptContext,
    system: &str,
    point: &FanoutPoint,
) -> Result<Thesis> {
    if ctx.cancel().is_cancelled() {
        return Err(ctx.cancel().into_error());
    }
    let _permit = ctx.parallelism.acquire().await?;
    let user = prompt_ctx.user_payload(&point.cell, point.n);
    let schema_hint = system_prompt(Role::Thesis);
    let mut attempt: u32 = 0;
    retry_thesis_extraction(THESIS_RETRIES, || {
        let this_attempt = attempt;
        attempt += 1;
        let user = user.clone();
        let system = system.to_owned();
        async move {
            let raw = if this_attempt == 0 {
                ctx.call_with_retry_at_temp_for(
                    &point.section,
                    &point.model,
                    Role::Thesis,
                    system,
                    user,
                    0,
                    point.temperature,
                )
                .await?
            } else {
                ctx.call_uncached_at_temp_for(
                    &point.section,
                    &point.model,
                    Role::Thesis,
                    system,
                    user,
                    crate::time::now_unix_secs(),
                    this_attempt,
                    point.temperature,
                )
                .await?
            };
            let parsed: Thesis = ctx.parse_model_json(Role::Thesis, &raw.text, schema_hint)?;
            let mut thesis = gate_short_claim(parsed, this_attempt)?;
            thesis.id = point.thesis_id();
            thesis.angle = format!("{}:{}", point.cell.dimension_id, point.cell.facet_id);
            thesis.provenance = Some(ThesisProvenance {
                section: point.section.clone(),
                model: point.model.clone(),
                temperature: point.temperature,
                replica: point.replica,
                index: point.index,
                source_run: None,
                source_thesis: None,
            });
            Ok(thesis)
        }
    })
    .await
}

/// Mirror one finished point on the stdout event stream
/// (`kind = "discovery_iteration"`) when JSONL events are on.
fn emit_iteration(point: &FanoutPoint, total: usize, outcome: &'static str) {
    use crate::telemetry::stdout_events::{
        Event, EventFormat, SCHEMA_VERSION, STDOUT_EVENTS, now_rfc3339, resolve_event_format,
    };
    if !resolve_event_format(EventFormat::Jsonl) {
        return;
    }
    STDOUT_EVENTS.emit(Event::DiscoveryIteration {
        schema: SCHEMA_VERSION,
        ts: now_rfc3339(),
        n: point.n,
        total,
        section: point.section.as_str(),
        model: point.model.as_str(),
        cell_dim: point.cell.dimension_id.as_str(),
        cell_facet: point.cell.facet_id.as_str(),
        temperature: point.temperature,
        replica: point.replica,
        thesis_index: point.index,
        outcome,
    });
}

/// Path of the thesis file of `point` under `theses_dir`.
pub fn thesis_path(theses_dir: &Path, point: &FanoutPoint) -> PathBuf {
    theses_dir.join(format!("{}.json", point.thesis_id()))
}

#[cfg(test)]
mod tests;
