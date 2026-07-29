//! `ExplorationMatrix` — the discovery fan-out grid.
//!
//! Per V4 §6.4 and proposal-02-rust.md §9.1, the discovery matrix is
//! `dims × facets_per_dim × cells_per_facet`. Each cell produces one
//! sketch via the `discover_matrix` prompt. The matrix differs from
//! the angle-rotated sketch fan-out of the standard pipeline because
//! it samples the *space* systematically rather than rotating
//! through a fixed list of personas.

use std::collections::BTreeMap;

/// One dimension in the exploration matrix. A dimension is a
/// high-level axis of variation the user wants to explore (e.g.
/// "deployment model", "storage layer", "auth strategy").
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Dimension {
    /// Stable id (kebab-case).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Facets (the values the dimension can take).
    pub facets: Vec<Facet>,
}

/// One facet (value) of a dimension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Facet {
    /// Stable id (kebab-case).
    pub id: String,
    /// Human-readable label.
    pub label: String,
}

/// One cell in the matrix — a `(dimension × facet)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MatrixCell {
    /// Dimension id.
    pub dimension_id: String,
    /// Facet id.
    pub facet_id: String,
    /// Composite label for the prompt.
    pub label: String,
}

/// The full exploration matrix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplorationMatrix {
    /// Dimensions in declaration order.
    pub dimensions: Vec<Dimension>,
    /// Number of sketches to generate per cell. Default 10
    /// (4 dims × 2 facets × 10 = 80 sketches, the user's chosen
    /// floor).
    pub sketches_per_cell: usize,
}

impl ExplorationMatrix {
    /// Build a matrix with the default dimensions and facets when no
    /// explicit one is supplied. The defaults target the four most
    /// universally-applicable axes: deployment model, storage
    /// strategy, consistency model, and observability.
    pub fn default_for(cardinality: usize) -> Self {
        let dims = vec![
            Dimension {
                id: "deployment-model".into(),
                label: "Deployment model".into(),
                facets: vec![
                    Facet { id: "serverless".into(), label: "serverless".into() },
                    Facet { id: "self-hosted".into(), label: "self-hosted".into() },
                ],
            },
            Dimension {
                id: "storage".into(),
                label: "Storage strategy".into(),
                facets: vec![
                    Facet { id: "sql".into(), label: "SQL".into() },
                    Facet { id: "kv".into(), label: "embedded key-value".into() },
                ],
            },
            Dimension {
                id: "consistency".into(),
                label: "Consistency model".into(),
                facets: vec![
                    Facet { id: "strong".into(), label: "strong".into() },
                    Facet { id: "eventual".into(), label: "eventual".into() },
                ],
            },
            Dimension {
                id: "observability".into(),
                label: "Observability".into(),
                facets: vec![
                    Facet { id: "logs-only".into(), label: "logs only".into() },
                    Facet { id: "metrics-tracing".into(), label: "metrics + tracing".into() },
                ],
            },
        ];
        let cells = dims.iter().map(|d| d.facets.len().max(1)).sum::<usize>();
        let per_cell = (cardinality / cells.max(1)).max(1);
        Self {
            dimensions: dims,
            sketches_per_cell: per_cell,
        }
    }

    /// Build a matrix from explicit `dimensions` and `facets_per_dim`.
    /// The facets inside each dimension are auto-generated from the
    /// count (`f1`, `f2`, …, `fN`).
    pub fn from_dimensions(num_dimensions: usize, facets_per_dim: usize) -> Self {
        let dims = (0..num_dimensions)
            .map(|i| Dimension {
                id: format!("dim-{i:02}"),
                label: format!("Dimension {i:02}"),
                facets: (0..facets_per_dim)
                    .map(|j| Facet {
                        id: format!("f{}", j + 1),
                        label: format!("F{}", j + 1),
                    })
                    .collect(),
            })
            .collect();
        let cells = num_dimensions * facets_per_dim.max(1);
        let per_cell = (80 / cells).max(1);
        Self {
            dimensions: dims,
            sketches_per_cell: per_cell,
        }
    }

    /// Total number of sketches the matrix will request. The formula
    /// is `sum(dimensions.facets) * sketches_per_cell` — each cell is
    /// one `(dimension, facet)` pair, and each cell produces
    /// `sketches_per_cell` sketches.
    pub fn cardinality(&self) -> usize {
        self.cells() * self.sketches_per_cell
    }

    /// Number of cells (one per `dimension × facet` pair).
    pub fn cells(&self) -> usize {
        // Empty matrix has zero cells (no fan-out). A non-empty
        // matrix with a zero-facet dimension still counts that
        // dimension as one cell so the fan-out never silently
        // disappears.
        if self.dimensions.is_empty() {
            0
        } else {
            self.dimensions
                .iter()
                .map(|d| d.facets.len().max(1))
                .sum()
        }
    }

    /// Iterate over every cell in declaration order.
    pub fn iter_cells(&self) -> impl Iterator<Item = MatrixCell> + '_ {
        self.dimensions.iter().flat_map(|d| {
            d.facets.iter().map(move |f| MatrixCell {
                dimension_id: d.id.clone(),
                facet_id: f.id.clone(),
                label: format!("{} / {}", d.label, f.label),
            })
        })
    }

    /// Look up a dimension by id.
    pub fn dimension(&self, id: &str) -> Option<&Dimension> {
        self.dimensions.iter().find(|d| d.id == id)
    }

    /// Tally of dimension → facet count, useful for logs.
    pub fn tally(&self) -> BTreeMap<String, usize> {
        self.dimensions.iter().map(|d| (d.id.clone(), d.facets.len())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_for_80_sketches_yields_ten_per_cell() {
        let m = ExplorationMatrix::default_for(80);
        assert_eq!(m.cardinality(), 80, "cells={} * per_cell={}", m.cells(), m.sketches_per_cell);
        assert_eq!(m.cells(), 8);
        assert_eq!(m.sketches_per_cell, 10);
    }

    #[test]
    fn default_for_160_doubles_per_cell() {
        let m = ExplorationMatrix::default_for(160);
        assert_eq!(m.cardinality(), 160);
        assert_eq!(m.sketches_per_cell, 20);
    }

    #[test]
    fn from_dimensions_creates_auto_ids() {
        let m = ExplorationMatrix::from_dimensions(3, 2);
        assert_eq!(m.dimensions.len(), 3);
        assert_eq!(m.dimensions[0].facets.len(), 2);
        assert_eq!(m.dimensions[0].facets[0].id, "f1");
        assert_eq!(m.dimensions[0].facets[1].id, "f2");
        assert_eq!(m.dimensions[1].id, "dim-01");
    }

    #[test]
    fn from_dimensions_with_zero_facets_uses_one_facet_min() {
        // The contract: at least one cell per dimension so the
        // matrix is never empty.
        let m = ExplorationMatrix::from_dimensions(4, 0);
        assert_eq!(m.cells(), 4);
    }

    #[test]
    fn iter_cells_yields_dense_grid() {
        let m = ExplorationMatrix::default_for(80);
        let cells: Vec<_> = m.iter_cells().collect();
        assert_eq!(cells.len(), 8);
        assert!(cells.iter().any(|c| c.dimension_id == "deployment-model" && c.facet_id == "serverless"));
        assert!(cells.iter().any(|c| c.dimension_id == "observability" && c.facet_id == "metrics-tracing"));
    }

    #[test]
    fn dimension_lookup_returns_match() {
        let m = ExplorationMatrix::default_for(80);
        assert_eq!(m.dimension("storage").unwrap().label, "Storage strategy");
        assert!(m.dimension("nope").is_none());
    }

    #[test]
    fn tally_counts_facets_per_dimension() {
        let m = ExplorationMatrix::default_for(80);
        let t = m.tally();
        assert_eq!(t.len(), 4);
        assert_eq!(t["deployment-model"], 2);
        assert_eq!(t["storage"], 2);
    }

    #[test]
    fn matrix_cell_serializes() {
        let c = MatrixCell {
            dimension_id: "d".into(),
            facet_id: "f".into(),
            label: "d / f".into(),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"dimension_id\":\"d\""));
        assert!(j.contains("\"facet_id\":\"f\""));
    }

    #[test]
    fn cardinality_round_trips_through_json() {
        let m = ExplorationMatrix::default_for(80);
        let j = serde_json::to_string(&m).unwrap();
        let back: ExplorationMatrix = serde_json::from_str(&j).unwrap();
        assert_eq!(back.cardinality(), 80);
    }
}
