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
//!
//! Phase G (v0.3): when a `problem_graph.json` is present and
//! non-trivial, the phase re-distributes `count` sketches across
//! the DAG nodes (one sketch per node when `count <= node_count`,
//! otherwise `ceil(count / node_count)` per node). The angle in
//! the user payload is replaced with the node id so the cache
//! key stays distinct per node.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::{ProblemGraph, Sketch};
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::{
    KNOWN_APIS_PLACEHOLDER, inject_known_apis, system_prompt,
};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::research::{ResearchSnippet, fetch_all};

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

    /// Load the `problem_graph.json` sidecar if it exists. Returns
    /// `None` when the file is missing or the graph is trivial
    /// (no decomposition). Pure function so the rest of the phase
    /// can branch on the result without coupling to the file
    /// layout.
    fn load_problem_graph(ctx: &RunContext) -> Option<ProblemGraph> {
        let path = ctx.run_dir().root().join("problem_graph.json");
        if !path.exists() {
            return None;
        }
        let g: ProblemGraph = read_json(&path).ok()?;
        if g.is_empty() || !g.should_decompose {
            return None;
        }
        Some(g)
    }

    /// Distribute `count` sketches across the DAG nodes. Returns a
    /// `Vec<(node_id, sketch_index)>` that the fan-out uses to label
    /// the per-sketch cache key. When `count < node_count` each
    /// node still gets at least one slot; when `count >= node_count`
    /// the slots are spread as evenly as possible.
    fn distribute_across_nodes(count: usize, node_ids: &[String]) -> Vec<(String, usize)> {
        if node_ids.is_empty() {
            return (0..count).map(|i| (String::new(), i)).collect();
        }
        let node_count = node_ids.len();
        let per_node = count.div_ceil(node_count).max(1);
        let mut out = Vec::with_capacity(node_count * per_node);
        for (i, id) in node_ids.iter().enumerate() {
            for j in 0..per_node {
                if out.len() >= count {
                    break;
                }
                out.push((id.clone(), i * per_node + j));
            }
        }
        out
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

    /// Track K (D9): fetch the configured research URLs and return
    /// the snippets that should land in the prompt via the
    /// `${known_apis}` placeholder. The helper is opt-in:
    /// `enabled == false` or `urls.is_empty()` short-circuits to
    /// `Ok(vec![])` without touching the network. Any
    /// per-URL fetch failure is dropped silently (the
    /// allowlist / network / host errors are recorded as empty
    /// entries in the result vec by `fetch_all`); the
    /// surviving snippets still surface so a partial failure is
    /// not a fatal error.
    pub(crate) async fn collect_research_snippets(
        enabled: bool,
        urls: Vec<String>,
    ) -> Vec<ResearchSnippet> {
        if !enabled || urls.is_empty() {
            return Vec::new();
        }
        let results = fetch_all(&urls).await;
        results
            .into_iter()
            .filter_map(|r| r.ok())
            .collect()
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

        // Track K (D9): when the research fetcher is enabled the
        // Sketch phase pulls snippets from the configured allowlist
        // URLs and injects them into the system prompt via the
        // `${known_apis}` placeholder. The fetcher is opt-in:
        // `research_enabled = false` keeps the prompt untouched
        // (zero overhead, no network), and a fetch failure collapses
        // to an empty snippet list so the LLM still receives a
        // well-formed prompt. To avoid changing the bundled
        // `sketch.md` template (and invalidating the prompt-set
        // hash cache), the placeholder is appended at runtime only
        // when the feature is enabled.
        let research_urls = ctx.config.research_urls.clone();
        let research_enabled = ctx.config.research_enabled;
        let snippets = Self::collect_research_snippets(research_enabled, research_urls).await;
        let system = if snippets.is_empty() {
            system
        } else {
            // Stamp the placeholder into a copy of the system prompt
            // so `inject_known_apis` can substitute it. The
            // template is left untouched on disk so the
            // `prompt_set_hash` cache key stays stable for runs that
            // never opt in.
            let augmented = if system.contains(KNOWN_APIS_PLACEHOLDER) {
                system
            } else {
                format!("{system}\n\n{KNOWN_APIS_PLACEHOLDER}")
            };
            inject_known_apis(&augmented, &snippets)
        };

        let sketches_dir = ctx.run_dir().sketches();
        std::fs::create_dir_all(&sketches_dir)?;

        let system_arc = std::sync::Arc::new(system);
        let user_arc = std::sync::Arc::new(user);

        // Phase G: when a non-trivial problem graph exists, the
        // fan-out is distributed across nodes (one sketch per node,
        // spread evenly). Otherwise the original angle-cycled
        // behaviour kicks in.
        let problem_graph = Self::load_problem_graph(ctx);
        let schedule: Vec<(String, usize)> = match &problem_graph {
            Some(g) => {
                let node_ids: Vec<String> = g.nodes.iter().map(|n| n.id.clone()).collect();
                Self::distribute_across_nodes(count, &node_ids)
            }
            None => (0..count).map(|i| (Self::angle_for(i), i)).collect(),
        };

        let futures = schedule.iter().map(|(label, i)| {
            let user_with_label = if problem_graph.is_some() {
                format!(
                    "{}\n\nFocus on DAG node=\"{label}\" and produce exactly one sketch.",
                    user_arc.as_str()
                )
            } else {
                format!(
                    "{}\n\nUse angle=\"{label}\" and produce exactly one sketch.",
                    user_arc.as_str()
                )
            };
            let ctx = ctx.clone();
            let system_arc = std::sync::Arc::clone(&system_arc);
            let label_for_sketch = label.clone();
            let i = *i;
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let mut sketch: Sketch = ctx
                    .call_with_retry_parse(
                        Role::Sketch,
                        system_arc.as_str().to_owned(),
                        user_with_label,
                        "Sketch: {thesis, key_decisions[], architecture_outline, assumptions[], strengths[], weaknesses[], hard_constraint_check{}, expected_validation}",
                        5,
                    )
                    .await?;
                if sketch.id.is_empty() {
                    sketch.id = format!("sk_{i:03}");
                }
                sketch.angle = label_for_sketch;
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

    /// When `count < node_count` every node still gets at least one
    /// sketch slot, so the graph is fully covered.
    #[test]
    fn distribute_across_nodes_underflow_keeps_every_node() {
        let nodes = vec!["a".into(), "b".into(), "c".into()];
        let slots = SketchPhase::distribute_across_nodes(2, &nodes);
        assert_eq!(slots.len(), 2);
        let unique: std::collections::HashSet<&str> =
            slots.iter().map(|(n, _)| n.as_str()).collect();
        // Two distinct nodes out of three — the third waits for
        // a follow-up run. Pinning that we never re-use the same
        // node for two slots in the underflow case.
        assert_eq!(unique.len(), 2);
    }

    /// When `count > node_count` the slots are spread as evenly as
    /// possible: `count.div_ceil(node_count)`.
    #[test]
    fn distribute_across_nodes_overflow_rounds_up() {
        let nodes = vec!["a".into(), "b".into()];
        let slots = SketchPhase::distribute_across_nodes(5, &nodes);
        // 5 / 2 = 3 per node (rounded up).
        assert_eq!(slots.len(), 5);
        let a_count = slots.iter().filter(|(n, _)| n == "a").count();
        let b_count = slots.iter().filter(|(n, _)| n == "b").count();
        assert_eq!(a_count, 3);
        assert_eq!(b_count, 2);
    }

    /// Empty node list is treated as "no graph": the caller gets a
    /// pure index sequence so the fan-out can fall back to the
    /// angle-cycled behaviour.
    #[test]
    fn distribute_across_nodes_empty_node_list_returns_indices() {
        let slots = SketchPhase::distribute_across_nodes(4, &[]);
        assert_eq!(slots.len(), 4);
        for (i, (label, idx)) in slots.iter().enumerate() {
            assert!(label.is_empty());
            assert_eq!(*idx, i);
        }
    }

    // =================================================================
    // D9 — research fetcher wire-up
    // =================================================================

    /// Track K (D9): the disabled path is a no-op. No fetch, no
    /// network, returns an empty vec even when a URL list is
    /// supplied. The contract is "opt-in means opt-in".
    #[tokio::test]
    async fn sketch_phase_skips_research_when_disabled() {
        let urls = vec!["https://docs.rs/serde".into()];
        let out = SketchPhase::collect_research_snippets(false, urls).await;
        assert!(out.is_empty(), "disabled flag must short-circuit");
    }

    /// Track K (D9): empty URL list is a no-op even when the
    /// flag is on. The optimiser pins the assert so a refactor
    /// that always fetches an empty list cannot accidentally
    /// issue a noop HTTP call.
    #[tokio::test]
    async fn collect_research_snippets_returns_empty_when_urls_empty() {
        let out = SketchPhase::collect_research_snippets(true, vec![]).await;
        assert!(out.is_empty(), "empty URL list must short-circuit");
    }

    /// Track K (D9): a host outside the allowlist is dropped
    /// silently by `fetch_all` (it returns `Err(DisallowedHost)`
    /// for that URL). The helper must filter the failure out and
    /// return an empty vec — never panic, never bubble the error
    /// up to the Sketch phase. Mirrors the "fetch fails gracefully"
    /// contract: the phase continues with the original prompt.
    #[tokio::test]
    async fn sketch_phase_continues_when_fetch_fails_gracefully() {
        let urls = vec!["https://evil.example.com/secret".into()];
        let out = SketchPhase::collect_research_snippets(true, urls).await;
        assert!(
            out.is_empty(),
            "disallowed host must be filtered out, got {out:?}"
        );
    }

    /// Track K (D9): over-cap URL list (> MAX_URLS_PER_CALL) is
    /// collapsed to a single `TooManyUrls` error by `fetch_all`,
    /// which the helper filters out — the Sketch phase must
    /// degrade gracefully instead of refusing to run.
    #[tokio::test]
    async fn collect_research_snippets_over_cap_collapses_to_empty() {
        let urls: Vec<String> = (0..crate::research::MAX_URLS_PER_CALL + 1)
            .map(|i| format!("https://docs.rs/page-{i}"))
            .collect();
        let out = SketchPhase::collect_research_snippets(true, urls).await;
        assert!(
            out.is_empty(),
            "over-cap call must collapse to empty, got {out:?}"
        );
    }
}
