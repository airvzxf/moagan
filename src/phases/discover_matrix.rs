//! Discovery mode — `discover_matrix` phase.
//!
//! Generates an `ExplorationMatrix x sketches_per_cell` fan-out of
//! sketches. Unlike `SketchPhase` (which rotates a fixed set of
//! angles), the matrix phase samples the design space
//! systematically:
//!
//! 1. The matrix has `dimensions × facets_per_dim` cells (e.g. 4 × 2
//!    = 8).
//! 2. Each cell produces `cardinality / cells` sketches (so the
//!    minimum 80 is enforced by the matrix itself).
//! 3. Each sketch is generated with the `discover_matrix` system
//!    prompt and `(dimension, facet)` injected into the user payload.
//!
//! Output: `sketches/sk_<uuid7>.json` (one per surviving sketch) plus
//! `exploration_matrix.json` (the matrix that was used) so a second
//! run can verify reproducibility without re-running the fan-out.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;

use crate::discovery::matrix::{ExplorationMatrix, MatrixCell};
use crate::domain::Sketch;
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::{discover_matrix_system_prompt, system_prompt};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Discovery matrix phase. Owns the matrix it generates so a
/// follow-up run can inspect the schema even if the LLM responses
/// were lost.
pub struct DiscoverMatrixPhase {
    /// The matrix to use. The phase does NOT auto-pick defaults —
    /// callers construct the matrix with `ExplorationMatrix::default_for`
    /// or `from_dimensions` and pass it in.
    pub matrix: ExplorationMatrix,
}

impl DiscoverMatrixPhase {
    /// Build a phase with the default matrix sized for `cardinality`.
    pub fn with_cardinality(cardinality: usize) -> Self {
        Self {
            matrix: ExplorationMatrix::default_for(cardinality),
        }
    }

    /// Build a phase from explicit `(dimensions, facets_per_dim)`,
    /// sizing `sketches_per_cell` so the total reaches the con\\\
    /// figured `cardinality` (default 80).
    pub fn from_dimensions(
        num_dimensions: usize,
        facets_per_dim: usize,
        cardinality: usize,
    ) -> Self {
        let mut m = ExplorationMatrix::from_dimensions(num_dimensions, facets_per_dim);
        let cells = m.cells().max(1);
        m.sketches_per_cell = (cardinality / cells).max(1);
        Self { matrix: m }
    }

    /// Persist the matrix alongside the sketches so the run can be
    /// reproduced.
    fn persist_matrix(&self, ctx: &RunContext) -> Result<PathBuf> {
        let path = ctx.run_dir().root().join("exploration_matrix.json");
        write_json(&path, &self.matrix)?;
        Ok(path)
    }

    /// Test-only helper so the matrix can be persisted without a
    /// full `RunContext`.
    #[cfg(test)]
    fn persist_matrix_for_test(&self, run_dir: &crate::fs_layout::RunDir<'_>) -> Result<PathBuf> {
        let path = run_dir.root().join("exploration_matrix.json");
        write_json(&path, &self.matrix)?;
        Ok(path)
    }

    /// Build the user payload for a `(cell, sketch_index)` pair.
    /// The sketch is told the brief and the cell's label, and asked
    /// to produce one sketch biased by that angle.
    fn user_payload(brief: &str, cell: &MatrixCell, index: usize) -> String {
        format!(
            "{brief}\n\n\
             Use dimension=\"{dim_id}\" and facet=\"{facet_id}\" (label: \"{label}\") and \
             produce exactly one sketch (cell index {index}).",
            brief = brief,
            dim_id = cell.dimension_id,
            facet_id = cell.facet_id,
            label = cell.label,
            index = index,
        )
    }
}

#[async_trait]
impl Phase for DiscoverMatrixPhase {
    fn name(&self) -> &'static str {
        "discover_matrix"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let matrix_path = self.persist_matrix(ctx)?;

        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let brief_text = serde_json::to_string(&brief).map_err(Error::from)?;
        let system = Arc::new(discover_matrix_system_prompt().to_owned());

        let sketches_dir = ctx.run_dir().sketches();
        std::fs::create_dir_all(&sketches_dir)?;

        let per_cell = self.matrix.sketches_per_cell.max(1);
        let cells: Vec<MatrixCell> = self.matrix.iter_cells().collect();

        // Build the future list (cell, index-in-cell, sketch_id).
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brief_arc = Arc::new(brief_text);
        let futures = cells.iter().flat_map(|cell| {
            let brief = Arc::clone(&brief_arc);
            let system = Arc::clone(&system);
            let counter = Arc::clone(&counter);
            (0..per_cell).map(move |_i| {
                let cell = cell.clone();
                let brief = Arc::clone(&brief);
                let system = Arc::clone(&system);
                let counter = Arc::clone(&counter);
                let ctx = ctx.clone();
                async move {
                    let _permit = ctx.parallelism.acquire().await?;
                    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let id = format!("sk_{:04}", n);
                    let user = DiscoverMatrixPhase::user_payload(brief.as_str(), &cell, n);
                    let mut sketch: Sketch = ctx
                        .call_with_retry_parse(
                            Role::Sketch,
                            system.as_str().to_owned(),
                            user,
                            system_prompt(Role::Sketch),
                            5,
                        )
                        .await?;
                    if sketch.id.is_empty() {
                        sketch.id = id.clone();
                    }
                    sketch.angle = format!("{}:{}", cell.dimension_id, cell.facet_id);
                    Ok::<Sketch, crate::error::Error>(sketch)
                }
            })
        });

        let results = join_all(futures).await;
        let mut paths = Vec::with_capacity(results.len());
        for r in results {
            let sketch = match r {
                Ok(s) => s,
                Err(e) => {
                    let _ = ctx.telemetry.warn(
                        "phase.discover_matrix.skipped",
                        "warn",
                        "sketch dropped because the LLM call failed",
                        serde_json::json!({"error": e.to_string()}),
                        crate::telemetry::WarningContext {
                            phase: Some("discover_matrix".into()),
                            role: Some("sketch".into()),
                            ..Default::default()
                        },
                    );
                    continue;
                }
            };
            if sketch.thesis.trim().len() < 30 {
                continue;
            }
            let id = sketch.id.clone();
            let path = sketches_dir.join(format!("{id}.json"));
            write_json(&path, &sketch)?;
            paths.push(path);
        }

        if paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_matrix produced zero sketches".into(),
            ));
        }

        // Quick triage summary so the next phase can skip a full
        // re-read of the directory.
        let summary_path = ctx.run_dir().root().join("exploration_summary.json");
        let summary = serde_json::json!({
            "matrix_path": matrix_path,
            "cells": cells.len(),
            "per_cell": per_cell,
            "expected_cardinality": self.matrix.cardinality(),
            "kept": paths.len(),
        });
        write_json(&summary_path, &summary)?;
        Ok(PhaseOutput::Sketches(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_cardinality_uses_default_matrix() {
        let p = DiscoverMatrixPhase::with_cardinality(80);
        assert_eq!(p.matrix.cardinality(), 80);
        assert_eq!(p.matrix.cells(), 8);
    }

    #[test]
    fn from_dimensions_reaches_default_cardinality() {
        let p = DiscoverMatrixPhase::from_dimensions(3, 2, 80);
        assert_eq!(p.matrix.cells(), 6);
        assert!(p.matrix.cardinality() >= 78);
    }

    #[test]
    fn user_payload_contains_dimension_and_facet() {
        let cell = MatrixCell {
            dimension_id: "deployment-model".into(),
            facet_id: "serverless".into(),
            label: "Deployment model / serverless".into(),
        };
        let s = DiscoverMatrixPhase::user_payload("BRIEF", &cell, 7);
        assert!(s.contains("deployment-model"));
        assert!(s.contains("serverless"));
        assert!(s.contains("BRIEF"));
        assert!(s.contains("7"));
    }

    #[test]
    fn user_payload_does_not_leak_other_cells() {
        let cell = MatrixCell {
            dimension_id: "storage".into(),
            facet_id: "sql".into(),
            label: "Storage strategy / SQL".into(),
        };
        let s = DiscoverMatrixPhase::user_payload("B", &cell, 0);
        assert!(!s.contains("deployment-model"));
        assert!(!s.contains("self-hosted"));
    }

    #[test]
    fn matrix_persists() {
        let p = DiscoverMatrixPhase::with_cardinality(80);
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
        let run_dir = home.run_dir(crate::ids::RunId::new());
        run_dir.ensure().unwrap();
        let path = p.persist_matrix_for_test(&run_dir).unwrap();
        assert!(path.exists());
        let back: ExplorationMatrix = read_json(&path).unwrap();
        assert_eq!(back.cardinality(), 80);
    }
}

// Test-only helper so the matrix can be persisted without a full
// RunContext.
