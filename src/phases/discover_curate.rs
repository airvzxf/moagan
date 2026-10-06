//! `discover_curate` — one curator call per cell of a discover run.
//!
//! Runs after the sketch fan-out and before `discover_render`. For
//! each matrix cell with at least [`MIN_THESES_TO_CURATE`] theses it
//! asks [`Role::Curator`] to group them (chunks of at most
//! [`MAX_THESES_PER_CALL`]), checks the answer in Rust, and writes
//! `curation/<dimension>__<facet>.json`. A cell is skipped while its
//! `Ok` curation still covers its theses, so re-running the phase only
//! redoes cells that are new, changed or failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tracing::Instrument;

use crate::discovery::catalog::{CellEntry, OUTSIDE_DIMENSION_ID, build_catalog};
use crate::discovery::curation::{
    CURATION_DIR, ChunkCuration, Curation, CurationStatus, MAX_THESES_PER_CALL,
    MIN_THESES_TO_CURATE, RawCuration, cell_key, cell_member_ids, chunk_ranges, curation_path,
    curator_payload, normalise,
};
use crate::domain::Sketch;
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::discover_render::load_inputs;
use crate::phases::util::{primary_json_paths, read_json, write_json};
use crate::phases::{Phase, PhaseOutput, RunContext};

/// Curates the cells of a discover run; see the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoverCuratePhase;

/// The cells of `cells` that need a curator call, in their order:
/// matrix cells (not the outside-the-matrix cell) with at least
/// [`MIN_THESES_TO_CURATE`] theses whose curation file under
/// `curation_dir` is missing, unreadable, `Failed`, or does not cover
/// the cell's current theses.
pub fn pending_cells(cells: &[CellEntry], curation_dir: &Path) -> Vec<CellEntry> {
    cells
        .iter()
        .filter(|cell| cell.dimension_id != OUTSIDE_DIMENSION_ID)
        .filter(|cell| {
            let ids = cell_member_ids(cell);
            if ids.len() < MIN_THESES_TO_CURATE {
                return false;
            }
            let path = curation_path(curation_dir, &cell.dimension_id, &cell.facet_id);
            let current = path.is_file()
                && read_json::<Curation>(&path).is_ok_and(|c| {
                    c.status == CurationStatus::Ok && c.covers(&cell_key(cell), &ids)
                });
            !current
        })
        .cloned()
        .collect()
}

/// Every curation file of `curation_dir` that parses, in file name
/// order; a missing directory has none. Unreadable files are skipped
/// with a warning.
pub fn load_curations(curation_dir: &Path) -> Result<Vec<Curation>> {
    if !curation_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut curations = Vec::new();
    for path in primary_json_paths(curation_dir)? {
        match read_json::<Curation>(&path) {
            Ok(curation) => curations.push(curation),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "discover_curate: curation file skipped"
            ),
        }
    }
    Ok(curations)
}

#[async_trait]
impl Phase for DiscoverCuratePhase {
    fn name(&self) -> &'static str {
        "discover_curate"
    }

    /// Curate every pending cell under the run's parallelism cap and
    /// write its curation file (`Ok` or `Failed`). Returns the files
    /// written in this run, sorted. Fails only on cancellation or when
    /// the run dir cannot be read; a curator error never fails it.
    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let run_dir = ctx.run_dir().root().to_path_buf();
        let inputs = load_inputs(&run_dir)?;
        let catalog = build_catalog(&inputs.matrix, &inputs.descriptions, &inputs.sketches);
        let curation_dir = run_dir.join(CURATION_DIR);
        let todo = pending_cells(&catalog.cells, &curation_dir);
        tracing::info!(
            cells = catalog.cells.len(),
            pending = todo.len(),
            "discover_curate: cells planned"
        );
        if todo.is_empty() {
            return Ok(PhaseOutput::Curations(Vec::new()));
        }
        std::fs::create_dir_all(&curation_dir)?;

        let constraints = Arc::new(inputs.brief.constraints.clone());
        let sketches = Arc::new(inputs.sketches);
        let shared = Arc::new(ctx.clone());
        let mut join_set: tokio::task::JoinSet<(PathBuf, Curation)> = tokio::task::JoinSet::new();
        for cell in todo {
            if ctx.cancel().is_cancelled() {
                break;
            }
            let span = tracing::trace_span!(
                "curate",
                cell_dim = %cell.dimension_id,
                cell_facet = %cell.facet_id,
            );
            let path = curation_path(&curation_dir, &cell.dimension_id, &cell.facet_id);
            let ctx = Arc::clone(&shared);
            let constraints = Arc::clone(&constraints);
            let sketches = Arc::clone(&sketches);
            join_set.spawn(
                async move {
                    (
                        path,
                        curate_cell(&ctx, &cell, &constraints, &sketches).await,
                    )
                }
                .instrument(span),
            );
        }

        let mut written = Vec::new();
        let mut failed = 0usize;
        while let Some(joined) = join_set.join_next().await {
            let (path, curation) = match joined {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "discover_curate: task aborted");
                    continue;
                }
            };
            if curation.status == CurationStatus::Failed {
                failed += 1;
            }
            match write_json(&path, &curation) {
                Ok(()) => written.push(path),
                Err(e) => tracing::warn!(
                    cell = %curation.cell,
                    error = %e,
                    "discover_curate: curation could not be written"
                ),
            }
        }
        if ctx.cancel().is_cancelled() {
            return Err(ctx.cancel().into_error());
        }
        tracing::info!(
            written = written.len(),
            failed,
            "discover_curate: cells curated"
        );
        written.sort();
        Ok(PhaseOutput::Curations(written))
    }
}

/// Curate one cell: wait for a parallelism permit, then curate each
/// chunk of its sorted theses in order. A cancelled run or a chunk
/// without an acceptable answer yields a `Failed` curation.
async fn curate_cell(
    ctx: &RunContext,
    cell: &CellEntry,
    constraints: &[String],
    sketches: &[Sketch],
) -> Curation {
    let ids = cell_member_ids(cell);
    let key = cell_key(cell);
    let permit = match ctx.parallelism.acquire().await {
        Ok(permit) => permit,
        Err(e) => {
            tracing::warn!(cell = %key, error = %e, "discover_curate: no permit");
            return Curation::merge(&key, ids, vec![None]);
        }
    };
    let mut chunks = Vec::new();
    for range in chunk_ranges(ids.len(), MAX_THESES_PER_CALL) {
        if ctx.cancel().is_cancelled() {
            chunks.push(None);
            break;
        }
        let chunk_ids = &ids[range];
        let theses: Vec<&Sketch> = chunk_ids
            .iter()
            .filter_map(|id| sketches.iter().find(|s| &s.id == id))
            .collect();
        let user = curator_payload(cell, constraints, &theses);
        chunks.push(curate_chunk(ctx, &key, chunk_ids, user).await);
    }
    drop(permit);
    let curation = Curation::merge(&key, ids, chunks);
    tracing::info!(
        cell = %key,
        status = ?curation.status,
        groups = curation.groups.len(),
        "discover_curate: cell done"
    );
    curation
}

/// One chunk: a cached first call and, when it fails or its answer is
/// not acceptable (unparseable, or more than 20 % of the theses
/// unassigned), one uncached retry. `None` when both attempts fail.
async fn curate_chunk(
    ctx: &RunContext,
    key: &str,
    ids: &[String],
    user: String,
) -> Option<ChunkCuration> {
    let system = system_prompt(Role::Curator);
    for attempt in 0..2u32 {
        let response = if attempt == 0 {
            ctx.call_with_retry(Role::Curator, system.to_owned(), user.clone(), 0)
                .await
        } else {
            ctx.call_uncached(
                Role::Curator,
                system.to_owned(),
                user.clone(),
                crate::time::now_unix_secs(),
                attempt,
            )
            .await
        };
        let parsed = response
            .and_then(|raw| ctx.parse_model_json::<RawCuration>(Role::Curator, &raw.text, system));
        match parsed {
            Ok(raw) => {
                let chunk = normalise(ids, &raw);
                if chunk.report.is_acceptable() {
                    return Some(chunk);
                }
                tracing::warn!(
                    cell = %key,
                    attempt,
                    theses = chunk.report.theses,
                    unassigned = chunk.report.unassigned,
                    "discover_curate: too many theses unassigned"
                );
            }
            Err(e) => tracing::warn!(
                cell = %key,
                attempt,
                error = %e,
                "discover_curate: curator answer unusable"
            ),
        }
    }
    None
}

#[cfg(test)]
mod tests;
