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
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use futures::future::join_all;

use crate::discovery::matrix::{ExplorationMatrix, MatrixCell};
use crate::discovery::saturation::SaturationTracker;
use crate::discovery::sketch_retry::retry_sketch_extraction;
use crate::discovery::state::SketchLoopState;
use crate::discovery::stop_policy::{StopDecision, StopPolicy, StopReason};
use crate::domain::Sketch;
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::{discover_matrix_system_prompt, system_prompt};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::telemetry::event::TelemetryEvent;

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

/// Translate a [`StopReason`] into the number of sketches the
/// matrix phase should keep. The rule of thumb: `Saturated` and
/// `MaxSketchesReached` keep the tracker's `completed` count;
/// `OutliersCollected` keeps the survivors minus the outlier
/// cap; everything else keeps the full batch. The function
/// never returns more than `current` (caller's invariant).
fn trim_count_for_reason(
    reason: &StopReason,
    tracker: &SaturationTracker,
    current: usize,
) -> usize {
    let preferred = match reason {
        StopReason::Saturated | StopReason::MaxSketchesReached | StopReason::MinSketchesReached => {
            tracker.completed
        }
        StopReason::OutliersCollected => current.saturating_sub(tracker.outliers_collected),
        StopReason::BudgetExhausted | StopReason::Cancelled => current,
    };
    preferred.min(current)
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

        // Resilience (D.34.2): load any previously-persisted sketch
        // loop state so a crashed mid-loop run can resume from the
        // last completed sketch instead of starting over. A missing
        // or version-mismatched file is normal on a fresh run.
        let run_dir = ctx.run_dir().root().to_path_buf();
        let mut state = match SketchLoopState::load(&run_dir)? {
            Some(s) => {
                tracing::info!(
                    completed = s.completed_sketches.len(),
                    failed = s.failed_attempts,
                    "resuming discover_matrix from persisted state"
                );
                s
            }
            None => SketchLoopState::new("discover_matrix".to_owned()),
        };

        // Build the future list (cell, index-in-cell, sketch_id).
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brief_arc = Arc::new(brief_text);
        let system_arc = Arc::clone(&system);
        let futures = cells.iter().flat_map(|cell| {
            let brief = Arc::clone(&brief_arc);
            let system = Arc::clone(&system_arc);
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
                    // D.34.1 / PR-05: drive the sketch extraction
                    // through the bounded retry helper in
                    // `src/discovery/sketch_retry.rs`
                    // (`retry_sketch_extraction`). The helper
                    // applies exponential backoff independent of
                    // the per-mode retry budget, so the
                    // matrix's worst-case 3 retries survive even
                    // when the run mode is `fast` (which would
                    // otherwise cap retries at 1 attempt). Each
                    // attempt threads its index through the new
                    // `calls.retry_count` column so the post-
                    // execution review can correlate the JSONL /
                    // SQLite call record with the warnings stream
                    // without scraping stderr. With `max_retries=3`
                    // and 2 broken responses followed by a valid
                    // one, the helper consumes exactly 3 mock calls
                    // (retry_count 0, 1, 2) — matching the spec's
                    // `retry_count` 0, 1, 2 contract.
                    let retry_counter = Arc::new(AtomicU32::new(0));
                    let mut sketch: Sketch = retry_sketch_extraction(3, || {
                        let ctx = ctx.clone();
                        let user = user.clone();
                        let system = system.as_str().to_owned();
                        let counter = Arc::clone(&retry_counter);
                        let schema_hint = system_prompt(Role::Sketch).to_owned();
                        async move {
                            let attempt = counter.fetch_add(1, Ordering::SeqCst);
                            let result: Result<Sketch> = async {
                                // First attempt (`attempt == 0`)
                                // goes through the cache-aware
                                // path so re-running the same
                                // prompt reuses a prior response.
                                // Retries bypass the cache so a
                                // previously cached broken
                                // response does not poison the
                                // retry budget (the original
                                // `call_with_retry_parse` follows
                                // the same rule; pin it here so
                                // the matrix does too).
                                let started_unix = crate::time::now_unix_secs();
                                let raw = if attempt == 0 {
                                    ctx.call_with_retry(Role::Sketch, system, user, attempt)
                                        .await?
                                } else {
                                    ctx.call_uncached(
                                        Role::Sketch,
                                        system,
                                        user,
                                        started_unix,
                                        attempt,
                                    )
                                    .await?
                                };
                                ctx.parse_model_json::<Sketch>(
                                    Role::Sketch,
                                    &raw.text,
                                    &schema_hint,
                                )
                            }
                            .await;
                            result
                        }
                    })
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
        let total_attempts = results.len();
        let mut paths = Vec::with_capacity(results.len());
        let mut surviving: Vec<Sketch> = Vec::with_capacity(results.len());
        let mut failed_count: usize = 0;
        for r in results {
            let sketch = match r {
                Ok(s) => s,
                Err(e) => {
                    failed_count += 1;
                    state.record_failure();
                    state.save(&run_dir)?;
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
                state.record_failure();
                state.save(&run_dir)?;
                continue;
            }
            let id = sketch.id.clone();
            let path = sketches_dir.join(format!("{id}.json"));
            write_json(&path, &sketch)?;
            state.record_completion(id);
            state.save(&run_dir)?;
            surviving.push(sketch);
            paths.push(path);
        }

        // Quality gate (D.13.21): if more than half of the attempts
        // failed, the surviving sketches are likely garbage and the
        // pipeline must not advance. A minimum-attempts guard of 4
        // keeps small runs from aborting on a single bad sample.
        if failed_count * 2 >= total_attempts && total_attempts >= 4 {
            return Err(Error::DiscoveryQualityTooLow {
                failed: failed_count,
                total: total_attempts,
                threshold_pct: 50,
            });
        }

        if paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_matrix produced zero sketches".into(),
            ));
        }

        // Stop policy (PR-19, D.13.1/.2/.3/.7/.8): the
        // [`SaturationTracker`] observes the surviving batch and
        // decides whether the matrix should keep going. The
        // matrix phase generates every cell concurrently, so
        // the realistic wiring is post-batch: we count the
        // survivors, run the policy, and trim `paths` to the
        // tracker's preferred count when the policy says `Stop`.
        // The clusters snapshot is empty here (clustering lives
        // in `discover_cluster`); the tracker's `update` correctly
        // handles that case (no mean similarity signal → the
        // `MaxSketchesReached` / `OutliersCollected` branches
        // still fire).
        let policy = StopPolicy::default();
        let mut tracker =
            SaturationTracker::with_policy(self.matrix.cardinality().max(paths.len()), policy);
        tracker.record_completions(paths.len());
        let decision = tracker.update(&surviving, &[]);
        if let StopDecision::Stop { reason } = decision {
            let preferred = trim_count_for_reason(&reason, &tracker, paths.len());
            if preferred < paths.len() {
                let dropped = paths.len() - preferred;
                tracing::info!(
                    kept = preferred,
                    dropped,
                    reason = ?reason,
                    "SaturationTracker::update returned Stop; trimming surviving set"
                );
                for path in paths.iter().skip(preferred) {
                    let _ = std::fs::remove_file(path);
                }
                paths.truncate(preferred);
            }
            if reason == StopReason::Saturated {
                TelemetryEvent::DiscoverySaturated {
                    run_id: ctx.run_id.to_string(),
                    coverage: tracker.coverage(),
                    at_unix: crate::time::now_unix_secs(),
                }
                .emit();
            }
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

        // Successful loop: drop the persisted state so the next
        // run starts fresh. Keeping the file around would only
        // confuse the next invocation (different matrix, different
        // sketch IDs).
        SketchLoopState::delete(&run_dir)?;

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
