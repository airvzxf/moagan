//! User payload of one discover sketch call.
//!
//! Every sketch call carries the operator's verbatim prompt, the
//! brief's constraints under stable ids (`C1..Cn`) and the facet
//! description of its cell. The run-constant part goes first so
//! provider-side prefix caching can reuse it across the fan-out.

use std::path::Path;

use crate::discovery::matrix::{
    DISCOVERY_DIMENSIONS_FILENAME, DimensionFacetDescription, DiscoveryDimensions, MatrixCell,
};
use crate::domain::Intake;
use crate::error::{Error, Result};
use crate::phases::util::read_json;

/// Run-level inputs shared by every sketch call. Loaded once per run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SketchPromptContext {
    /// Operator prompt as intake normalised it.
    pub operator_prompt: String,
    /// Brief constraints in order; entry `i` is constraint `C{i+1}`.
    pub constraints: Vec<String>,
    /// Facet descriptions derived by `discover_dimensions`.
    pub descriptions: Vec<DimensionFacetDescription>,
}

impl SketchPromptContext {
    /// Read the inputs from `run_dir`: `prompt.md` (or
    /// `brief.json#raw_prompt` when `prompt.md` is missing or blank),
    /// `brief.json#constraints`, and the descriptions in
    /// `discovery_dimensions.json` (absent for `--matrix-spec` runs).
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
        Ok(Self {
            operator_prompt,
            constraints: brief.constraints,
            descriptions,
        })
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
        out.push_str(&format!(
            "\nProduce idea #{index} for this cell. \
             Write in the language of the operator prompt."
        ));
        out
    }
}

#[cfg(test)]
mod tests;
