//! `discover_render` — the post-sketch phase of `moagan discover`.
//!
//! Reads what the fan-out left in the run dir, builds the catalogue,
//! checks that every sketch is listed exactly once, and writes the
//! rendered files under `final/`. No LLM call; re-running it on the
//! same run dir rewrites byte-identical files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::atomic::writer::AtomicWriter;
use crate::discovery::catalog::build_catalog;
use crate::discovery::matrix::{
    DISCOVERY_DIMENSIONS_FILENAME, DimensionFacetDescription, DiscoveryDimensions,
    ExplorationMatrix,
};
use crate::discovery::render::{RenderInput, render};
use crate::domain::{Intake, Sketch};
use crate::error::{Error, Result};
use crate::phases::util::{primary_json_paths, read_json};
use crate::phases::{Phase, PhaseOutput, RunContext};

/// Renders the discover catalogue into `final/`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoverRenderPhase;

#[async_trait]
impl Phase for DiscoverRenderPhase {
    fn name(&self) -> &'static str {
        "discover_render"
    }

    /// Render the run dir and write the files under `final/`.
    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let run_dir = ctx.run_dir();
        let files = render_run_dir(run_dir.root())?;
        let readme = write_catalog(&run_dir.final_dir(), &files)?;
        tracing::info!(
            files = files.len(),
            readme = %readme.display(),
            "discover_render: catalogue written"
        );
        Ok(PhaseOutput::Catalog(readme))
    }
}

/// What a run dir holds for the catalogue.
#[derive(Debug, Clone)]
pub struct RunInputs {
    /// `exploration_matrix.json`.
    pub matrix: ExplorationMatrix,
    /// Facet descriptions of `discovery_dimensions.json`; empty when absent.
    pub descriptions: Vec<DimensionFacetDescription>,
    /// `brief.json`; the default (empty) brief when absent.
    pub brief: Intake,
    /// Every primary `sketches/*.json`, in file name order.
    pub sketches: Vec<Sketch>,
}

/// Load `exploration_matrix.json` (required), the facet descriptions
/// of `discovery_dimensions.json` (optional), `brief.json` as an
/// [`Intake`] (missing = empty brief) and every primary
/// `sketches/*.json` (missing dir = no sketches; an empty sketch id
/// takes the file stem). Fails with `Error::InvalidState` when there
/// is no matrix.
pub fn load_inputs(run_dir: &Path) -> Result<RunInputs> {
    let matrix_path = run_dir.join("exploration_matrix.json");
    if !matrix_path.is_file() {
        return Err(Error::InvalidState(format!(
            "no exploration_matrix.json in {}: the sketch fan-out has not run",
            run_dir.display()
        )));
    }
    let matrix: ExplorationMatrix = read_json(&matrix_path)?;
    let dimensions_path = run_dir.join(DISCOVERY_DIMENSIONS_FILENAME);
    let descriptions = if dimensions_path.is_file() {
        read_json::<DiscoveryDimensions>(&dimensions_path)?.descriptions
    } else {
        Vec::new()
    };
    let brief_path = run_dir.join("brief.json");
    let brief: Intake = if brief_path.is_file() {
        read_json(&brief_path)?
    } else {
        Intake::default()
    };
    let sketches = load_sketches(&run_dir.join("sketches"))?;
    Ok(RunInputs {
        matrix,
        descriptions,
        brief,
        sketches,
    })
}

/// Load the run dir (see [`load_inputs`]), build and validate the
/// catalogue, then render it. The run id shown is the run dir's name.
/// Fails with `Error::InvalidState` when there is no matrix, the
/// catalogue breaks invariant I3 or two cells render to the same file.
pub fn render_run_dir(run_dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let RunInputs {
        matrix,
        descriptions,
        brief,
        sketches,
    } = load_inputs(run_dir)?;

    let catalog = build_catalog(&matrix, &descriptions, &sketches);
    catalog
        .validate(&sketches)
        .map_err(|e| Error::InvalidState(format!("{}: {e}", run_dir.display())))?;

    let run_id = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = render(&RenderInput {
        run_id: &run_id,
        problem: &brief.problem,
        constraints: &brief.constraints,
        matrix: &matrix,
        catalog: &catalog,
        sketches: &sketches,
    });

    let mut seen = BTreeSet::new();
    for (path, _) in &files {
        if !seen.insert(path.as_path()) {
            return Err(Error::InvalidState(format!(
                "two catalogue files render to {}: matrix ids collide",
                path.display()
            )));
        }
    }
    Ok(files)
}

/// Write each rendered file under `final_dir` atomically, creating
/// subdirectories as needed. Returns the path of `README.md`.
pub fn write_catalog(final_dir: &Path, files: &[(PathBuf, String)]) -> Result<PathBuf> {
    let writer = AtomicWriter::new();
    for (path, text) in files {
        writer.write(&final_dir.join(path), text.as_bytes())?;
    }
    Ok(final_dir.join("README.md"))
}

/// Primary sketches of `dir` (no `.meta.json` seals); a missing dir is
/// an empty run and an empty `id` takes the file stem.
fn load_sketches(dir: &Path) -> Result<Vec<Sketch>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut sketches = Vec::new();
    for path in primary_json_paths(dir)? {
        let mut sketch: Sketch = read_json(&path)?;
        if sketch.id.is_empty() {
            sketch.id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        sketches.push(sketch);
    }
    Ok(sketches)
}

#[cfg(test)]
mod tests;
