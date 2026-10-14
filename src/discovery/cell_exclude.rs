//! Per-cell exclusions of a discover run (`--cell-exclude`).
//!
//! An exclusion names one matrix cell and something the theses of
//! that cell must not propose. Only that cell's thesis payload carries
//! it, so an attractor can be pushed aside in one cell without steering
//! the rest of the fan-out. The exclusions are part of the run spec
//! (`discover_run.json`), so `moagan continue` keeps them.

use serde::{Deserialize, Serialize};

use crate::discovery::matrix::MatrixCell;
use crate::error::{Error, Result};

/// One `--cell-exclude 'dimension/facet=text'` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellExclusion {
    /// Dimension id of the cell.
    pub dimension_id: String,
    /// Facet id of the cell.
    pub facet_id: String,
    /// What the theses of the cell must not propose.
    pub text: String,
}

impl CellExclusion {
    /// Parse `dimension/facet=text`. The cell is the part before the
    /// first `=`, split at its first `/`; the text is everything after
    /// that `=`. Each of the three parts is trimmed and must not be
    /// empty. Fails with `Error::InvalidArgs` naming the entry and the
    /// expected form.
    pub fn parse(entry: &str) -> Result<Self> {
        let parts = entry.split_once('=').and_then(|(cell, text)| {
            let (dimension_id, facet_id) = cell.split_once('/')?;
            Some((dimension_id.trim(), facet_id.trim(), text.trim()))
        });
        match parts {
            Some((dimension_id, facet_id, text))
                if !dimension_id.is_empty() && !facet_id.is_empty() && !text.is_empty() =>
            {
                Ok(Self {
                    dimension_id: dimension_id.to_owned(),
                    facet_id: facet_id.to_owned(),
                    text: text.to_owned(),
                })
            }
            _ => Err(Error::InvalidArgs(format!(
                "--cell-exclude {entry:?}: expected 'dimension/facet=text' \
                 (e.g. 'platform/database=SQLite')"
            ))),
        }
    }

    /// Whether this exclusion names `cell` (same dimension id and facet id).
    pub fn applies_to(&self, cell: &MatrixCell) -> bool {
        self.dimension_id == cell.dimension_id && self.facet_id == cell.facet_id
    }
}

#[cfg(test)]
mod tests;
