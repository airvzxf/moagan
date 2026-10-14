//! User payload of one discover thesis call.
//!
//! Every thesis call carries the operator's verbatim prompt, the
//! brief's constraints under stable ids (`C1..Cn`), the facet
//! description of its cell and that cell's exclusions
//! (`--cell-exclude`). The run-constant part goes first so
//! provider-side prefix caching can reuse it across the fan-out.

use std::path::Path;

use crate::discovery::cell_exclude::CellExclusion;
use crate::discovery::matrix::{
    DISCOVERY_DIMENSIONS_FILENAME, DimensionFacetDescription, DiscoveryDimensions,
    ExplorationMatrix, MatrixCell,
};
use crate::discovery::run_spec::{DISCOVER_RUN_FILENAME, DiscoverRunSpec};
use crate::domain::Intake;
use crate::error::{Error, Result};
use crate::phases::util::read_json;

/// Run-level inputs shared by every thesis call. Loaded once per run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThesisPromptContext {
    /// Operator prompt as intake normalised it.
    pub operator_prompt: String,
    /// Brief constraints in order; entry `i` is constraint `C{i+1}`.
    pub constraints: Vec<String>,
    /// Facet descriptions derived by `discover_dimensions`.
    pub descriptions: Vec<DimensionFacetDescription>,
    /// Per-cell exclusions of the run spec, in spec order.
    pub exclusions: Vec<CellExclusion>,
}

impl ThesisPromptContext {
    /// Read the inputs from `run_dir`: `prompt.md` (or
    /// `brief.json#raw_prompt` when `prompt.md` is missing or blank),
    /// `brief.json#constraints`, the descriptions in
    /// `discovery_dimensions.json` (absent for `--matrix-spec` runs)
    /// and the `cell_excludes` of `discover_run.json` (none when the
    /// file is absent). An exclusion that names no cell of
    /// `exploration_matrix.json` is logged as a warning.
    /// Fails when neither source yields a non-blank prompt.
    pub fn load(run_dir: &Path) -> Result<Self> {
        let brief: Intake = read_json(&run_dir.join("brief.json"))?;
        let prompt_md = std::fs::read_to_string(run_dir.join("prompt.md")).unwrap_or_default();
        let operator_prompt = if prompt_md.trim().is_empty() {
            brief.raw_prompt
        } else {
            prompt_md
        };
        if operator_prompt.trim().is_empty() {
            return Err(Error::InvalidState(format!(
                "no operator prompt in {}: prompt.md and brief.json#raw_prompt are both empty",
                run_dir.display()
            )));
        }
        let dimensions_path = run_dir.join(DISCOVERY_DIMENSIONS_FILENAME);
        let descriptions = if dimensions_path.exists() {
            read_json::<DiscoveryDimensions>(&dimensions_path)?.descriptions
        } else {
            Vec::new()
        };
        let spec_path = run_dir.join(DISCOVER_RUN_FILENAME);
        let exclusions = if spec_path.is_file() {
            read_json::<DiscoverRunSpec>(&spec_path)?.cell_excludes
        } else {
            Vec::new()
        };
        warn_unmatched(run_dir, &exclusions)?;
        Ok(Self {
            operator_prompt,
            constraints: brief.constraints,
            descriptions,
            exclusions,
        })
    }

    /// The exclusion lines of `cell`'s payload: one
    /// `Do NOT propose in this cell: <text>` line per exclusion that
    /// applies to `cell`, in spec order; `""` when none applies.
    pub fn exclusion_lines(&self, cell: &MatrixCell) -> String {
        self.exclusions
            .iter()
            .filter(|exclusion| exclusion.applies_to(cell))
            .map(|exclusion| format!("Do NOT propose in this cell: {}\n", exclusion.text))
            .collect()
    }

    /// Description of `cell`'s facet, or `""` when none was derived.
    pub fn description_for(&self, cell: &MatrixCell) -> &str {
        self.descriptions
            .iter()
            .find(|d| d.dimension_id == cell.dimension_id && d.facet_id == cell.facet_id)
            .map_or("", |d| d.description.as_str())
    }

    /// Payload for fan-out iteration `index` on `cell`. `index` must be
    /// unique per iteration: identical payloads share one cache entry.
    pub fn user_payload(&self, cell: &MatrixCell, index: usize) -> String {
        let mut out = format!(
            "<operator_prompt>\n{}\n</operator_prompt>\n\n",
            self.operator_prompt
        );
        if self.constraints.is_empty() {
            out.push_str("Hard constraints: none (answer hard_constraint_check with {}).\n");
        } else {
            out.push_str(
                "Hard constraints (answer hard_constraint_check with exactly these keys):\n",
            );
            for (i, constraint) in self.constraints.iter().enumerate() {
                out.push_str(&format!("C{}: {constraint}\n", i + 1));
            }
        }
        out.push_str(&format!(
            "\nExploration cell: {} ({}:{})\n",
            cell.label, cell.dimension_id, cell.facet_id
        ));
        let description = self.description_for(cell);
        if !description.is_empty() {
            out.push_str(description);
            out.push('\n');
        }
        out.push_str(&self.exclusion_lines(cell));
        out.push_str(&format!(
            "\nProduce idea #{index} for this cell. \
             Write in the language of the operator prompt."
        ));
        out
    }
}

/// Warn about every exclusion that names no cell of the run's
/// `exploration_matrix.json`; nothing to check when it is absent.
fn warn_unmatched(run_dir: &Path, exclusions: &[CellExclusion]) -> Result<()> {
    let matrix_path = run_dir.join("exploration_matrix.json");
    if exclusions.is_empty() || !matrix_path.is_file() {
        return Ok(());
    }
    let matrix: ExplorationMatrix = read_json(&matrix_path)?;
    for exclusion in exclusions {
        if !matrix.iter_cells().any(|cell| exclusion.applies_to(&cell)) {
            tracing::warn!(
                dimension_id = %exclusion.dimension_id,
                facet_id = %exclusion.facet_id,
                "--cell-exclude names no cell of the matrix; it has no effect"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
