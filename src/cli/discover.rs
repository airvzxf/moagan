//! `moagan discover` — discovery mode (Plan B sub-phase B).
//!
//! Discovery is a separate subcommand (not a `--mode discovery` flag on
//! `moagan run`) because its pipeline diverges heavily from the
//! linear `intake → clarify → route → propose → ...` flow:
//!
//! 1. Build an `ExplorationMatrix` (roles × models × temperatures).
//! 2. Fan out 80+ sketches via `DiscoverMatrixPhase`.
//! 3. Tag each sketch with `DiscoverTagPhase` (LLM tagger).
//! 4. Cluster via SimHash + LLM refinement (`DiscoverClusterPhase`).
//! 5. Detect cross-cluster contradictions (`DiscoverContradictPhase`).
//! 6. Derive facets per category (`DiscoverFacetPhase`).
//! 7. Extract per-facet markdown (`DiscoverExtractPhase`).
//! 8. Integrate into `final/cat_NN.md` + `final/summary.md`
//!    (`DiscoverIntegratePhase`).
//!
//! The output is a *biblia* (knowledge base), not a winning proposal.
//!
//! Discovery deliberately does NOT route through `cmd::Run::Run`:
//! - It uses different LLM roles (`tagger`, `extractor`, `integrator`).
//! - It writes to `tags/`, `clusters/`, `facets/`, `extractions/`,
//!   `final/cat_NN.md` instead of the standard `proposals/` path.
//! - It does not produce a `ranking.json` or `portfolio.md`.
//!
//! Cardinality minimum is 80 sketches; the spec says 40–500 (V4 §6.4)
//! and the user's Plan B preferred the upper half of the lower band.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, info, trace, warn};

use crate::cli::flags_batch;
use crate::config::Config;
use crate::discovery::matrix::ExplorationMatrix;
use crate::discovery::matrix_spec::MatrixSpec;
use crate::discovery::{DiscoveryCoordinator, DiscoveryOutcome};
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::Role;
use crate::phases::Pipeline;
use crate::phases::PipelineKind;
use crate::phases::RunContext;
use crate::phases::{
    ClarifyPhase, DiscoverClusterPhase, DiscoverContradictPhase, DiscoverDimensionsPhase,
    DiscoverExtractPhase, DiscoverFacetPhase, DiscoverIntegratePhase, DiscoverMatrixPhase,
    DiscoverSummaryPhase, DiscoverTagPhase, IntakePhase,
};
use crate::redact::RedactPolicy;
use crate::storage::sqlite::Db;
use crate::telemetry::Telemetry;

use crate::cli::continue_cmd::load_manifest;
use crate::domain::Manifest;

/// F2 (Track G.2) default `sketches_per_cell`. The matrix's
/// per-cell fan-out is `cells() * sketches_per_cell`. F2 lowers
/// the v0.5 floor from `cardinality = 80` to
/// `sketches_per_cell = 10` so the per-cell fan-out is the
/// explicit knob (an operator who wants 80 sketches on a 4×2
/// matrix sets `sketches_per_cell = 20`).
pub const DEFAULT_SKETCHES_PER_CELL: usize = 10;

/// F2 minimum allowed `sketches_per_cell`. Used by the CLI
/// dispatcher's `--sketches-per-cell` validator AND the
/// `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env-var parser so both
/// surfaces reject the same floor. F2.x (v0.13.2) lowered the
/// operator-facing floor to 1 to support debug / integration
/// runs; default is unchanged at 10 to preserve the v0.5
/// cardinality contract for nominal discovery runs.
pub const MIN_SKETCHES_PER_CELL: usize = 1;

/// Resolve the operator's input into an [`ExplorationMatrix`].
///
/// F1 (Track G.2) resolves in this order, first match wins:
///
/// 1. `--matrix-spec` (CLI flag, repetitive or consolidated) →
///    [`MatrixSpec::parse_all`]. The matrix uses the spec verbatim;
///    the `discover_dimensions` phase is skipped.
/// 2. `--dimensions N --facets-per-dimension M` (CLI flag pair, no
///    spec) → build a programmatic spec with `N × M` cells. The
///    matrix uses the spec verbatim; the `discover_dimensions`
///    phase is skipped. Skipped when `--llm-derive` is `true` (the
///    explicit LLM-derive opt-in overrides the count shortcut).
/// 3. `--llm-derive` or `--dimensions N` (no spec, no per-dim
///    count) → [`crate::phases::DiscoverDimensionsPhase`] runs at
///    runtime to derive the dimension list from the brief via the
///    LLM. The matrix's dimensions are loaded from the
///    `<run_dir>/discovery_dimensions.json` sidecar.
/// 4. No flag → same as (3) with no target count; the LLM is free
///    to pick any number of dimensions with asymmetric facets.
///
/// F2 (Track G.2) decouples the per-cell fan-out from the
/// matrix's `cells()`: `sketches_per_cell` is the operator's
/// explicit knob (CLI flag, env var, or TOML value) — no
/// integer-division shortfall between cardinality and cells.
pub fn resolve_matrix(opts: &DiscoverOptions, _cfg: &Config) -> Result<(MatrixSpec, usize)> {
    let sketches_per_cell = opts.sketches_per_cell;
    debug!(
        sketches_per_cell,
        matrix_spec_len = opts.matrix_spec.len(),
        llm_derive = opts.llm_derive,
        dimensions = ?opts.dimensions,
        facets_per_dimension = ?opts.facets_per_dimension,
        "resolve_matrix: enter"
    );
    if let Some(spec) = parse_matrix_spec_inputs(&opts.matrix_spec)? {
        debug!(dims = spec.dimensions.len(), "resolve_matrix: spec path");
        return Ok((spec, sketches_per_cell));
    }
    if opts.llm_derive {
        debug!("resolve_matrix: llm_derive path");
        return Ok((MatrixSpec::default(), sketches_per_cell));
    }
    if let (Some(_dims), Some(facets_per_dim)) = (opts.dimensions, opts.facets_per_dimension) {
        // Legacy `--dimensions N --facets-per-dimension M` pair —
        // build a programmatic spec with placeholder ids. The
        // operator uses this for tests; the LLM-derive path
        // (above) is the preferred default.
        let n = opts.dimensions.unwrap_or(1);
        let mut spec = MatrixSpec::default();
        for i in 0..n.max(1) {
            let id = format!("dim-{:02}", i);
            let mut facets = Vec::with_capacity(facets_per_dim.max(1));
            for j in 0..facets_per_dim.max(1) {
                facets.push(crate::discovery::matrix_spec::FacetSpec {
                    id: format!("f{}", j + 1),
                    label: format!("F{}", j + 1),
                    description: String::new(),
                });
            }
            spec.dimensions
                .push(crate::discovery::matrix_spec::DimensionSpec {
                    id,
                    label: format!("Dimension {}", i),
                    facets,
                });
        }
        trace!(
            dims = spec.dimensions.len(),
            "resolve_matrix: legacy dim×facet path"
        );
        return Ok((spec, sketches_per_cell));
    }
    if opts.dimensions.is_some() {
        // Operator passed `--dimensions N` without `--facets-per-dimension`.
        // The LLM picks the facet count asymmetrically per
        // dimension; the `discover_dimensions` phase owns the
        // selection.
        debug!("resolve_matrix: dimensions-only, LLM picks facets");
        return Ok((MatrixSpec::default(), sketches_per_cell));
    }
    // No flag at all — full LLM-derive.
    debug!("resolve_matrix: full LLM-derive fallback");
    Ok((MatrixSpec::default(), sketches_per_cell))
}

/// Parse and validate the operator's `--matrix-spec` inputs.
/// Returns `Ok(None)` when every entry is empty (the caller falls
/// back to LLM-derive or the legacy count pair). Returns
/// `Err(Error::InvalidArgs)` on the first malformed entry so the
/// dispatcher surfaces a clear CLI message.
fn parse_matrix_spec_inputs(entries: &[String]) -> Result<Option<MatrixSpec>> {
    let non_empty: Vec<&String> = entries.iter().filter(|s| !s.trim().is_empty()).collect();
    if non_empty.is_empty() {
        trace!(
            entries = entries.len(),
            "parse_matrix_spec_inputs: all empty"
        );
        return Ok(None);
    }
    let parsed = MatrixSpec::parse_all(non_empty.into_iter().cloned())?;
    parsed.validate()?;
    Ok(Some(parsed))
}

/// Build the discovery pipeline. The phases are wired in the order
/// they appear in V4 §6.3:
///
/// 1. intake + clarify (mandatory seeding of the brief).
/// 2. discover_dimensions (F1: LLM-derive or skip when --matrix-spec).
/// 3. discover_matrix (sketch fan-out).
/// 4. discover_tag (LLM tagger).
/// 5. discover_cluster (SimHash + LLM refinement).
/// 6. discover_contradict (cross-cluster disagreements).
/// 7. discover_facet (per-cluster facet list).
/// 8. discover_extract (per-facet markdown).
/// 9. discover_integrate (one `final/cat_NN.md` per cluster).
/// 10. discover_summary (executive index + optional uncategorized).
///
/// F1 (Track G.2) inserts `DiscoverDimensionsPhase` between
/// `ClarifyPhase` and `DiscoverMatrixPhase`. The phase is a
/// no-op when a `--matrix-spec` is supplied (the matrix uses the
/// spec verbatim) and an active LLM-derive when the operator
/// passed `--llm-derive` or no spec at all.
pub fn build_discovery_pipeline(opts: &DiscoverOptions, cfg: &Config) -> Pipeline {
    debug!("build_discovery_pipeline: enter");
    let (spec, _sketches_per_cell) =
        resolve_matrix(opts, cfg).unwrap_or((MatrixSpec::default(), 10));
    let needs_dimensions_phase = spec.dimensions.is_empty();
    let mut pipeline = Pipeline::new().push(IntakePhase).push(ClarifyPhase);
    if needs_dimensions_phase {
        pipeline = pipeline.push(DiscoverDimensionsPhase);
    }
    pipeline
        .push(DiscoverMatrixPhase::new(ExplorationMatrix::from_spec(
            spec,
            _sketches_per_cell,
        )))
        .push(DiscoverTagPhase)
        .push(DiscoverClusterPhase {
            threshold: opts.cluster_threshold,
        })
        .push(DiscoverContradictPhase::default())
        .push(DiscoverFacetPhase::with_cache(opts.cache_facets))
        .push(DiscoverExtractPhase)
        .push(DiscoverIntegratePhase)
        .push(DiscoverSummaryPhase)
}

/// Build the pre-matrix pipeline (intake + clarify + optional
/// dimensions derivation). PR-17 splits the discovery flow so the
/// sketch fan-out is driven by [`DiscoveryCoordinator::run_with_ctx`],
/// not by the pipeline runner. Keeping `intake` + `clarify` in the
/// pipeline preserves the pause/resume hooks at those phase
/// boundaries.
fn build_pre_matrix_pipeline(opts: &DiscoverOptions, cfg: &Config) -> Pipeline {
    debug!("build_pre_matrix_pipeline: enter");
    let (spec, _sketches_per_cell) =
        resolve_matrix(opts, cfg).unwrap_or((MatrixSpec::default(), 10));
    let mut pipeline = Pipeline::new().push(IntakePhase).push(ClarifyPhase);
    if spec.dimensions.is_empty() {
        pipeline = pipeline.push(DiscoverDimensionsPhase);
    }
    pipeline
}

/// Build the post-matrix pipeline (tag → cluster → … → summary).
/// PR-17 keeps these phases in the flat pipeline runner; the
/// coordinator owns only the matrix part. The pipeline's per-phase
/// cancel token still surfaces as a `StopDecision` at the matrix
/// boundary when the operator presses Ctrl-C.
fn build_post_matrix_pipeline(opts: &DiscoverOptions) -> Pipeline {
    debug!(
        cluster_threshold = opts.cluster_threshold,
        "build_post_matrix_pipeline: enter"
    );
    Pipeline::new()
        .push(DiscoverTagPhase)
        .push(DiscoverClusterPhase {
            threshold: opts.cluster_threshold,
        })
        .push(DiscoverContradictPhase::default())
        .push(DiscoverFacetPhase::with_cache(opts.cache_facets))
        .push(DiscoverExtractPhase)
        .push(DiscoverIntegratePhase)
        .push(DiscoverSummaryPhase)
}

/// Options for `moagan discover`.
#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    /// Provider name (must be in config).
    pub provider: String,
    /// User prompt.
    pub prompt: String,
    /// Optional override of the home directory.
    pub home: Option<PathBuf>,
    /// Optional directory of canned mock responses.
    pub mock_dir: Option<PathBuf>,
    /// F2 (Track G.2): sketches per matrix cell. The matrix
    /// fan-out is `cells() × sketches_per_cell ×
    /// profile_total`. Default 10; floor 1 (replaces the
    /// v0.5 `cardinality = 80` contract; floor lowered from
    /// 10 in v0.13.2). The CLI's `--sketches-per-cell` flag
    /// is the canonical operator-facing knob; the
    /// `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env var and the
    /// `[discovery_matrix].sketches_per_cell` TOML key are
    /// merge-order fall-backs.
    pub sketches_per_cell: usize,
    /// Optional override of the global parallel cap.
    pub max_parallelism: Option<usize>,
    /// F1: target dimension count (no default — `None` means the
    /// LLM picks freely).
    pub dimensions: Option<usize>,
    /// F1: target facets per dimension (no default — `None` means
    /// the LLM picks asymmetrically per dimension).
    pub facets_per_dimension: Option<usize>,
    /// F1: operator-supplied matrix spec (repetible and
    /// consolidated). When non-empty, the LLM-derive path is
    /// skipped.
    pub matrix_spec: Vec<String>,
    /// F1: force the LLM-derive path even when the operator did
    /// not pass a spec.
    pub llm_derive: bool,
    /// SimHash threshold for clustering (0..=1). Default 0.7.
    pub cluster_threshold: f32,
    /// Output directory for the run. Defaults to MOAGAN_HOME resolution.
    pub out_dir: Option<PathBuf>,
    /// Non-interactive: every checkpoint is a `<skipped:non_interactive>`
    /// marker instead of blocking on stdin. Required for CI / smoke
    /// runs where stdin is not a TTY.
    pub non_interactive: bool,
    /// Enable the cross-run facet cache. When `true`, the
    /// `discover_facet` phase writes derived facet lists to
    /// `<MOAGAN_HOME>/cache/facets/` and skips the
    /// `facet_deriver` LLM call on subsequent runs that share
    /// the same `(brief, category_id)` (V4 §6.8 + catalog
    /// D.13.13). Default `false` so the LLM-every-run baseline
    /// is preserved unless the operator opts in via the
    /// `--cache-facets` CLI flag.
    pub cache_facets: bool,
    /// PR-D1: per-provider sampling-temperature profiles sourced
    /// from the `--temperature-profile` CLI flag (last-wins per
    /// provider model) merged with the persisted `[discovery]`
    /// block from `~/.config/moagan/config.toml`. The CLI specs
    /// win on conflict — the operator's explicit invocation
    /// beats the persisted default. When this list is empty AND
    /// the persisted `[discovery]` block is empty, the matrix
    /// uses the default `[1.0] × 1` profile (the v0.5 single-shot
    /// contract).
    pub temperature_profiles: Vec<TemperatureProfileSpec>,
    /// F3 (Track G.2): `--explain` flag from the CLI. The
    /// dispatcher reads this to short-circuit before the
    /// pipeline starts; the field is kept on the options struct
    /// so `discover_explain::build_and_format` can be called
    /// from a single helper with the full picture. Default
    /// `false` so existing call sites stay unchanged.
    pub explain: bool,
}

/// Parsed CLI form of a per-provider temperature profile (PR-D1).
///
/// The clap `Vec<String>` for `--temperature-profile` is parsed
/// into this typed form once at the dispatcher boundary so the
/// downstream matrix / coordinator code consumes validated,
/// type-safe values. The spec grammar is
/// `provider=<model>;temperatures=<csv>;replicas=<n>`:
///
/// * `provider=<model>` — REQUIRED. Provider MODEL name (e.g.
///   `MiniMax-M3`, `mimo-v2.5`). Must be non-empty.
/// * `temperatures=<csv>` — REQUIRED. Comma-separated floats in
///   `0.0..=2.0`. At least one value required.
/// * `replicas=<n>` — REQUIRED. Integer `>= 1`.
///
/// Multiple `--temperature-profile` flags for the same provider
/// are allowed; the LAST spec wins (documented behaviour so the
/// audit can pin the merge order).
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureProfileSpec {
    /// Provider MODEL name (the lookup key the matrix uses; case
    /// sensitive).
    pub provider: String,
    /// Sampling temperatures the loop iterates per `(cell,
    /// replica)` pair. Always non-empty (the parser enforces it).
    pub temperatures: Vec<f32>,
    /// Replicas per `(cell, temperature)` pair. Always `>= 1`.
    pub replicas_per_temperature: usize,
}

impl TemperatureProfileSpec {
    /// Parse a single CLI spec into the typed form. Returns a
    /// [`crate::error::Error::InvalidArgs`] error on every malformed
    /// input (missing `provider=`, out-of-range temperature, etc.)
    /// so the dispatcher surfaces the message through the same
    /// channel as the other CLI validators (D.15.5 pattern).
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        debug!(spec = s, "TemperatureProfileSpec::parse: enter");
        let mut provider: Option<String> = None;
        let mut temperatures: Option<Vec<f32>> = None;
        let mut replicas: Option<usize> = None;
        for kv in s.split(';') {
            let kv = kv.trim();
            if kv.is_empty() {
                warn!(spec = s, "empty segment in temperature-profile spec");
                return Err(crate::error::Error::InvalidArgs(format!(
                    "empty `key=value` segment in temperature-profile spec {s:?}"
                )));
            }
            let (k, v) = kv.split_once('=').ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "expected `key=value` in temperature-profile spec segment {kv:?} \
                     (full spec: {s:?}); grammar is \
                     `provider=<name>;temperatures=<csv>;replicas=<n>`"
                ))
            })?;
            let key = k.trim();
            let value = v.trim();
            match key {
                "provider" => {
                    if value.is_empty() {
                        return Err(crate::error::Error::InvalidArgs(format!(
                            "provider name is empty in temperature-profile spec {s:?}"
                        )));
                    }
                    provider = Some(value.to_owned());
                }
                "temperatures" => {
                    let parsed = value
                        .split(',')
                        .map(|t| t.trim())
                        .filter(|t| !t.is_empty())
                        .map(|t| {
                            t.parse::<f32>().map_err(|e| {
                                crate::error::Error::InvalidArgs(format!(
                                    "invalid temperature {t:?} in temperature-profile \
                                     spec {s:?}: {e}"
                                ))
                            })
                        })
                        .collect::<crate::error::Result<Vec<f32>>>()?;
                    if parsed.is_empty() {
                        return Err(crate::error::Error::InvalidArgs(format!(
                            "temperatures list is empty in temperature-profile spec {s:?}"
                        )));
                    }
                    for t in &parsed {
                        if !(*t >= 0.0 && *t <= 2.0) {
                            return Err(crate::error::Error::InvalidArgs(format!(
                                "temperature {t} out of range 0.0..=2.0 in \
                                 temperature-profile spec {s:?}"
                            )));
                        }
                    }
                    temperatures = Some(parsed);
                }
                "replicas" => {
                    let parsed = value.parse::<usize>().map_err(|e| {
                        crate::error::Error::InvalidArgs(format!(
                            "invalid replicas {value:?} in temperature-profile spec {s:?}: {e}"
                        ))
                    })?;
                    if parsed == 0 {
                        return Err(crate::error::Error::InvalidArgs(format!(
                            "replicas must be >= 1 in temperature-profile spec {s:?}; got 0"
                        )));
                    }
                    replicas = Some(parsed);
                }
                other => {
                    return Err(crate::error::Error::InvalidArgs(format!(
                        "unknown key {other:?} in temperature-profile spec {s:?}; \
                         expected `provider`, `temperatures`, or `replicas`"
                    )));
                }
            }
        }
        let out = Self {
            provider: provider.ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "missing `provider=<name>` in temperature-profile spec {s:?}"
                ))
            })?,
            temperatures: temperatures.ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "missing `temperatures=<csv>` in temperature-profile spec {s:?}"
                ))
            })?,
            replicas_per_temperature: replicas.ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "missing `replicas=<n>` in temperature-profile spec {s:?}"
                ))
            })?,
        };
        trace!(
            provider = %out.provider,
            temperatures = out.temperatures.len(),
            replicas = out.replicas_per_temperature,
            "TemperatureProfileSpec::parse: ok"
        );
        Ok(out)
    }

    /// Convert into the matrix's `TemperatureProfile` (the form
    /// stored on `ExplorationMatrix`). Drops the `provider`
    /// string because the matrix indexes the profile map by
    /// provider model name, and the matrix owns that key.
    pub fn into_matrix_profile(self) -> crate::discovery::matrix::TemperatureProfile {
        crate::discovery::matrix::TemperatureProfile {
            temperatures: self.temperatures,
            replicas_per_temperature: self.replicas_per_temperature,
        }
    }
}

/// Run discovery end-to-end. Returns the run id on success.
///
/// `run_id` is the canonical id for THIS run. The caller
/// (`cli::dispatch_with_run_id`) pre-allocates it so the
/// `pipeline_span` in `run_with_cli` carries it from the very
/// first event emitted during dispatch — every coordinator /
/// discovery_iteration / probe / sketch inherits the id on
/// stderr, and the value matches the `Event::RunStart.run_id`
/// emitted on stdout after dispatch returns.
pub async fn run(opts: DiscoverOptions, cfg: &Config, run_id: RunId) -> Result<RunId> {
    debug!(
        provider = %opts.provider,
        sketches_per_cell = opts.sketches_per_cell,
        cluster_threshold = opts.cluster_threshold,
        non_interactive = opts.non_interactive,
        run_id = %run_id,
        "discover::run: enter"
    );
    let home = Arc::new(match opts.home.clone() {
        Some(path) => MoaganHome::at(path),
        None => MoaganHome::resolve()?,
    });
    home.ensure()?;
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    info!(run_id = %run_id, "discover: allocated run directory");

    let default_provider = if opts.provider.is_empty() {
        cfg.default_provider.clone()
    } else {
        opts.provider.clone()
    };
    // Same as `run_full_pipeline`: the section name (no `SECTION:MODEL`
    // suffix) is what `provider_registry_key(section, model)` joins on.
    // Extract it once so the `RunContext` and the registry agree on
    // the same `(section, model_id)` pair.
    let default_provider_section = crate::cli::probe::parse_provider_model(&default_provider)
        .map(|(s, _)| s)
        .unwrap_or_else(|_| default_provider.clone());
    // PR-x23: thread `active_pairs = Some(&[(section, model)])`
    // through the registry build so the max_tokens probe fans out
    // only for the `(provider, model)` pair the operator asked
    // for. The pre-v0.10 `build_registry_for` shim drops
    // `active_pairs` on the floor (it threads `None` into
    // `build_registry_for_with_active`), which forces a
    // multi-model `minimax` section to probe every model in
    // parallel and races 8 sequential Phase-1 walks against the
    // pipeline's first LLM call. The `build_registry_for_with_active`
    // variant honours the filter.
    let active_pairs: Vec<(String, String)> = vec![(
        default_provider_section.clone(),
        if default_provider.contains(':') {
            crate::cli::probe::parse_provider_model(&default_provider)
                .map(|(_, m)| m)
                .unwrap_or_default()
        } else {
            default_provider.clone()
        },
    )];
    let providers = Arc::new(super::run::build_registry_for_with_active(
        cfg,
        &default_provider,
        opts.mock_dir.as_deref(),
        None,
        Some(&active_pairs),
    )?);
    debug!(
        providers = providers.len(),
        "discover: provider registry built"
    );
    // PR-x23: pull the auto-probe tables off the registry so the
    // `RunContext` (and the pre-pipeline `await_ready` gate below)
    // sees the same handles the registry fired.
    let max_tokens_table = providers.max_tokens_table().cloned();
    let temperature_table = providers.temperature_table().cloned();
    let param_rejections = providers.param_rejections().cloned();
    let default_model = if default_provider.contains(':') {
        let m = crate::cli::probe::parse_provider_model(&default_provider)
            .map(|(_, m)| m)
            .unwrap_or_default();
        trace!(model = %m, "discover: default model parsed (registry)");
        m
    } else {
        // Bare SECTION is no longer accepted in v0.10+ (no
        // implicit "first model" fallback). Surface the error
        // early so the operator sees it before the rest of the
        // pipeline boots.
        warn!(
            provider = %default_provider,
            "discover: bare SECTION without model (registry)"
        );
        return Err(Error::InvalidArgs(format!(
            "--provider '{default_provider}' is a bare section name; \
             pass the explicit SECTION:MODEL form (e.g. \
             --provider {default_provider}:MODEL_ID). No implicit \
             'first model' fallback in v0.10+."
        )));
    };

    let policy = RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(
        run_id,
        "discover",
        "running",
        env!("CARGO_PKG_VERSION"),
        None,
        None,
        None,
    )?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    // PR-B1: `--max-parallelism` is validated up-front (PR #543
    // lifted the cap from 64 to u32::MAX simultaneous LLM calls to
    // honour the operator's choice). We surface
    // `flags_batch::validate_max_parallelism`'s exact error message
    // so the operator sees a consistent rejection across commands.
    if let Some(n) = opts.max_parallelism {
        flags_batch::validate_max_parallelism(n).map_err(Error::InvalidArgs)?;
    }
    let resolved_parallelism = opts.max_parallelism.unwrap_or(cfg.max_parallelism);
    debug!(resolved_parallelism, "discover: parallelism resolved");
    // Wire the per-provider `RateLimiter` from the resolved
    // `--max-parallelism` so `parallelism=32` actually produces
    // 32 in flight rather than being throttled at the hardcoded
    // `refill_per_sec = 4` default. Per-provider overrides
    // (`MOAGAN_RATE_LIMIT_<provider>` or
    // `[rate_limit_per_provider]` in `~/.config/moagan/config.toml`)
    // beat the derived default on conflict (catalog §D.19.6).
    let effective_rate_limit = crate::config::RateLimitConfig {
        capacity: resolved_parallelism as u32,
        // PR-2 (perf/discovery-parallelism): the discovery loop is
        // now actually parallel (see `coordinator::run_with_ctx_and_target`,
        // `join_set.spawn`). The previous `parallelism / 4` default
        // was calibrated for the old sequential loop where the
        // bottleneck was a single concurrent call — it silently
        // throttled dispatcher throughput to 1/4 of the configured
        // parallelism. With the parallel loop, the rate limiter
        // and the semaphore have the same knob — both limit
        // concurrent in-flight calls — so the default matches
        // 1:1. Operators who want a lower rate than the parallelism
        // cap can override with `MOAGAN_RATE_LIMIT_<provider>` (the
        // `attach_parallelism_rate_limit` call below applies that
        // override whenever the per-provider config is set).
        refill_per_sec: resolved_parallelism.max(1) as u32,
        initial: None,
    };
    crate::llm::provider::attach_parallelism_rate_limit(
        providers.as_ref(),
        Some(&effective_rate_limit),
        &cfg.rate_limit_per_provider,
    );
    let parallelism = Parallelism::new(resolved_parallelism);

    // PR-D1: merge CLI `--temperature-profile` specs (last-wins per
    // provider) on top of the persisted `[discovery]` block from
    // `~/.config/moagan/config.toml`. The CLI flag always wins
    // because the operator is explicitly overriding the persisted
    // default for this run; the persisted block is the fall-back
    // when no CLI flag was supplied. We clone `cfg` (so the
    // caller's `&Config` stays borrowable downstream), apply the
    // merge, and feed the resulting `Arc<Config>` into
    // `RunContext::new_with_config` so the coordinator reads the
    // merged profiles from `ctx.config.discovery_matrix`.
    let mut effective_cfg = cfg.clone();
    debug!(
        temperature_profiles = opts.temperature_profiles.len(),
        "discover: merging temperature profiles"
    );
    for spec in opts.temperature_profiles.iter() {
        let model = spec.provider.clone();
        let profile = spec.clone().into_matrix_profile();
        trace!(
            provider = %model,
            temperatures = profile.temperatures.len(),
            replicas = profile.replicas_per_temperature,
            "discover: applied temperature profile"
        );
        effective_cfg
            .discovery_matrix
            .temperature_profiles
            .insert(model, profile);
        // Keep `effective_cfg.discovery_matrix.default_profile`
        // (sourced from the persisted `[discovery]` block, falling
        // back to `None` so the matrix uses its built-in
        // `TemperatureProfile::default()`) as the source of truth.
        // We deliberately do NOT honour a CLI flag named
        // `--default-temperature-profile` to keep the surface small
        // (the audit said "no magic switch") so the persisted
        // block wins.
    }

    // F1 bridge: lift the CLI matrix knobs (`--matrix-spec` /
    // `--dimensions` / `--facets-per-dimension` / `--llm-derive`)
    // into `effective_cfg.discovery_matrix` so the coordinator
    // (`src/discovery/coordinator.rs::build_coordinator_matrix`)
    // and any downstream reader see the operator's CLI choice
    // instead of the persisted `[discovery]` block alone. CLI
    // always wins (matches the precedence the F1 subagent brief
    // documented for `--temperature-profile`).
    if !opts.matrix_spec.is_empty() {
        effective_cfg.discovery_matrix.matrix_spec = opts.matrix_spec.clone();
    }
    if let Some(d) = opts.dimensions {
        effective_cfg.discovery_matrix.dimensions = Some(d);
    }
    if let Some(f) = opts.facets_per_dimension {
        effective_cfg.discovery_matrix.facets_per_dimension = Some(f);
    }
    if opts.llm_derive {
        effective_cfg.discovery_matrix.llm_derive_first = true;
    }
    // F2 (Track G.2): the CLI's `--sketches-per-cell` flag is the
    // canonical source-of-truth for the matrix fan-out. The TOML
    // `[discovery_matrix].sketches_per_cell` block is the fall-back
    // when no flag was supplied. We do NOT honour a CLI flag named
    // `--default-sketches-per-cell` to keep the surface small (the
    // F1 audit said "no magic switch") so the persisted block
    // wins on conflict (matching the `--temperature-profile` merge
    // order). The `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env var
    // was applied in `Config::apply_env_overrides` BEFORE the CLI
    // value overwrites it here, so the precedence chain is
    // CLI > env > TOML > default (10).
    effective_cfg.discovery_matrix.sketches_per_cell = opts.sketches_per_cell;

    let ctx = RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::clone(&providers),
        default_provider_section.clone(),
        default_model,
        parallelism,
        telemetry.clone(),
        opts.prompt.clone(),
        "discover".to_owned(),
        Arc::new(effective_cfg.clone()),
    )
    .with_timeouts(
        effective_cfg.phase_timeout_secs,
        effective_cfg.total_timeout_secs,
    )
    .with_max_tokens_table_opt(max_tokens_table)
    .with_temperature_table_opt(temperature_table)
    .with_param_rejections_opt(param_rejections)
    .with_interactive(!opts.non_interactive)
    // Per-role rate-limit (catalog §D.19.6): wire each
    // `[rate_limit_per_role]` entry into a `RateLimiter` keyed by
    // the parsed `Role`. Unknown role names are silently skipped
    // so a stale config never aborts the run; the per-role bucket
    // then throttles the chatty roles (e.g. `tagger` in the
    // post-matrix fan-out) without affecting the per-provider
    // bucket the rest of the pipeline uses.
    .with_role_rate_limits({
        let mut rate_limit_per_role: std::collections::HashMap<_, _> =
            std::collections::HashMap::new();
        for (role_name, cfg) in &effective_cfg.rate_limit_per_role {
            if let Ok(role) = role_name.parse::<Role>() {
                rate_limit_per_role.insert(
                    role,
                    std::sync::Arc::new(crate::llm::rate_limiter::RateLimiter::new(cfg.clone())),
                );
            }
        }
        rate_limit_per_role
    })
    // v0.9.6: per-`role` adaptive throttle governors. Each
    // `[throttle_per_role]` entry is a `ThrottleConfig` keyed by
    // `Role`. The default-constructed `GovernorRegistry` returns a
    // default-config governor the first time an unknown role is
    // called, so omitting the entry matches the v0.9.5 default
    // (no adaptive backpressure).
    .with_throttle_governors({
        let mut throttle = crate::llm::governor::GovernorRegistry::new();
        let dp = default_provider.clone();
        for (role_name, cfg) in &effective_cfg.throttle_per_role {
            if let Ok(role) = role_name.parse::<Role>() {
                throttle.with_config_for(
                    &dp,
                    role,
                    crate::llm::governor::ThrottleConfig::from(cfg.clone()),
                );
            }
        }
        throttle
    })
    // v0.9.6: per-`(provider, role)` circuit breakers. Each
    // `[circuit_breaker_per_role]` entry is a `BreakerConfig` keyed
    // by `Role`. The provider is `default_provider` at the
    // call-site so the lookup matches what the
    // `ThrottleGovernor` and the per-`(provider, role)` breaker
    // share.
    .with_breakers_per_role({
        let mut breakers = crate::llm::circuit_breaker::BreakerRegistry::new();
        let dp = default_provider.clone();
        for (role_name, cfg) in &effective_cfg.circuit_breaker_per_role {
            if let Ok(role) = role_name.parse::<Role>() {
                breakers.pre_create(
                    &dp,
                    role,
                    crate::llm::circuit_breaker::BreakerConfig::from(*cfg),
                );
            }
        }
        breakers
    });

    let pipeline = build_pre_matrix_pipeline(&opts, &effective_cfg);
    // PR-x23: gate the first LLM call behind the max_tokens probe.
    // Without this wait, the pipeline's `intake` call fires while
    // the single-model probe (now scoped to the active pair by the
    // discover-path registry build) is still walking Phase 1
    // (19 sequential `2^k` values). The intake call then uses the
    // static `max_tokens` knob (524288) and the upstream returns
    // HTTP 400 before the probe ever gets to write 196608 into the
    // table — a self-inflicted race that masks the auto-probe fix.
    // `await_ready` joins every probe task spawned by the registry
    // build (just the one, in the active-pair case) and is a no-op
    // for registries without a max_tokens table.
    if let Some(table) = ctx.max_tokens_table.as_ref() {
        table.await_ready().await;
    }
    let pipeline_future = pipeline.run(&ctx);
    tokio::pin!(pipeline_future);
    info!(run_id = %run_id, "discover: pre-matrix pipeline started");
    let _outputs = tokio::select! {
        result = &mut pipeline_future => result?,
        _ = tokio::signal::ctrl_c() => {
            warn!(run_id = %run_id, "discover: shutdown signal received");
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };
    debug!(run_id = %run_id, "discover: pre-matrix pipeline done");

    // PR-17: drive the sketch fan-out through the discovery
    // coordinator instead of the flat `DiscoverMatrixPhase`. The
    // coordinator owns its own crash-recovery state machine
    // (`SketchLoopState`) and applies the spec's saturation
    // stop policy via `SaturationTracker`. The cancel token is
    // the same handle the pre-matrix pipeline honoured, so a
    // Ctrl-C during the matrix part still short-circuits the
    // loop cleanly.
    let coordinator = DiscoveryCoordinator::new(
        (*home).clone(),
        run_id,
        ctx.cancel().clone(),
        crate::domain::Brief::default(),
        "deployment-model:serverless".to_owned(),
        crate::cli::Mode::Fast,
    );
    let coordinator_ctx = Arc::new(ctx.clone());
    let coordinator_future =
        coordinator.run_with_ctx_and_target(coordinator_ctx.clone(), Some(opts.sketches_per_cell));
    tokio::pin!(coordinator_future);
    info!(run_id = %run_id, "discover: matrix coordinator started");
    let outcome: DiscoveryOutcome = tokio::select! {
        result = &mut coordinator_future => result.map_err(|e| match e {
            crate::discovery::coordinator::CoordinatorError::Error(inner) => inner,
        })?,
        _ = tokio::signal::ctrl_c() => {
            warn!(run_id = %run_id, "discover: coordinator shutdown signal");
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };
    tracing::info!(
        sketches_completed = outcome.sketches_completed,
        sketches_failed = outcome.sketches_failed,
        "DiscoveryCoordinator::run_with_ctx finished; running post-matrix pipeline"
    );

    let post_pipeline = build_post_matrix_pipeline(&opts);
    let post_future = post_pipeline.run(&ctx);
    tokio::pin!(post_future);
    info!(run_id = %run_id, "discover: post-matrix pipeline started");
    let _outputs = tokio::select! {
        result = &mut post_future => result?,
        _ = tokio::signal::ctrl_c() => {
            warn!(run_id = %run_id, "discover: post-matrix shutdown signal");
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };

    telemetry.flush()?;
    debug!(run_id = %run_id, "discover: telemetry flushed");
    if let Err(e) = db.update_run_status(run_id, "completed") {
        // PR-04a (E-1): routing flip moves this warning to
        // stdout (the new home for non-ERROR tracing). The
        // pre-flip duplicate `eprintln!` polluted stderr with
        // operator-facing noise and broke the canonical
        // `2> errors.jsonl` pipeline.
        warn!(run_id = %run_id, error = %e, "discover: failed to update run status");
    }
    // PR-04a (A-2): the human-readable discover banner used to
    // print unconditionally. Piping `moagan discover` through a
    // downstream JSON consumer produced a broken half-NDJSON,
    // half-plain-text stream. Gate the print on `stdout` being a
    // TTY; non-interactive consumers see the equivalent
    // `tracing::info!` event in the stdout tracing stream.
    if std::io::stdout().is_terminal() {
        let mut stdout = std::io::stdout().lock();
        let _ = write_discover_banner(
            &mut stdout,
            &run_id.short(),
            &default_provider,
            &run_dir.root().display(),
        );
    } else {
        info!(
            run_id = %run_id,
            provider = %default_provider,
            run_dir = %run_dir.root().display(),
            "discover: completed (banner suppressed because stdout is non-TTY)"
        );
    }
    info!(run_id = %run_id, "discover: completed");
    Ok(run_id)
}

/// Pull the `Discover` subcommand's `sketches_per_cell` value
/// out of an argument list, falling back to the provided
/// default when the flag is absent. Used by `discover_cmd` to
/// validate the input before spawning the run. The F2 floor is
/// `MIN_SKETCHES_PER_CELL = 1` (replaces the v0.5
/// `cardinality >= 80` guard; lowered from 10 in v0.13.2).
/// The legacy `--cardinality` flag is rejected outright so a
/// stale script does not silently fall back to the
/// integer-division shortfall the F1
/// `derive_sketches_per_cell` helper relied on.
#[allow(dead_code)]
pub(crate) fn parse_sketches_per_cell(args: &[String], default: usize) -> Result<usize> {
    let mut value = default;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--sketches-per-cell" || arg.starts_with("--sketches-per-cell=") {
            let raw = if let Some(eq) = arg.strip_prefix("--sketches-per-cell=") {
                eq.to_owned()
            } else {
                // Consume the next arg as the flag's value, then
                // break out of the loop (no further scan needed).
                let next = args
                    .get(i + 1)
                    .ok_or_else(|| Error::InvalidArgs(format!("{} needs a value", arg)))?
                    .clone();
                value = next
                    .trim()
                    .parse()
                    .map_err(|e| Error::InvalidArgs(format!("invalid sketches-per-cell: {e}")))?;
                break;
            };
            value = raw
                .trim()
                .parse()
                .map_err(|e| Error::InvalidArgs(format!("invalid sketches-per-cell: {e}")))?;
            break;
        }
        if arg == "--cardinality" || arg.starts_with("--cardinality=") {
            return Err(Error::InvalidArgs(
                "--cardinality was renamed to --sketches-per-cell in F2; \
                 pass --sketches-per-cell <N> instead (floor 1)"
                    .to_owned(),
            ));
        }
        i += 1;
    }
    if value < MIN_SKETCHES_PER_CELL {
        return Err(Error::InvalidArgs(format!(
            "sketches-per-cell {value} below the minimum of {MIN_SKETCHES_PER_CELL}"
        )));
    }
    Ok(value)
}

// =====================================================================
// v0.5 PR-24: discovery resume
// =====================================================================

/// F2 (Track G.2): default `sketches_per_cell` used by
/// [`run_resume`] when the `<run_dir>/exploration_matrix.json`
/// artefact is missing or malformed. Replaces the v0.5
/// `RESUME_DEFAULT_CARDINALITY = 80` so a fresh resume rebuilds
/// the matrix around the new per-cell floor instead of the
/// legacy total cardinality.
const RESUME_DEFAULT_SKETCHES_PER_CELL: usize = 10;
const RESUME_DEFAULT_CLUSTER_THRESHOLD: f32 = 0.7;

/// Read the discovery matrix `sketches_per_cell` from
/// `<run_dir>/exploration_matrix.json` if present. Falls back
/// to [`RESUME_DEFAULT_SKETCHES_PER_CELL`] when the file is
/// missing or malformed; the coordinator's persisted state
/// (`.discovery_state.json`) takes precedence over both when the
/// matrix size does not match the loop's `completed_sketches`.
///
/// F2 backward-read compat: an `exploration_matrix.json`
/// persisted by a v0.5 (or earlier) run carries the legacy
/// `cardinality` field but no `sketches_per_cell`. When the
/// matrix shape (cells × sketches_per_cell) is recoverable from
/// the legacy total, derive `sketches_per_cell = ceil(cardinality
/// / cells)` so a resume picks up the operator's original
/// fan-out instead of silently dropping to the new floor.
fn resume_sketches_per_cell(home: &MoaganHome, run_id: RunId) -> usize {
    let path = home.run_dir(run_id).root().join("exploration_matrix.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return RESUME_DEFAULT_SKETCHES_PER_CELL;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return RESUME_DEFAULT_SKETCHES_PER_CELL;
    };
    // F2 first choice: explicit `sketches_per_cell` written by
    // F2-aware matrix builds.
    if let Some(n) = value
        .get("sketches_per_cell")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .filter(|n| *n >= MIN_SKETCHES_PER_CELL)
    {
        return n;
    }
    // F2 backward-read: a v0.5 `exploration_matrix.json` carries
    // `cardinality` + `dimensions`. Derive the per-cell fan-out
    // so the resumed matrix matches the original run.
    let legacy_cardinality = value
        .get("cardinality")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .filter(|n| *n > 0);
    let legacy_cells = value
        .get("dimensions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("facets").and_then(|f| f.as_array()))
                .map(|f| f.len())
                .sum::<usize>()
        })
        .filter(|n| *n > 0);
    if let (Some(card), Some(cells)) = (legacy_cardinality, legacy_cells) {
        // Ceil-divide so 80 sketches on 8 cells → 10 per cell.
        // Note: with floor=1 (v0.13.2), the `.max(MIN_SKETCHES_PER_CELL)`
        // is effectively a no-op for any positive `card / cells` ratio
        // — it only matters for malformed legacy sidecars whose
        // cardinality is below `cells` (e.g. `card=4, cells=8 → 1 per
        // cell`); the ceil-divide value is the new authoritative
        // answer in that case.
        return card.div_ceil(cells).max(MIN_SKETCHES_PER_CELL);
    }
    RESUME_DEFAULT_SKETCHES_PER_CELL
}

/// Resume a paused or failed `moagan discover` run.
///
/// v0.5 PR-24 (V4 §6.11, T01-06 §10.2). The dispatch contract:
///
/// - The caller ([`crate::cli::continue_cmd::run_continue`])
///   guarantees `manifest.mode == "discover"` and the kind is
///   [`PipelineKind::Discovery`]. Linear runs do NOT route
///   through this helper; they use
///   [`crate::cli::continue_cmd::resume_pipeline`].
/// - The canonical 10-phase discovery pipeline is rebuilt via
///   [`Pipeline::resume_with_kind`] against
///   [`PipelineKind::Discovery`] so the cutoff index is sourced
///   from the discovery canonical list (not the linear one — that
///   was the bug PR-24 closed: `unknown phase "discover_matrix"`).
/// - The execution strategy mirrors [`run`]: the matrix fan-out
///   is driven by [`DiscoveryCoordinator::run_with_ctx`] when the
///   resume point is at-or-before `discover_matrix`; the
///   post-matrix phases run through the standard
///   [`Pipeline::run`] path with `resume: true` so each phase
///   event in `telemetry/phases.jsonl.gz` carries the
///   `resume: true` flag.
///
/// The `last_phase` argument comes from
/// `Db::last_completed_phase(run_id)` and is the phase name
/// recorded in the SQLite `phases` table. We use it as the cutoff
/// for [`Pipeline::resume_with_kind`]; the helper returns the
/// remaining phases and we translate them into the coordinator +
/// post-matrix execution.
pub async fn run_resume(
    home: &MoaganHome,
    manifest: &Manifest,
    last_phase: &str,
    api_key: Option<&str>,
    non_interactive: bool,
) -> Result<()> {
    if manifest.mode != "discover" {
        return Err(Error::InvalidArgs(format!(
            "continue --kind discovery requires manifest.mode = \"discover\"; \
             got {:?} (use `--kind linear` or omit `--kind` for linear runs)",
            manifest.mode
        )));
    }

    let run_id = manifest.run_id;
    let run_dir = home.run_dir(run_id);

    // Build the canonical discovery pipeline (10 phases) and
    // filter it via `Pipeline::resume_with_kind` so we get the
    // list of phases the resume should run. This is the same list
    // exposed by `Pipeline::canonical_phase_order_for(Discovery)`;
    // using the kind-aware resume means `last_phase == "clarify"`
    // correctly resolves and produces `discover_matrix + ... +
    // discover_summary` instead of the linear `unknown phase`
    // error that motivated PR-24.
    let canonical = build_canonical_for_resume_pipeline(home, manifest);
    let resumed = Pipeline::resume_with_kind(canonical, last_phase, PipelineKind::Discovery)?;
    if resumed.is_empty() {
        // PR-04a (E-1): routed through the tracing subscriber so
        // the operator-facing notice respects --log-format and the
        // v0.12.0 stream routing flip (INFO → stdout, stderr only
        // carries ERRORs). The pre-flip `eprintln!` polluted stderr
        // unconditionally.
        info!(
            run_id = %run_id,
            last_phase = ?last_phase,
            "discover: nothing left to do after phase"
        );
        return Ok(());
    }

    let default_provider = if manifest.provider.is_empty() || manifest.provider == "unknown" {
        Config::load().unwrap_or_default().default_provider.clone()
    } else {
        manifest.provider.clone()
    };
    let cfg = Config::load().unwrap_or_default();

    let home_arc = Arc::new(home.clone());
    let providers = Arc::new(super::run::build_registry_for_with_api_key(
        &cfg,
        &default_provider,
        None,
        api_key,
    )?);
    let default_model = if default_provider.contains(':') {
        crate::cli::probe::parse_provider_model(&default_provider)
            .map(|(_, m)| m)
            .unwrap_or_default()
    } else {
        // Bare SECTION is no longer accepted in v0.10+ (no
        // implicit "first model" fallback). Surface the error
        // early so the operator sees it before the rest of the
        // pipeline boots.
        return Err(Error::InvalidArgs(format!(
            "--provider '{default_provider}' is a bare section name; \
             pass the explicit SECTION:MODEL form (e.g. \
             --provider {default_provider}:MODEL_ID). No implicit \
             'first model' fallback in v0.10+."
        )));
    };
    let policy = RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    let parallelism = Parallelism::new(cfg.max_parallelism);
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home_arc),
        providers,
        default_provider.clone(),
        default_model.clone(),
        parallelism,
        telemetry.clone(),
        String::new(),
        manifest.mode.clone(),
    )
    .with_interactive(!non_interactive);

    // Decide whether the resume should re-run the coordinator
    // (matrix fan-out) or skip directly to the post-matrix
    // pipeline. The rule mirrors the canonical discovery order:
    //   last_phase ∈ {"intake", "clarify"} → re-run matrix.
    //   last_phase == "discover_matrix"    → matrix already done, run post.
    //   last_phase is a discover_* post-matrix phase → filter post.
    let needs_matrix = matches!(last_phase, "intake" | "clarify");

    if needs_matrix {
        // Wrap the coordinator call with phase events so
        // `telemetry/phases.jsonl.gz` records `discover_matrix` as
        // a real phase (start + end) on the resumed run, mirroring
        // the original run's behaviour. `resume: true` flows
        // through `Pipeline::run`'s marker so every event from
        // the resumed pipeline carries the flag.
        ctx.telemetry
            .phase("discover_matrix", 0, "start", None, true)?;
        let coordinator = DiscoveryCoordinator::new(
            (*home_arc).clone(),
            run_id,
            ctx.cancel().clone(),
            crate::domain::Brief::default(),
            "deployment-model:serverless".to_owned(),
            crate::cli::Mode::Fast,
        );
        let coordinator_ctx = Arc::new(ctx.clone());
        let target = resume_sketches_per_cell(home_arc.as_ref(), run_id);
        let outcome = match tokio::select! {
            result = coordinator.run_with_ctx_and_target(coordinator_ctx.clone(), Some(target)) => result,
            _ = tokio::signal::ctrl_c() => {
                ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
                return Err(ctx.cancel().into_error());
            }
        } {
            Ok(o) => o,
            Err(crate::discovery::coordinator::CoordinatorError::Error(inner)) => {
                ctx.telemetry.phase(
                    "discover_matrix",
                    0,
                    "error",
                    Some(&inner.to_string()),
                    true,
                )?;
                return Err(inner);
            }
        };
        ctx.telemetry
            .phase("discover_matrix", 0, "end", None, true)?;
        tracing::info!(
            sketches_completed = outcome.sketches_completed,
            sketches_failed = outcome.sketches_failed,
            "discovery resume: coordinator finished; running post-matrix pipeline"
        );
    } else {
        // The matrix fan-out is already complete; emit a no-op
        // marker so the resumed pipeline's `discover_matrix` is
        // distinguishable from the original run's. We log
        // "skipped" rather than touching the SQLite `phases` table
        // — the resume is allowed to skip already-completed phases
        // without poisoning the timeline.
        tracing::info!(
            last_phase,
            "discovery resume: skipping matrix fan-out (already complete)"
        );
    }

    // Run the post-matrix pipeline end-to-end from this point on.
    // The matrix completion (or skip) above guarantees the input
    // artefacts the post-matrix phases expect are present.
    let post_opts = DiscoverOptions {
        provider: default_provider.clone(),
        prompt: String::new(),
        home: Some(home_arc.root().to_path_buf()),
        mock_dir: None,
        sketches_per_cell: RESUME_DEFAULT_SKETCHES_PER_CELL,
        max_parallelism: None,
        dimensions: None,
        facets_per_dimension: None,
        matrix_spec: Vec::new(),
        llm_derive: false,
        cluster_threshold: RESUME_DEFAULT_CLUSTER_THRESHOLD,
        out_dir: None,
        non_interactive,
        cache_facets: false,
        temperature_profiles: Vec::new(),
        explain: false,
    };
    let post_pipeline = build_post_matrix_pipeline(&post_opts);

    // Translate the discovery `last_phase` into the equivalent
    // post-matrix cutoff so the filter skips phases that were
    // already completed in the original run.
    let post_filter_from = match last_phase {
        "intake" | "clarify" | "discover_matrix" => "discover_tag",
        other => other,
    };
    let post_resumed = Pipeline::resume(post_pipeline, post_filter_from)?;

    let post_future = post_resumed.run(&ctx);
    tokio::pin!(post_future);
    let _outputs = tokio::select! {
        result = &mut post_future => result?,
        _ = tokio::signal::ctrl_c() => {
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    };

    telemetry.flush()?;
    if let Err(e) = db.update_run_status(run_id, "completed") {
        // PR-04a (E-1): routing flip — same rationale as the
        // matching call site above; the duplicate `eprintln!` is
        // gone so the warning follows the stdout stream.
        warn!(
            run_id = %run_id,
            error = %e,
            "discover: failed to update run status (resume)"
        );
    }
    // PR-04b-1 (A-2): the human-readable resume banner used to
    // print unconditionally. The sibling gate at L886 (PR-04a)
    // only covered the discover entry point; this branch covers
    // the resume entry point so a `moagan continue --kind discovery`
    // piped into `jq` does not produce a broken half-NDJSON,
    // half-plain-text stream. Gate the print on `stdout` being a
    // TTY; non-interactive consumers see the equivalent
    // `tracing::info!` event in the stdout tracing stream.
    if std::io::stdout().is_terminal() {
        let mut stdout = std::io::stdout().lock();
        let _ = write_resume_banner(&mut stdout, &run_id.short(), last_phase);
    } else {
        info!(
            run_id = %run_id,
            last_phase = ?last_phase,
            "continue discovery: resumed (banner suppressed because stdout is non-TTY)"
        );
    }
    Ok(())
}

/// Build the canonical 10-phase discovery pipeline used as the
/// reference list for [`Pipeline::resume_with_kind`] in
/// [`run_resume`].
///
/// The matrix's `cardinality` field is sourced from
/// `exploration_matrix.json` if present, otherwise the default;
/// this keeps the resumed matrix shape consistent with the
/// original run. The remaining dimensions/threshold knobs fall
/// back to the documented defaults because the canonical
/// pipeline only uses them at the matrix boundary.
///
/// `home` is passed in by the caller (`run_resume`) so the
/// canonical-pipeline probe reads the same `<MOAGAN_HOME>/.runs/`
/// tree the actual resume will. We deliberately do NOT re-resolve
/// here — if `MOAGAN_HOME` and `$HOME` are both unset, that is
/// already an error upstream and we want it to surface as one,
/// not be silently papered over with a synthetic tempdir that
/// would only ever produce the default `sketches_per_cell`.
fn build_canonical_for_resume_pipeline(home: &MoaganHome, manifest: &Manifest) -> Pipeline {
    let target = resume_sketches_per_cell(home, manifest.run_id);
    let opts = DiscoverOptions {
        provider: manifest.provider.clone(),
        prompt: String::new(),
        home: None,
        mock_dir: None,
        sketches_per_cell: target,
        max_parallelism: None,
        dimensions: None,
        facets_per_dimension: None,
        matrix_spec: Vec::new(),
        llm_derive: false,
        cluster_threshold: RESUME_DEFAULT_CLUSTER_THRESHOLD,
        out_dir: None,
        non_interactive: true,
        cache_facets: false,
        temperature_profiles: Vec::new(),
        explain: false,
    };
    build_discovery_pipeline(&opts, &Config::load().unwrap_or_default())
}

/// Re-export of [`load_manifest`] for the dispatcher path; the
/// discovery resume helper reads the manifest exactly the way
/// [`crate::cli::continue_cmd`] does. Kept as a top-level import
/// so the function path stays short for tests.
#[allow(dead_code)]
pub(crate) fn load_manifest_for_resume(home: &MoaganHome, run_id: RunId) -> Result<Manifest> {
    load_manifest(home, run_id)
}

/// Build the human-readable discover banner (the line that prints
/// `moagan discover <id> provider=<name> -> <path>`). Public-ish so
/// unit tests can capture it through `Vec<u8>` without touching the
/// real stdout. The shape is exactly the format string the
/// production print statement at L886 emits; if you change one,
/// change both (and update `write_discover_banner_emits_expected_shape`).
fn write_discover_banner<W: std::io::Write>(
    out: &mut W,
    run_id_short: &str,
    provider: &str,
    run_dir: &dyn std::fmt::Display,
) -> std::io::Result<()> {
    writeln!(
        out,
        "moagan discover {} provider={} -> {}",
        run_id_short, provider, run_dir
    )
}

/// Build the human-readable resume banner (the line that prints
/// `moagan continue --kind discovery <id>: resumed after phase <name>`).
/// Mirrors `write_discover_banner` so the resume entry point can be
/// unit-tested through `Vec<u8>` too.
fn write_resume_banner<W: std::io::Write>(
    out: &mut W,
    run_id_short: &str,
    last_phase: &str,
) -> std::io::Result<()> {
    writeln!(
        out,
        "moagan continue --kind discovery {run_id_short}: resumed after phase {last_phase}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sketches_per_cell_default_when_flag_missing() {
        // Default falls back when no flag was supplied. The F2
        // default is 10; the floor is `MIN_SKETCHES_PER_CELL = 1`
        // (lowered from 10 in v0.13.2). The default here matches
        // the `default_value_t = 10` clap annotation on
        // `--sketches-per-cell`.
        let v = parse_sketches_per_cell(&[], 10).unwrap();
        assert_eq!(v, 10);
    }

    #[test]
    fn parse_sketches_per_cell_reads_explicit_flag() {
        let v = parse_sketches_per_cell(&["--sketches-per-cell".to_string(), "25".to_string()], 10)
            .unwrap();
        assert_eq!(v, 25);
    }

    #[test]
    fn parse_sketches_per_cell_reads_eq_form() {
        let v = parse_sketches_per_cell(&["--sketches-per-cell=40".to_string()], 10).unwrap();
        assert_eq!(v, 40);
    }

    #[test]
    fn parse_sketches_per_cell_rejects_below_minimum() {
        let e = parse_sketches_per_cell(&["--sketches-per-cell".to_string(), "0".to_string()], 10)
            .unwrap_err();
        assert!(
            e.to_string()
                .contains(&format!("below the minimum of {MIN_SKETCHES_PER_CELL}")),
            "error must mention the F2 floor; got {e:?}"
        );
    }

    #[test]
    fn parse_sketches_per_cell_accepts_new_floor() {
        // 1 is the v0.13.2 operator-facing floor; values at or
        // above the floor round-trip unchanged.
        let v = parse_sketches_per_cell(&["--sketches-per-cell".to_string(), "1".to_string()], 10)
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn parse_sketches_per_cell_rejects_legacy_cardinality_flag() {
        // The v0.5 `--cardinality` flag is rejected outright in
        // F2 so a stale script does not silently feed the
        // integer-division shortfall the F1 path used to apply.
        let e = parse_sketches_per_cell(&["--cardinality".to_string(), "80".to_string()], 10)
            .unwrap_err();
        assert!(
            e.to_string().contains("--cardinality was renamed"),
            "error must name the rename; got {e:?}"
        );
    }

    #[test]
    fn parse_sketches_per_cell_rejects_non_numeric() {
        let e =
            parse_sketches_per_cell(&["--sketches-per-cell".to_string(), "abc".to_string()], 10)
                .unwrap_err();
        assert!(
            e.to_string().contains("invalid sketches-per-cell"),
            "error must mention the field name; got {e:?}"
        );
    }

    #[test]
    fn parse_sketches_per_cell_requires_value() {
        let e = parse_sketches_per_cell(&["--sketches-per-cell".to_string()], 10).unwrap_err();
        assert!(e.to_string().contains("needs a value"));
    }

    /// PR-B1 (B1.4) lifted to u32::MAX: `discover` validates
    /// `--max-parallelism` against the same helper as `run`, which
    /// now caps at `u32::MAX` (`4_294_967_295`). One above that
    /// bound is rejected with the documented message; the
    /// hard-cap-of-64 history is preserved in the helper's
    /// `flags_batch::validate_max_parallelism` test (which is
    /// the source of truth for the cap and its message).
    #[test]
    fn max_parallelism_cap_holds_for_discover() {
        // Exactly the cap: accepted.
        assert!(flags_batch::validate_max_parallelism(4_294_967_295).is_ok());
        // One above the cap: rejected with the documented message.
        let err = flags_batch::validate_max_parallelism(4_294_967_296).expect_err("must error");
        assert!(
            err.contains("exceeds maximum 4_294_967_295"),
            "error must mention the cap; got {err:?}"
        );
    }

    // ---- PR-D1: TemperatureProfileSpec parser tests ----

    /// PR-D1: the minimal spec (`provider=...;temperatures=<one>;
    /// replicas=<n>`) parses to the typed form. The operator's
    /// `mimo-v2.5` / `[0.5]` / `2` example from the spec.
    #[test]
    fn parse_temperature_profile_spec_minimal() {
        let spec = TemperatureProfileSpec::parse("provider=foo;temperatures=0.5;replicas=2")
            .expect("minimal spec must parse");
        assert_eq!(spec.provider, "foo");
        assert_eq!(spec.temperatures, vec![0.5]);
        assert_eq!(spec.replicas_per_temperature, 2);
    }

    /// PR-D1: a CSV temperature list parses into the typed
    /// `Vec<f32>`. The audit's canonical `[0.0, 0.3, 0.7, 1.0] ×
    /// 4` example yields 4 temperatures + 4 replicas.
    #[test]
    fn parse_temperature_profile_spec_csv() {
        let spec =
            TemperatureProfileSpec::parse("provider=foo;temperatures=0.0,0.3,0.7,1.0;replicas=4")
                .expect("CSV spec must parse");
        assert_eq!(spec.provider, "foo");
        assert_eq!(spec.temperatures, vec![0.0, 0.3, 0.7, 1.0]);
        assert_eq!(spec.replicas_per_temperature, 4);
    }

    /// PR-D1: a spec missing `provider=` fails cleanly with a
    /// message that names the missing key (so an operator
    /// debugging a typo sees exactly what's wrong).
    #[test]
    fn parse_temperature_profile_spec_rejects_missing_provider() {
        let err = TemperatureProfileSpec::parse("temperatures=0.5;replicas=2")
            .expect_err("missing provider must fail");
        assert!(
            err.to_string().contains("missing `provider=<name>`"),
            "error must name the missing key; got {err:?}"
        );
    }

    /// PR-D1: a temperature outside `0.0..=2.0` fails cleanly.
    /// Pin the band here so a future spec change doesn't
    /// accidentally accept a 5.0 by mistake.
    #[test]
    fn parse_temperature_profile_spec_rejects_out_of_range_temp() {
        let err = TemperatureProfileSpec::parse("provider=foo;temperatures=2.5;replicas=1")
            .expect_err("out-of-range temperature must fail");
        assert!(
            err.to_string().contains("out of range 0.0..=2.0"),
            "error must mention the range; got {err:?}"
        );
    }

    /// PR-D1: `replicas=0` fails cleanly. The audit's contract is
    /// `replicas >= 1`; a zero would silently produce an empty
    /// matrix, which is a footgun.
    #[test]
    fn parse_temperature_profile_spec_rejects_zero_replicas() {
        let err = TemperatureProfileSpec::parse("provider=foo;temperatures=0.5;replicas=0")
            .expect_err("replicas=0 must fail");
        assert!(
            err.to_string().contains("replicas must be >= 1"),
            "error must explain the floor; got {err:?}"
        );
    }

    /// PR-D1: `into_matrix_profile` drops the `provider` key (the
    /// matrix stores the profile under the provider's model name
    /// as the map key, not as a field on the profile itself).
    /// Pin the conversion shape so a future field added to
    /// `TemperatureProfile` doesn't silently leak the provider
    /// string into the matrix.
    #[test]
    fn parse_temperature_profile_spec_into_matrix_profile() {
        let spec =
            TemperatureProfileSpec::parse("provider=minimax-m3;temperatures=0.0,0.7;replicas=3")
                .expect("spec must parse");
        let matrix_profile = spec.into_matrix_profile();
        assert_eq!(matrix_profile.temperatures, vec![0.0, 0.7]);
        assert_eq!(matrix_profile.replicas_per_temperature, 3);
    }

    // ---- F2 (Track G.2): sketches_per_cell resume helper ----

    /// F2: a fresh resume probe (no `exploration_matrix.json`)
    /// falls back to [`RESUME_DEFAULT_SKETCHES_PER_CELL`] = 10. The
    /// v0.5 default was `80`; F2 lowers the floor so a fresh
    /// install does not silently fan out the legacy cardinality.
    #[test]
    fn resume_sketches_per_cell_falls_back_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let run_id = RunId::new();
        let v = resume_sketches_per_cell(&home, run_id);
        assert_eq!(v, RESUME_DEFAULT_SKETCHES_PER_CELL);
    }

    /// F2: a malformed `exploration_matrix.json` falls back to the
    /// default rather than panicking — the resume path is
    /// best-effort and the matrix will be rebuilt around the
    /// operator's CLI value at the next phase boundary.
    #[test]
    fn resume_sketches_per_cell_falls_back_on_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run_dir");
        std::fs::write(run_dir.root().join("exploration_matrix.json"), b"{not json").unwrap();
        let v = resume_sketches_per_cell(&home, run_id);
        assert_eq!(v, RESUME_DEFAULT_SKETCHES_PER_CELL);
    }

    /// F2: the F2-aware matrix shape
    /// (`sketches_per_cell: 25`) round-trips through the helper
    /// verbatim. The forward-read path wins over the legacy
    /// `cardinality` field on conflict (a future migration would
    /// have rewritten the file with the new field).
    #[test]
    fn resume_sketches_per_cell_reads_f2_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run_dir");
        let matrix_json = serde_json::json!({
            "cells": 4,
            "sketches_per_cell": 25,
            "cardinality": 1000, // legacy field — must be ignored when F2 field present
            "dimensions": [],
        });
        std::fs::write(
            run_dir.root().join("exploration_matrix.json"),
            serde_json::to_vec(&matrix_json).unwrap(),
        )
        .unwrap();
        let v = resume_sketches_per_cell(&home, run_id);
        assert_eq!(v, 25);
    }

    /// F2 backward-read: a v0.5 `exploration_matrix.json` carries
    /// only the legacy `cardinality` field plus the `dimensions`
    /// array. The helper derives `sketches_per_cell = ceil(card /
    /// cells)` so a resume picks up the operator's original
    /// fan-out (4×2 → 80 sketches → 20 per cell, matching the
    /// v0.5 default).
    #[test]
    fn resume_sketches_per_cell_derives_from_legacy_cardinality() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run_dir");
        // 4 dims × 2 facets = 8 cells; cardinality = 80 → 10 per cell.
        // (The 10 comes from ceil-div `80 / 8`, not from the v0.13.2
        // floor of 1 — this assertion would also hold at floor=10.)
        let matrix_json = serde_json::json!({
            "cells": 8,
            "cardinality": 80,
            "dimensions": [
                {"id": "a", "facets": [{"id": "x"}, {"id": "y"}]},
                {"id": "b", "facets": [{"id": "x"}, {"id": "y"}]},
                {"id": "c", "facets": [{"id": "x"}, {"id": "y"}]},
                {"id": "d", "facets": [{"id": "x"}, {"id": "y"}]},
            ],
        });
        std::fs::write(
            run_dir.root().join("exploration_matrix.json"),
            serde_json::to_vec(&matrix_json).unwrap(),
        )
        .unwrap();
        let v = resume_sketches_per_cell(&home, run_id);
        assert_eq!(v, 10);
    }

    /// F2 backward-read: ceil division. 81 sketches on 8 cells
    /// rounds up to 11 per cell (not 10).
    #[test]
    fn resume_sketches_per_cell_ceil_divides_legacy_cardinality() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run_dir");
        let matrix_json = serde_json::json!({
            "cardinality": 81,
            "dimensions": [
                {"facets": [{"id": "x"}, {"id": "y"}]},
                {"facets": [{"id": "x"}, {"id": "y"}]},
                {"facets": [{"id": "x"}, {"id": "y"}]},
                {"facets": [{"id": "x"}, {"id": "y"}]},
            ],
        });
        std::fs::write(
            run_dir.root().join("exploration_matrix.json"),
            serde_json::to_vec(&matrix_json).unwrap(),
        )
        .unwrap();
        let v = resume_sketches_per_cell(&home, run_id);
        assert_eq!(v, 11);
    }

    // ------------------------------------------------------------
    // PR-04b-1 (A-2): unit tests for the banner helpers. The
    // original `discover_banner_suppressed_when_stdout_is_not_a_tty`
    // integration test was useless (it ran `moagan discover --help`
    // which exits via clap parse before any banner code runs; the
    // assertion was vacuously true). These unit tests capture the
    // banner through `Vec<u8>` and pin the gate via `include_str!`
    // so the contract is locked without touching a real stdout.
    // ------------------------------------------------------------

    /// `write_discover_banner` must produce the exact line the
    /// production print statement emits. If the format string
    /// drifts, this test breaks immediately.
    #[test]
    fn write_discover_banner_emits_expected_shape() {
        let mut buf: Vec<u8> = Vec::new();
        write_discover_banner(
            &mut buf,
            "abc123",
            "minimax",
            &std::path::Path::new("/tmp/run").display(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "moagan discover abc123 provider=minimax -> /tmp/run\n");
    }

    /// `write_resume_banner` mirrors the discover banner for the
    /// `moagan continue --kind discovery` entry point. Pins the
    /// resume contract.
    #[test]
    fn write_resume_banner_emits_expected_shape() {
        let mut buf: Vec<u8> = Vec::new();
        write_resume_banner(&mut buf, "abc123", "rank").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(
            s,
            "moagan continue --kind discovery abc123: resumed after phase rank\n"
        );
    }

    /// Pin the gate: both banner prints are wrapped in
    /// `if std::io::stdout().is_terminal()`. We can't unit-test the
    /// gate directly because `is_terminal()` depends on the OS, so
    /// we read the source to confirm (a) the gate is present at the
    /// discover call site, (b) it precedes the helper call, and
    /// (c) a second gate precedes the resume helper call.
    #[test]
    fn discover_banner_is_gated_by_is_terminal() {
        let src = include_str!("discover.rs");
        // First gate (discover entry point, L886).
        let gate_idx = src
            .find("if std::io::stdout().is_terminal()")
            .expect("the is_terminal gate must exist in discover.rs");
        let helper_call = src
            .find("write_discover_banner(")
            .expect("the helper must be called at least once");
        let resume_helper = src
            .find("write_resume_banner(")
            .expect("the resume helper must be called");
        assert!(
            gate_idx < helper_call,
            "the gate must precede the helper call"
        );
        // Second gate (resume entry point). Search from after the
        // first gate so we don't match the same occurrence.
        let second_gate = src[gate_idx + 1..]
            .find("if std::io::stdout().is_terminal()")
            .map(|i| i + gate_idx + 1);
        let second_gate = second_gate.expect("a second is_terminal gate for resume must exist");
        assert!(second_gate < resume_helper, "resume helper must be gated");
    }
}
