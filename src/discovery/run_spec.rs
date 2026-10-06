//! The operator's choices for one `moagan discover` run.
//!
//! A fresh run writes them to `<run_dir>/discover_run.json` before
//! its first model call; `moagan continue --kind discovery` reads
//! them back so the resumed run is the same pipeline with the same
//! inputs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::DiscoveryMatrixConfig;
use crate::discovery::matrix::{ExplorationMatrix, TemperatureProfile};
use crate::error::{Error, Result};
use crate::phases::util::{read_json, write_json};

/// File name of the persisted spec under the run dir.
pub const DISCOVER_RUN_FILENAME: &str = "discover_run.json";

/// Everything a discover run needs besides the prompt (`prompt.md`)
/// and the provider configuration (`config.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoverRunSpec {
    /// Default provider as `SECTION:MODEL`.
    pub provider: String,
    /// Directory of canned mock answers (`--mock-dir`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mock_dir: Option<PathBuf>,
    /// Cap on simultaneous model calls; `None` = the config default.
    #[serde(default)]
    pub max_parallelism: Option<usize>,
    /// Sketches per matrix cell, per temperature and replica.
    pub sketches_per_cell: usize,
    /// Operator matrix spec entries (`--matrix-spec`); empty = none.
    #[serde(default)]
    pub matrix_spec: Vec<String>,
    /// Target dimension count (`--dimensions`).
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Facets per dimension (`--facets-per-dimension`).
    #[serde(default)]
    pub facets_per_dimension: Option<usize>,
    /// Derive the dimensions with the model even when counts are given.
    #[serde(default)]
    pub llm_derive: bool,
    /// Temperature profiles keyed `section::model`.
    #[serde(default)]
    pub temperature_profiles: BTreeMap<String, TemperatureProfile>,
    /// Profile of a provider pair that has no entry above.
    #[serde(default)]
    pub default_profile: Option<TemperatureProfile>,
}

impl DiscoverRunSpec {
    /// Write the spec to `<run_dir>/discover_run.json`.
    pub fn save(&self, run_dir: &Path) -> Result<()> {
        write_json(&run_dir.join(DISCOVER_RUN_FILENAME), self)
    }

    /// Read `<run_dir>/discover_run.json`. A run written before the
    /// file existed is rebuilt from `exploration_matrix.json`: its
    /// first `section::model` profile key (sorted) becomes the
    /// provider, and its `sketches_per_cell`, profiles and default
    /// profile are kept. Fails with `Error::InvalidState` when neither
    /// file can provide a provider.
    pub fn load(run_dir: &Path) -> Result<Self> {
        let path = run_dir.join(DISCOVER_RUN_FILENAME);
        if path.is_file() {
            return read_json(&path);
        }
        let matrix_path = run_dir.join("exploration_matrix.json");
        if !matrix_path.is_file() {
            return Err(Error::InvalidState(format!(
                "{} has neither {DISCOVER_RUN_FILENAME} nor exploration_matrix.json: \
                 not a discover run, or it stopped before it could be resumed",
                run_dir.display()
            )));
        }
        let matrix: ExplorationMatrix = read_json(&matrix_path)?;
        let temperature_profiles: BTreeMap<String, TemperatureProfile> =
            matrix.temperature_profiles.into_iter().collect();
        let provider = temperature_profiles
            .keys()
            .find_map(|key| key.split_once("::"))
            .map(|(section, model)| format!("{section}:{model}"))
            .ok_or_else(|| {
                Error::InvalidState(format!(
                    "{}: no {DISCOVER_RUN_FILENAME} and exploration_matrix.json names no \
                     `section::model` profile, so the provider of the run is unknown",
                    run_dir.display()
                ))
            })?;
        Ok(Self {
            provider,
            mock_dir: None,
            max_parallelism: None,
            sketches_per_cell: matrix.sketches_per_cell,
            matrix_spec: Vec::new(),
            dimensions: None,
            facets_per_dimension: None,
            llm_derive: false,
            temperature_profiles,
            default_profile: Some(matrix.default_profile),
        })
    }

    /// `true` when the dimensions come from the model: no non-blank
    /// matrix spec entry, and either `llm_derive` or not both counts.
    pub fn derives_dimensions(&self) -> bool {
        let has_spec = self.matrix_spec.iter().any(|s| !s.trim().is_empty());
        let has_counts = self.dimensions.is_some() && self.facets_per_dimension.is_some();
        !has_spec && (self.llm_derive || !has_counts)
    }

    /// Overwrite the matrix knobs of `cfg` with this spec, so every
    /// phase reads the spec's values.
    pub fn apply_to(&self, cfg: &mut DiscoveryMatrixConfig) {
        cfg.sketches_per_cell = self.sketches_per_cell;
        cfg.matrix_spec = self.matrix_spec.clone();
        cfg.dimensions = self.dimensions;
        cfg.facets_per_dimension = self.facets_per_dimension;
        cfg.llm_derive_first = self.llm_derive;
        cfg.temperature_profiles = self
            .temperature_profiles
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        cfg.default_profile = self.default_profile.clone();
    }
}

#[cfg(test)]
mod tests;
