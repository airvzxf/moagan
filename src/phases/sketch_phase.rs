//! Sketch phase. Fans out short, opinionated hypotheses before any
//! full proposal is written. Each sketch is isolated from the others
//! so the model cannot converge prematurely.
//!
//! Per T01-06 §5.5:
//!
//! > Agents do not see other sketches. This avoids premature
//! > convergence.
//!
//! The fan-out uses the same `Semaphore`-bounded `join_all` pattern as
//! `ProposePhase` so the global `max_parallelism` is honoured.
//!
//! Outputs:
//! - `sketches/sk_<uuid7>.json` — one per surviving sketch.
//! - `final/sketches_summary.json` — aggregate view of the angle
//!   distribution and the dropped-sketch count, used by `moagan
//!   inspect`.
//!
//! Cardinality: `count` is supplied by the mode table (4 for
//! `standard`/`batch`, 6 for `deep`, 12 for `explore`, 0 for `fast`).
//! When `count == 0` the phase is a no-op and emits an empty
//! `PhaseOutput::Sketches` — the wiring in `build_pipeline_for_mode`
//! uses this fact to skip the phase in `fast` mode without an extra
//! branch.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::Sketch;
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Default angles cycled across the fan-out. The list is the
/// `spec §5.5` recommended set; when `count > angles.len()` the cycle
/// repeats with a `(N)` suffix so two sketches of the same angle still
/// receive distinct prompts (and thus distinct cache keys).
const DEFAULT_ANGLES: &[&str] = &[
    "minimalist",
    "pragmatic",
    "production-grade",
    "security-first",
    "cost-first",
    "scalability",
    "maintainability",
    "exploratory",
    "contrarian",
    "domain-first",
];

/// Result of filtering raw sketches after the fan-out.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SketchFilterStats {
    /// Total sketches returned by the LLM.
    pub raw: usize,
    /// Sketches dropped because `thesis` was empty or too short.
    pub dropped_empty_thesis: usize,
    /// Sketches dropped because a hard constraint check failed.
    pub dropped_hard_constraint: usize,
    /// Sketches that survived all filters.
    pub kept: usize,
}

/// Sketch phase. Generates `count` sketches concurrently, applies a
/// minimum filter, and persists each survivor under `sketches/`.
pub struct SketchPhase {
    /// Number of sketches to generate. 0 makes the phase a no-op.
    pub count: u32,
}

impl SketchPhase {
    /// Return the angle used for the i-th sketch in a fan-out.
    /// Cycled from `DEFAULT_ANGLES`, suffixed when the count exceeds
    /// the table so distinct prompts reach the cache.
    fn angle_for(i: usize) -> String {
        let base = DEFAULT_ANGLES[i % DEFAULT_ANGLES.len()];
        if i < DEFAULT_ANGLES.len() {
            base.to_string()
        } else {
            format!("{base}-{}", i / DEFAULT_ANGLES.len())
        }
    }

    /// Cheap pre-filter applied before persistence. Spec §5.5 lists
    /// six checks; v0.2 only enforces the two that are mechanical
    /// (empty thesis, hard-constraint false). The richer
    /// redundancy/coverage detectors land in Sub-fase A follow-up.
    #[cfg(test)]
    fn is_acceptable(sk: &Sketch) -> bool {
        if sk.thesis.trim().len() < 30 {
            return false;
        }
        if sk.hard_constraint_check.values().any(|ok| !*ok) {
            return false;
        }
        true
    }
}

#[async_trait]
impl Phase for SketchPhase {
    fn name(&self) -> &'static str {
        "sketch"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let count = self.count as usize;
        if count == 0 {
            // `fast` mode (and any future caller that explicitly opts
            // out) skips the phase entirely; persist an empty
            // summary so `inspect` can tell `fast` from a mode that
            // ran 0 sketches because the budget ran out.
            let summary_path = ctx.run_dir().final_dir().join("sketches_summary.json");
            let stats = SketchFilterStats::default();
            write_json(&summary_path, &stats)?;
            return Ok(PhaseOutput::Sketches(Vec::new()));
        }

        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let user = serde_json::to_string(&brief).map_err(Error::from)?;
        let system = system_prompt(Role::Sketch).to_owned();

        let sketches_dir = ctx.run_dir().sketches();
        std::fs::create_dir_all(&sketches_dir)?;

        let system_arc = std::sync::Arc::new(system);
        let user_arc = std::sync::Arc::new(user);

        let futures = (0..count).map(|i| {
            let angle = Self::angle_for(i);
            let user_with_angle = format!(
                "{}\n\nUse angle=\"{angle}\" and produce exactly one sketch.",
                user_arc.as_str()
            );
            let ctx = ctx.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let angle_for_sketch = angle.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let mut sketch: Sketch = ctx
                    .call_with_retry_parse(
                        Role::Sketch,
                        system_arc.as_str().to_owned(),
                        user_with_angle,
                        "Sketch: {thesis, key_decisions[], architecture_outline, assumptions[], strengths[], weaknesses[], hard_constraint_check{}, expected_validation}",
                        5,
                    )
                    .await?;
                if sketch.id.is_empty() {
                    sketch.id = format!("sk_{i:03}");
                }
                sketch.angle = angle_for_sketch;
                Ok::<Sketch, crate::error::Error>(sketch)
            }
        });

        let results = join_all(futures).await;
        let mut stats = SketchFilterStats {
            raw: results.len(),
            ..Default::default()
        };
        let mut paths = Vec::with_capacity(count);
        for r in results {
            let sketch = match r {
                Ok(s) => s,
                Err(e) => {
                    // One sketch failing must not abort the phase;
                    // log a warning and continue so the surviving
                    // sketches still feed the selection step.
                    let _ = ctx.telemetry.warn(
                        "phase.sketch_skipped",
                        "warn",
                        "sketch dropped because the LLM call failed",
                        serde_json::json!({"error": e.to_string()}),
                        crate::telemetry::WarningContext {
                            phase: Some("sketch".into()),
                            role: Some("sketch".into()),
                            ..Default::default()
                        },
                    );
                    stats.dropped_empty_thesis += 1;
                    continue;
                }
            };
            if sketch.thesis.trim().len() < 30 {
                stats.dropped_empty_thesis += 1;
                continue;
            }
            if sketch.hard_constraint_check.values().any(|ok| !*ok) {
                stats.dropped_hard_constraint += 1;
                continue;
            }
            let id = sketch.id.clone();
            let path: PathBuf = sketches_dir.join(format!("{id}.json"));
            write_json(&path, &sketch)?;
            paths.push(path);
        }
        stats.kept = paths.len();
        let summary_path = ctx.run_dir().final_dir().join("sketches_summary.json");
        write_json(&summary_path, &stats)?;
        Ok(PhaseOutput::Sketches(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Sketch;

    #[test]
    fn angle_cycles_through_default_table() {
        assert_eq!(SketchPhase::angle_for(0), "minimalist");
        assert_eq!(SketchPhase::angle_for(1), "pragmatic");
        assert_eq!(SketchPhase::angle_for(9), "domain-first");
    }

    #[test]
    fn angle_wraps_with_suffix_after_table_size() {
        // Beyond the default 10, the same base repeats with a
        // distinct suffix so prompts (and cache keys) stay distinct.
        assert_eq!(SketchPhase::angle_for(10), "minimalist-1");
        assert_eq!(SketchPhase::angle_for(11), "pragmatic-1");
    }

    #[test]
    fn acceptable_sketch_survives() {
        let sk = Sketch {
            thesis: "Use Rust and SQLite, single binary, no daemon.".into(),
            hard_constraint_check: [("no_serverless".to_string(), true)].into_iter().collect(),
            ..Default::default()
        };
        assert!(SketchPhase::is_acceptable(&sk));
    }

    #[test]
    fn short_thesis_rejected() {
        let sk = Sketch {
            thesis: "tiny".into(),
            hard_constraint_check: [("no_serverless".to_string(), true)].into_iter().collect(),
            ..Default::default()
        };
        assert!(!SketchPhase::is_acceptable(&sk));
    }

    #[test]
    fn failed_hard_constraint_rejected() {
        let sk = Sketch {
            thesis: "Use serverless functions for the whole pipeline.".into(),
            hard_constraint_check: [("no_serverless".to_string(), false)].into_iter().collect(),
            ..Default::default()
        };
        assert!(!SketchPhase::is_acceptable(&sk));
    }

    #[test]
    fn filter_stats_round_trip_json() {
        let s = SketchFilterStats {
            raw: 4,
            dropped_empty_thesis: 1,
            dropped_hard_constraint: 1,
            kept: 2,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: SketchFilterStats = serde_json::from_str(&j).unwrap();
        assert_eq!(back.raw, 4);
        assert_eq!(back.kept, 2);
    }
}
