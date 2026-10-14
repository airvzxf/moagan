//! `moagan discover merge`: pool the theses of finished discover runs
//! into one new run.
//!
//! Repeated runs keep finding new ideas, but each run is curated and
//! rendered on its own. A merge writes a new run dir with the brief and
//! prompt of the first source, the union of the source matrices, and
//! every source thesis renumbered `th_NNNN` with the run and id it came
//! from in its provenance. The merged run then runs only
//! `discover_curate` and `discover_render`, so every cell gets one
//! curation over the pooled theses. The sources must share the brief
//! constraints, because a thesis answers `C1..Cn` by number.

use std::path::{Path, PathBuf};

use crate::discovery::matrix::{
    DISCOVERY_DIMENSIONS_FILENAME, DimensionFacetDescription, DiscoveryDimensions,
    ExplorationMatrix,
};
use crate::domain::Thesis;
use crate::error::{Error, Result};
use crate::phases::discover_render::{RunInputs, load_inputs};
use crate::phases::discover_theses::EXPLORATION_MATRIX_FILENAME;
use crate::phases::util::{read_json, write_json};

/// One finished discover run to merge.
#[derive(Debug, Clone)]
pub struct SourceRun {
    /// Run id: the name of the run dir.
    pub run_id: String,
    /// The run dir.
    pub dir: PathBuf,
    /// What the run dir holds ([`load_inputs`]).
    pub inputs: RunInputs,
}

impl SourceRun {
    /// Load the run dir `dir` ([`load_inputs`]); its name is the run
    /// id. Fails with `Error::InvalidState` when it has no
    /// `exploration_matrix.json` or no `brief.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        if !dir.join("brief.json").is_file() {
            return Err(Error::InvalidState(format!(
                "no brief.json in {}: not a finished discover run",
                dir.display()
            )));
        }
        let inputs = load_inputs(dir)?;
        let run_id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            run_id,
            dir: dir.to_path_buf(),
            inputs,
        })
    }
}

/// `Ok` when every source has the brief constraints of the first one,
/// same texts in the same order. Otherwise `Error::InvalidArgs` naming
/// the first source that differs, the first source, and the first
/// constraint number that differs, and saying to start every run from
/// the same brief (`--brief <run>/brief.json`).
pub fn check_same_constraints(sources: &[SourceRun]) -> Result<()> {
    let Some((first, rest)) = sources.split_first() else {
        return Ok(());
    };
    let want = &first.inputs.brief.constraints;
    for source in rest {
        let have = &source.inputs.brief.constraints;
        if have == want {
            continue;
        }
        let differs_at = want.iter().zip(have).take_while(|(a, b)| a == b).count() + 1;
        return Err(Error::InvalidArgs(format!(
            "run {} does not share the brief constraints of run {} (they differ from C{differs_at}): \
             merged theses answer C1..Cn by number, so start every run from the same brief \
             (moagan discover --brief <run>/brief.json)",
            source.run_id, first.run_id
        )));
    }
    Ok(())
}

/// The matrix of the merged run: a copy of the first source's matrix
/// (its fan-out size and temperature profiles included), then, source
/// by source, every dimension it does not have yet appended, and every
/// new facet of a dimension it has appended to that dimension. Ids
/// decide identity; a known id keeps its first label. `None` when
/// `sources` is empty.
pub fn merge_matrices(sources: &[SourceRun]) -> Option<ExplorationMatrix> {
    let (first, rest) = sources.split_first()?;
    let mut merged = first.inputs.matrix.clone();
    for source in rest {
        for dimension in &source.inputs.matrix.dimensions {
            match merged.dimensions.iter_mut().find(|d| d.id == dimension.id) {
                Some(known) => {
                    for facet in &dimension.facets {
                        if !known.facets.iter().any(|f| f.id == facet.id) {
                            known.facets.push(facet.clone());
                        }
                    }
                }
                None => merged.dimensions.push(dimension.clone()),
            }
        }
    }
    Some(merged)
}

/// The facet descriptions of the merged run: one per cell, from the
/// first source that describes the cell, in source order.
pub fn merge_descriptions(sources: &[SourceRun]) -> Vec<DimensionFacetDescription> {
    let mut merged: Vec<DimensionFacetDescription> = Vec::new();
    for description in sources.iter().flat_map(|s| &s.inputs.descriptions) {
        let known = merged.iter().any(|d| {
            d.dimension_id == description.dimension_id && d.facet_id == description.facet_id
        });
        if !known {
            merged.push(description.clone());
        }
    }
    merged
}

/// Every thesis of every source, sources in order and each source's
/// theses in its file name order, renumbered `th_0000`, `th_0001`, …
/// Each provenance records `source_run` (the source's run id) and
/// `source_thesis` (the id in that run); a thesis that a previous merge
/// already pooled keeps its original source. Fails with
/// `Error::InvalidArgs` naming the thesis and its run when a thesis
/// records no provenance (written before moagan v0.20.0).
pub fn pool_theses(sources: &[SourceRun]) -> Result<Vec<Thesis>> {
    let mut pooled = Vec::new();
    for source in sources {
        for thesis in &source.inputs.theses {
            let mut thesis = thesis.clone();
            let Some(provenance) = thesis.provenance.as_mut() else {
                return Err(Error::InvalidArgs(format!(
                    "thesis {} of run {} records no provenance (written before moagan v0.20.0); \
                     merge needs theses that record where they came from",
                    thesis.id, source.run_id
                )));
            };
            if provenance.source_run.is_none() {
                provenance.source_run = Some(source.run_id.clone());
                provenance.source_thesis = Some(thesis.id.clone());
            }
            thesis.id = format!("th_{:04}", pooled.len());
            pooled.push(thesis);
        }
    }
    Ok(pooled)
}

/// Write the merged run of `sources` into the run dir `dest`:
/// `brief.json` and `prompt.md` (when present) copied from the first
/// source, `exploration_matrix.json` from [`merge_matrices`],
/// `discovery_dimensions.json` when a source has one (the first such
/// sidecar, with the merged dimensions and [`merge_descriptions`]), and
/// the [`pool_theses`] under `theses/`. The constraints and the
/// provenance are checked before anything is written. Returns the
/// number of theses written.
pub fn write_merged_run(sources: &[SourceRun], dest: &Path) -> Result<usize> {
    check_same_constraints(sources)?;
    let theses = pool_theses(sources)?;
    let (Some(first), Some(matrix)) = (sources.first(), merge_matrices(sources)) else {
        return Err(Error::InvalidArgs(
            "discover merge needs source runs".to_owned(),
        ));
    };
    let theses_dir = dest.join("theses");
    std::fs::create_dir_all(&theses_dir)?;
    std::fs::copy(first.dir.join("brief.json"), dest.join("brief.json"))?;
    let prompt = first.dir.join("prompt.md");
    if prompt.is_file() {
        std::fs::copy(&prompt, dest.join("prompt.md"))?;
    }
    let sidecar = sources
        .iter()
        .map(|s| s.dir.join(DISCOVERY_DIMENSIONS_FILENAME))
        .find(|p| p.is_file());
    if let Some(path) = sidecar {
        let mut dimensions: DiscoveryDimensions = read_json(&path)?;
        dimensions.dimensions = matrix.dimensions.clone();
        dimensions.descriptions = merge_descriptions(sources);
        write_json(&dest.join(DISCOVERY_DIMENSIONS_FILENAME), &dimensions)?;
    }
    write_json(&dest.join(EXPLORATION_MATRIX_FILENAME), &matrix)?;
    for thesis in &theses {
        write_json(&theses_dir.join(format!("{}.json", thesis.id)), thesis)?;
    }
    Ok(theses.len())
}

#[cfg(test)]
mod tests;
