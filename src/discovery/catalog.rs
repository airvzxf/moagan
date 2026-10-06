//! Catalogue of a discover run: every sketch placed in its matrix cell.
//!
//! The catalogue is the deterministic, LLM-free view of a finished
//! fan-out. Cells follow the matrix order; each non-empty cell holds
//! one implicit group with its sketches ordered by id until a curation
//! replaces it (see [`crate::discovery::curation`]). A sketch whose
//! angle names no matrix cell lands in a trailing "outside the matrix"
//! cell, so no sketch is ever dropped.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::discovery::matrix::{DimensionFacetDescription, ExplorationMatrix};
use crate::domain::Sketch;

/// Dimension id of the cell that holds sketches with an unknown angle.
pub const OUTSIDE_DIMENSION_ID: &str = "outside-matrix";
/// Facet id of the cell that holds sketches with an unknown angle.
pub const OUTSIDE_FACET_ID: &str = "unmatched";
/// Label of the single group of a cell that is not curated.
pub const DEFAULT_GROUP_LABEL: &str = "All theses";

const OUTSIDE_DIMENSION_LABEL: &str = "Outside the matrix";
const OUTSIDE_FACET_LABEL: &str = "Unmatched angle";

/// Every sketch of a run, placed in the cells of its matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// Cells in matrix order, plus the outside cell when needed.
    pub cells: Vec<CellEntry>,
}

/// One matrix cell (`dimension × facet`) and its sketches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellEntry {
    /// Dimension id from the matrix.
    pub dimension_id: String,
    /// Facet id from the matrix.
    pub facet_id: String,
    /// Dimension label from the matrix.
    pub dimension_label: String,
    /// Facet label from the matrix.
    pub facet_label: String,
    /// Facet description from `discovery_dimensions.json`; empty when absent.
    pub description: String,
    /// Whether the groups come from a curation.
    #[serde(default)]
    pub curation: CellCuration,
    /// Groups of sketches; empty when the cell has no sketch.
    pub groups: Vec<Group>,
    /// Pairs of theses whose choices cannot both be adopted; empty unless curated.
    #[serde(default)]
    pub tensions: Vec<Tension>,
}

/// Where the groups of a cell come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellCuration {
    /// One implicit group: the cell was not curated.
    #[default]
    Flat,
    /// The groups of a valid curation.
    Grouped,
    /// The curation failed; the cell keeps its implicit group.
    Failed,
}

/// Two theses of one cell whose choices cannot both be adopted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tension {
    /// Sketch id of the first thesis.
    pub a: String,
    /// Sketch id of the second thesis.
    pub b: String,
    /// The incompatible choices, in one sentence.
    pub note: String,
}

/// A labelled set of sketches inside one cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// Short label of the group.
    pub label: String,
    /// One-line summary; empty for the implicit group.
    pub summary: String,
    /// Sketch that states the group's approach best (always a member);
    /// `None` for the implicit group.
    #[serde(default)]
    pub representative: Option<String>,
    /// Sketches of the group: ordered by sketch id, except that a
    /// curated group lists its representative first.
    pub members: Vec<Member>,
}

/// One sketch inside a group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    /// Id of the sketch (its file stem under `sketches/`).
    pub sketch_id: String,
    /// Sketch of the same group this one repeats; `None` unless curated.
    pub duplicate_of: Option<String>,
}

/// Why a catalogue does not list every sketch exactly once (invariant I3).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CatalogError {
    /// A sketch of the run is in no cell.
    #[error("sketch {0} is missing from the catalogue")]
    Missing(String),
    /// A sketch of the run is listed more than once.
    #[error("sketch {0} appears {1} times in the catalogue")]
    Repeated(String, usize),
    /// The catalogue lists an id that is not a sketch of the run.
    #[error("catalogue lists {0}, which is not a sketch of this run")]
    Unknown(String),
}

/// Place every sketch in the cell named by its `angle`
/// (`"{dimension_id}:{facet_id}"`). Cells keep the matrix order; a
/// non-empty cell gets one [`DEFAULT_GROUP_LABEL`] group with members
/// sorted by id. Sketches with an unknown angle go to a trailing
/// outside cell, created only when at least one such sketch exists.
pub fn build_catalog(
    matrix: &ExplorationMatrix,
    descriptions: &[DimensionFacetDescription],
    sketches: &[Sketch],
) -> Catalog {
    let mut cells: Vec<CellEntry> = Vec::new();
    let mut index_of: BTreeMap<String, usize> = BTreeMap::new();
    for dimension in &matrix.dimensions {
        for facet in &dimension.facets {
            let key = format!("{}:{}", dimension.id, facet.id);
            index_of.entry(key).or_insert(cells.len());
            let description = descriptions
                .iter()
                .find(|d| d.dimension_id == dimension.id && d.facet_id == facet.id)
                .map(|d| d.description.clone())
                .unwrap_or_default();
            cells.push(CellEntry {
                dimension_id: dimension.id.clone(),
                facet_id: facet.id.clone(),
                dimension_label: dimension.label.clone(),
                facet_label: facet.label.clone(),
                description,
                curation: CellCuration::Flat,
                groups: Vec::new(),
                tensions: Vec::new(),
            });
        }
    }

    let mut members_of: Vec<Vec<String>> = vec![Vec::new(); cells.len()];
    let mut outside: Vec<String> = Vec::new();
    for sketch in sketches {
        match index_of.get(&sketch.angle) {
            Some(&i) => members_of[i].push(sketch.id.clone()),
            None => outside.push(sketch.id.clone()),
        }
    }
    if !outside.is_empty() {
        cells.push(CellEntry {
            dimension_id: OUTSIDE_DIMENSION_ID.to_owned(),
            facet_id: OUTSIDE_FACET_ID.to_owned(),
            dimension_label: OUTSIDE_DIMENSION_LABEL.to_owned(),
            facet_label: OUTSIDE_FACET_LABEL.to_owned(),
            description: String::new(),
            curation: CellCuration::Flat,
            groups: Vec::new(),
            tensions: Vec::new(),
        });
        members_of.push(outside);
    }

    for (cell, mut ids) in cells.iter_mut().zip(members_of) {
        if ids.is_empty() {
            continue;
        }
        ids.sort();
        cell.groups.push(Group {
            label: DEFAULT_GROUP_LABEL.to_owned(),
            summary: String::new(),
            representative: None,
            members: ids
                .into_iter()
                .map(|sketch_id| Member {
                    sketch_id,
                    duplicate_of: None,
                })
                .collect(),
        });
    }
    Catalog { cells }
}

impl Catalog {
    /// Check invariant I3 against the run's sketches: each sketch id is
    /// listed exactly once and no listed id is foreign. Sketches are
    /// checked in the given order, then catalogue members in catalogue
    /// order; the first violation is returned.
    pub fn validate(&self, sketches: &[Sketch]) -> Result<(), CatalogError> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for member in self.members() {
            *counts.entry(member.sketch_id.as_str()).or_insert(0) += 1;
        }
        for sketch in sketches {
            match counts.get(sketch.id.as_str()).copied().unwrap_or(0) {
                0 => return Err(CatalogError::Missing(sketch.id.clone())),
                1 => {}
                n => return Err(CatalogError::Repeated(sketch.id.clone(), n)),
            }
        }
        let known: BTreeSet<&str> = sketches.iter().map(|s| s.id.as_str()).collect();
        for member in self.members() {
            if !known.contains(member.sketch_id.as_str()) {
                return Err(CatalogError::Unknown(member.sketch_id.clone()));
            }
        }
        Ok(())
    }

    /// Number of members over every group of every cell.
    pub fn member_count(&self) -> usize {
        self.members().count()
    }

    /// First cell that lists `sketch_id`, if any.
    pub fn cell_of(&self, sketch_id: &str) -> Option<&CellEntry> {
        self.cells.iter().find(|cell| {
            cell.groups
                .iter()
                .any(|g| g.members.iter().any(|m| m.sketch_id == sketch_id))
        })
    }

    fn members(&self) -> impl Iterator<Item = &Member> {
        self.cells
            .iter()
            .flat_map(|c| c.groups.iter())
            .flat_map(|g| g.members.iter())
    }
}

#[cfg(test)]
mod tests;
