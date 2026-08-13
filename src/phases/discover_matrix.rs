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
use crate::fs_layout::RunDir;
use crate::llm::Role;
use crate::llm::prompts::{discover_matrix_system_prompt, system_prompt};
use crate::phases::phase::{Phase, PhaseOutput, RunContext, temperature_for_role};
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

    /// Build a phase from explicit `(dimensions, facets_per_dim)`
    /// AND per-provider temperature profiles. Same shape as
    /// [`Self::from_dimensions`] but carries the profile map through
    /// to the matrix so the iteration loop can fan out across
    /// `(cell, temperature, replica)` triples.
    pub fn from_dimensions_with_profiles(
        num_dimensions: usize,
        facets_per_dim: usize,
        cardinality: usize,
        temperature_profiles: std::collections::HashMap<
            String,
            crate::discovery::matrix::TemperatureProfile,
        >,
        default_profile: crate::discovery::matrix::TemperatureProfile,
    ) -> Self {
        let mut m = ExplorationMatrix::from_dimensions_with_profiles(
            num_dimensions,
            facets_per_dim,
            temperature_profiles,
            default_profile,
        );
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

    /// Persist a human-readable draft of one surviving sketch under
    /// `<run_dir>/drafts/<sketch_id>.md` (PR-22, V4 §6.10). The
    /// discovery spec promises a `drafts/` directory but no phase
    /// previously wrote to it; this writer closes the gap by
    /// emitting one markdown per sketch with the LLM response
    /// fields plus the run metadata (model, role, temperature)
    /// so an inspector can read the raw sketch without
    /// re-parsing the JSON. The path layout matches the spec
    /// (`<run_dir>/drafts/<id>.md`) so `drafts/cat_NN/borrador.md`
    /// (V4 §6.10, the per-cluster integration draft) can coexist
    /// later in `drafts/cat_NN/` without colliding.
    ///
    /// Errors propagate so a transient disk failure does not
    /// silently leave `drafts/` empty while `sketches/` claims
    /// the run produced the artefact.
    fn write_draft(
        run_dir: &RunDir<'_>,
        sketch: &Sketch,
        model: &str,
        temperature: f32,
        role: &str,
    ) -> Result<PathBuf> {
        let drafts_dir = run_dir.drafts();
        std::fs::create_dir_all(&drafts_dir)?;
        let path = drafts_dir.join(format!("{}.md", sketch.id));
        let body = Self::render_draft(sketch, model, temperature, role);
        // Drafts are leaf artefacts (one per sketch) and are only
        // ever written by the matrix phase after the sketch JSON
        // has been durably persisted. A plain `std::fs::write`
        // matches the sidecar pattern of `sketches_summary.json`
        // (also a leaf artefact written once after the fan-out)
        // and avoids spawning the `.meta.json` sidecar that
        // `AtomicWriter` adds on every other artefact in this
        // run. The drafts dir is the spec-mandated surface, not
        // the integrity-checked one, so the sidecar would only
        // add noise to the inspect CLI output.
        std::fs::write(&path, body.as_bytes())?;
        Ok(path)
    }

    /// Render the markdown body of a draft sidecar. Pure
    /// function so unit tests can pin the wire format without
    /// touching the filesystem. The format is a YAML-style
    /// frontmatter header (greppable, easy to parse) followed
    /// by the sketch fields rendered as `# / ## / -` blocks so
    /// the file reads cleanly when opened in any markdown
    /// viewer.
    fn render_draft(sketch: &Sketch, model: &str, temperature: f32, role: &str) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", sketch.id));
        if !sketch.angle.is_empty() {
            out.push_str(&format!("angle: {}\n", sketch.angle));
        }
        out.push_str(&format!("model: {model}\n"));
        out.push_str(&format!("role: {role}\n"));
        out.push_str(&format!("temperature: {temperature:.1}\n"));
        out.push_str(&format!(
            "written_at_unix: {}\n",
            crate::time::now_unix_secs()
        ));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", sketch.id));
        if !sketch.thesis.is_empty() {
            out.push_str("## Thesis\n\n");
            out.push_str(sketch.thesis.trim());
            out.push_str("\n\n");
        }
        if !sketch.key_decisions.is_empty() {
            out.push_str("## Key decisions\n\n");
            for d in &sketch.key_decisions {
                out.push_str("- ");
                out.push_str(d.trim());
                out.push('\n');
            }
            out.push('\n');
        }
        if !sketch.architecture_outline.is_empty() {
            out.push_str("## Architecture outline\n\n");
            out.push_str(sketch.architecture_outline.trim());
            out.push_str("\n\n");
        }
        if !sketch.assumptions.is_empty() {
            out.push_str("## Assumptions\n\n");
            for a in &sketch.assumptions {
                out.push_str("- ");
                out.push_str(a.trim());
                out.push('\n');
            }
            out.push('\n');
        }
        if !sketch.strengths.is_empty() {
            out.push_str("## Strengths\n\n");
            for s in &sketch.strengths {
                out.push_str("- ");
                out.push_str(s.trim());
                out.push('\n');
            }
            out.push('\n');
        }
        if !sketch.weaknesses.is_empty() {
            out.push_str("## Weaknesses\n\n");
            for w in &sketch.weaknesses {
                out.push_str("- ");
                out.push_str(w.trim());
                out.push('\n');
            }
            out.push('\n');
        }
        if !sketch.hard_constraint_check.is_empty() {
            out.push_str("## Hard-constraint check\n\n");
            for (k, v) in &sketch.hard_constraint_check {
                out.push_str(&format!("- {k}: {v}\n"));
            }
            out.push('\n');
        }
        if !sketch.expected_validation.is_empty() {
            out.push_str("## Expected validation\n\n");
            out.push_str(sketch.expected_validation.trim());
            out.push('\n');
        }
        out
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

        // Build the future list (cell, temperature, replica, sketch_id).
        //
        // PR-D1: when the matrix carries a per-provider
        // `temperature_profiles` map (or a non-default
        // `default_profile`), the iteration expands the inner
        // `(0..per_cell)` loop into a `(temperature × replica)` loop
        // driven by `matrix.profile_for(&ctx.default_model)`. With the
        // default profile (`[1.0] × 1`) this is exactly one task per
        // `(cell, sketch_index)` pair — the v0.5 fan-out.
        //
        // The lookup key is `ctx.default_model` (the model the
        // active `RunContext` is bound to). When the operator sets
        // `--provider mimo-v2.5 --temperature-profile 'provider=mimo-v2.5;...'`,
        // the matrix's `temperature_profiles["mimo-v2.5"]` profile
        // is matched and the loop fans out per the spec.
        let profile = self.matrix.profile_for(&ctx.default_model).clone();
        let profile_temperatures: Vec<f32> = profile.temperatures.clone();
        let profile_replicas: usize = profile.replicas_per_temperature.max(1);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brief_arc = Arc::new(brief_text);
        let system_arc = Arc::clone(&system);
        // Pre-compute the full (cell, temperature, replica,
        // sketch_index) iterator as a `Vec` so the closure
        // ownership is trivial (no nested-`flat_map` returning
        // references to locals). The capacity is `cells.len() *
        // profile_temperatures.len() * profile_replicas * per_cell`
        // — the same number the loop would spawn otherwise; no
        // memory bloat. With the default profile (`[1.0] × 1`)
        // and `per_cell = sketches_per_cell`, this is exactly the
        // v0.5 `(cells × sketches_per_cell)` fan-out.
        let mut work_items: Vec<(MatrixCell, f32)> = Vec::with_capacity(
            cells.len() * profile_temperatures.len() * profile_replicas * per_cell,
        );
        for cell in cells.iter() {
            for &temperature in profile_temperatures.iter() {
                for _replica in 0..profile_replicas {
                    for _ in 0..per_cell {
                        work_items.push((cell.clone(), temperature));
                    }
                }
            }
        }
        let futures = work_items.into_iter().map(|(cell, temperature)| {
            let brief = Arc::clone(&brief_arc);
            let system = Arc::clone(&system_arc);
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
                //
                // PR-D1: every iteration is stamped with the
                // explicit `temperature` from the active
                // profile; the cache key in
                // `src/llm/cache/mod.rs:117` includes the
                // resolved temperature so different
                // `temperature` values cache distinctly
                // (the audit confirmed this; pinned here so
                // the wire path stays consistent).
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
                                ctx.call_with_retry_at_temp(
                                    Role::Sketch,
                                    system,
                                    user,
                                    attempt,
                                    temperature,
                                )
                                .await?
                            } else {
                                ctx.call_uncached_at_temp(
                                    Role::Sketch,
                                    system,
                                    user,
                                    started_unix,
                                    attempt,
                                    temperature,
                                )
                                .await?
                            };
                            ctx.parse_model_json::<Sketch>(Role::Sketch, &raw.text, &schema_hint)
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
            // PR-22 (V4 §6.10): write a per-sketch human-readable
            // draft under `drafts/<id>.md` so the discover pipeline
            // emits the sidecar the spec promises. The metadata
            // (model, temperature, role) is captured at this point
            // because the live values live on the `RunContext`
            // (default_model + role-specific temperature lookup)
            // and would otherwise have to be reconstructed after
            // the fact from the telemetry JSONL.
            let profile_overrides: Option<&std::collections::HashMap<String, f32>> =
                if ctx.config.profile_temperature_overrides.is_empty() {
                    None
                } else {
                    Some(&ctx.config.profile_temperature_overrides)
                };
            let temperature = temperature_for_role(Role::Sketch, profile_overrides);
            let draft_path = Self::write_draft(
                &ctx.run_dir(),
                &sketch,
                &ctx.default_model,
                temperature,
                Role::Sketch.as_str(),
            )?;
            tracing::debug!(
                sketch_id = %id,
                draft = %draft_path.display(),
                "draft sidecar written"
            );
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
        crate::test_support::with_moagan_home("discover_matrix_persists", |_home| {
            let p = DiscoverMatrixPhase::with_cardinality(80);
            let home = Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
            let run_dir = home.run_dir(crate::ids::RunId::new());
            run_dir.ensure().unwrap();
            let path = p.persist_matrix_for_test(&run_dir).unwrap();
            assert!(path.exists());
            let back: ExplorationMatrix = read_json(&path).unwrap();
            assert_eq!(back.cardinality(), 80);
        });
    }

    /// PR-22 unit test: `render_draft` is a pure function so the
    /// wire format can be pinned without touching the filesystem.
    /// Every section heading + the frontmatter fields + the
    /// thesis bullet are asserted so a future rename or removal
    /// of a section trips this test before it lands.
    #[test]
    fn render_draft_emits_frontmatter_and_section_headings() {
        let sketch = Sketch {
            id: "sk_0042".to_owned(),
            thesis: "Ship a single Rust binary that bundles config, embed, and runtime together."
                .to_owned(),
            key_decisions: vec!["static link".into(), "rust runtime".into()],
            architecture_outline: "A single moagan binary implements every pipeline phase."
                .to_owned(),
            assumptions: vec!["Linux + macOS only".into()],
            strengths: vec!["easy install".into()],
            weaknesses: vec!["larger binary".into()],
            hard_constraint_check: [("portable".to_owned(), true)].into_iter().collect(),
            expected_validation: "Smoke test on a fresh container rebuilds the suite.".to_owned(),
            angle: "deployment-model:serverless".to_owned(),
        };
        let body = DiscoverMatrixPhase::render_draft(&sketch, "mock-model", 1.0, "sketch");
        assert!(
            body.starts_with("---\n"),
            "must open with YAML frontmatter delimiter"
        );
        assert!(body.contains("id: sk_0042"));
        assert!(body.contains("angle: deployment-model:serverless"));
        assert!(body.contains("model: mock-model"));
        assert!(body.contains("role: sketch"));
        assert!(
            body.contains("temperature: 1.0"),
            "temperature format must keep one decimal so 1.0 doesn't render as `1`"
        );
        assert!(body.contains("written_at_unix: "));
        assert!(body.contains("\n---\n"));
        assert!(body.contains("\n# sk_0042\n"));
        assert!(body.contains("## Thesis"));
        assert!(body.contains("Ship a single Rust binary"));
        assert!(body.contains("## Key decisions"));
        assert!(body.contains("- static link"));
        assert!(body.contains("- rust runtime"));
        assert!(body.contains("## Architecture outline"));
        assert!(body.contains("## Assumptions"));
        assert!(body.contains("- Linux + macOS only"));
        assert!(body.contains("## Strengths"));
        assert!(body.contains("- easy install"));
        assert!(body.contains("## Weaknesses"));
        assert!(body.contains("- larger binary"));
        assert!(body.contains("## Hard-constraint check"));
        assert!(body.contains("- portable: true"));
        assert!(body.contains("## Expected validation"));
        assert!(body.contains("Smoke test on a fresh container rebuilds the suite."));
    }

    /// PR-22 unit test: a minimal sketch (no angle, no list
    /// fields, no hard constraints) still renders a valid
    /// frontmatter + heading. Pins the empty-section skips so a
    /// future "always render" change can't accidentally emit
    /// `## Key decisions\n\n` for a sketch that has no
    /// decisions — which would be the wrong empty section.
    #[test]
    fn render_draft_skips_empty_optional_sections() {
        let sketch = Sketch {
            id: "sk_empty".to_owned(),
            thesis: "Single Rust binary pipeline for moagan discovery sketches.".to_owned(),
            ..Sketch::default()
        };
        let body = DiscoverMatrixPhase::render_draft(&sketch, "mock-model", 1.0, "sketch");
        assert!(body.contains("id: sk_empty"));
        // No angle line because `sketch.angle.is_empty()`.
        assert!(!body.contains("angle: "));
        assert!(body.contains("## Thesis"));
        assert!(body.contains("Single Rust binary pipeline"));
        assert!(!body.contains("## Key decisions"));
        assert!(!body.contains("## Architecture outline"));
        assert!(!body.contains("## Assumptions"));
        assert!(!body.contains("## Strengths"));
        assert!(!body.contains("## Weaknesses"));
        assert!(!body.contains("## Hard-constraint check"));
        assert!(!body.contains("## Expected validation"));
    }
}

// Test-only helper so the matrix can be persisted without a full
// RunContext.
