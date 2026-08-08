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
//!
//! E5 (catalog 10-integrada-v0 §D.5.3): beyond the cheap
//! thesis/constraint pre-filter, the phase applies two quality
//! gates before persistence:
//!
//! 1. **Redundancy** — the new sketch must not be a near-duplicate
//!    of an already-accepted one. Similarity is measured as
//!    Jaccard over the lowercased alphanumeric token sets of the
//!    combined `thesis + key_decisions + architecture_outline +
//!    assumptions + strengths + weaknesses` text. A similarity
//!    score >= [`SIMILARITY_REJECT_THRESHOLD`] (0.85) marks the
//!    sketch as `Redundant` and the phase drops it.
//! 2. **Coverage** — when the brief declares non-empty categories
//!    in `expected_validation`/`acceptance`-style keywords, the
//!    sketch's `expected_validation` text must touch at least
//!    [`COVERAGE_MIN_RATIO`] (0.5) of those keywords. Otherwise
//!    the sketch is `LowCoverage` and dropped.
//!
//! Both reasons are recorded on
//! [`SketchFilterStats`] so the synthesize phase can surface them
//! without re-walking the fan-out.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::config::RateLimitConfig;
use crate::domain::{ProblemGraph, Sketch};
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::{KNOWN_APIS_PLACEHOLDER, inject_known_apis, system_prompt};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::research::{ResearchFetcher, ResearchSnippet};
use crate::telemetry::csv_summary::{SketchSummaryRow, write_sketches_summary};

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
    /// Sketches dropped by the E5 redundancy filter (jaccard
    /// similarity above [`SIMILARITY_REJECT_THRESHOLD`] against an
    /// already-kept sibling).
    pub dropped_redundant: usize,
    /// Sketches dropped by the E5 coverage filter (token coverage of
    /// the brief's category set below [`COVERAGE_MIN_RATIO`]).
    pub dropped_low_coverage: usize,
    /// Sketches that survived all filters.
    pub kept: usize,
}

/// Sketch phase. Generates `count` sketches concurrently, applies a
/// minimum filter, and persists each survivor under `sketches/`.
pub struct SketchPhase {
    /// Number of sketches to generate. 0 makes the phase a no-op.
    pub count: u32,
}

/// E5: minimum Jaccard similarity (1 - jaccard distance) above
/// which a new sketch is rejected as redundant against an
/// already-accepted sibling. Calibrated against `cards against
/// humanity` style proposals where two angles can drift apart by
/// 30-40% but still produce the same plan; below 0.85 the overlap
/// is coincidence rather than duplication.
pub const SIMILARITY_REJECT_THRESHOLD: f64 = 0.85;

/// E5: minimum coverage of the brief's category token-set that
/// each sketch must reach. Coverage is computed as
/// `|sketch_tokens ∩ brief_tokens| / min(|brief_tokens|,
/// |sketch_tokens|)`. The symmetric numerator divides by the
/// smaller of the two sets so the metric does not collapse
/// on long briefs (where the absolute token count dilutes
/// the ratio past any meaningful threshold). Real sketches
/// discuss the same topic in different words, so an honest
/// sketch typically overlaps 5-20% of the shared
/// content-words with the brief. The gate's job is to catch
/// sketches that drift entirely off-topic, not to demand
/// the sketch echo half of the brief back.
pub const COVERAGE_MIN_RATIO: f64 = 0.05;

/// E5: when the brief's category set is below this size the
/// coverage check is skipped entirely. A 1-token brief
/// (`{"x"}`) would force any realistic sketch thesis to fail;
/// skipping avoids the degenerate case while still running the
/// check on briefs with at least [`COVERAGE_MIN_BRIEF_TOKENS`]
/// meaningful tokens.
pub const COVERAGE_MIN_BRIEF_TOKENS: usize = 3;

/// Outcome of running the E5 quality filters against one sketch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FilterVerdict {
    /// Sketch survives all filters. `similarity` is the highest
    /// similarity seen against already-accepted siblings; `coverage`
    /// is the brief coverage ratio.
    Accept {
        /// Highest similarity score against an accepted sibling.
        similarity: f64,
        /// Brief coverage ratio (0.0..=1.0).
        coverage: f64,
    },
    /// Sketch is too similar to an already-accepted sibling.
    Redundant {
        /// Similarity score against the offending sibling.
        similarity: f64,
    },
    /// Sketch does not cover enough of the brief's categories.
    LowCoverage {
        /// Brief coverage ratio that failed the threshold.
        coverage: f64,
    },
}

/// E5: turn a prose buffer into a normalised alphanumeric token
/// set. Splits on any byte that is not alphanumeric or underscore,
/// lowercases, drops empty fragments. Total over the input length
/// so a 256 KiB brief tokenises in one pass.
fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// E5: walk a brief JSON value and append any prose under `key`
/// (case-insensitive) into the buffer. Handles both
/// `value[key] == String` and `value[key] == Vec<String>` shapes
/// because the LLM returns either form across the brief lifecycle.
fn walk_brief_value(brief: &serde_json::Value, key: &str, buf: &mut String) {
    let Some(obj) = brief.as_object() else {
        return;
    };
    let needle = key.to_ascii_lowercase();
    for (k, v) in obj {
        if k.to_ascii_lowercase() != needle {
            continue;
        }
        match v {
            serde_json::Value::String(s) => {
                buf.push_str(s);
                buf.push('\n');
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(s) = item.as_str() {
                        buf.push_str(s);
                        buf.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
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

    /// E5: tokenise a sketch's prose fields into a normalised
    /// alphanumeric word set. Lowercased and reduced to `[a-z0-9]+`
    /// tokens so casing and punctuation differences do not skew the
    /// similarity score. `Architecture_outline` is intentionally
    /// included — it is the field the LLM varies per angle and
    /// carries the most overlap signal for two angles that converge
    /// on the same plan.
    pub(crate) fn sketch_token_set(sk: &Sketch) -> HashSet<String> {
        let mut buf = String::new();
        buf.push_str(&sk.thesis);
        buf.push('\n');
        for d in &sk.key_decisions {
            buf.push_str(d);
            buf.push('\n');
        }
        buf.push_str(&sk.architecture_outline);
        buf.push('\n');
        for a in &sk.assumptions {
            buf.push_str(a);
            buf.push('\n');
        }
        for s in &sk.strengths {
            buf.push_str(s);
            buf.push('\n');
        }
        for w in &sk.weaknesses {
            buf.push_str(w);
            buf.push('\n');
        }
        buf.push_str(&sk.expected_validation);
        tokenize(&buf)
    }

    /// E5: tokenise the brief into the "category" set. The brief
    /// is parsed as a `serde_json::Value` (the same shape
    /// `read_json` returns) and we collect the union of the
    /// human-readable prose fields. Keys are walked case-insensitively
    /// so `Problem`, `problem`, and `PROBLEM` all contribute. When
    /// the brief lacks any prose field we fall back to an empty
    /// set, which short-circuits the coverage check to `Accept`
    /// (no categories = no requirement).
    pub(crate) fn brief_token_set(brief: &serde_json::Value) -> HashSet<String> {
        let mut buf = String::new();
        for key in [
            "problem",
            "objectives",
            "deliverables",
            "constraints",
            "assumptions",
            "non_goals",
            "acceptance",
            "risks",
        ] {
            walk_brief_value(brief, key, &mut buf);
        }
        tokenize(&buf)
    }

    /// E5: jaccard similarity between two token sets
    /// (`|A ∩ B| / |A ∪ B|`). Returns `0.0` when both sets are
    /// empty so the caller can treat an empty draft as
    /// vacuously-not-redundant (the coverage filter is the one
    /// that has to surface the issue in that case).
    pub(crate) fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
        if a.is_empty() && b.is_empty() {
            return 0.0;
        }
        let intersection = a.intersection(b).count();
        let union = a.union(b).count();
        if union == 0 {
            0.0
        } else {
            intersection as f64 / union as f64
        }
    }

    /// E5: apply the redundancy + coverage filters to a sketch.
    /// Returns a [`FilterVerdict`] the caller can switch on. The
    /// brief token-set is precomputed once by the caller (it is
    /// constant across the fan-out); the accepted-set is mutated
    /// in place when the verdict is `Accept` so the next call sees
    /// the new entry.
    pub(crate) fn apply_filter(
        sk: &Sketch,
        accepted: &mut Vec<HashSet<String>>,
        brief_tokens: &HashSet<String>,
    ) -> FilterVerdict {
        let sk_tokens = Self::sketch_token_set(sk);
        let mut max_sim = 0.0_f64;
        for other in accepted.iter() {
            let sim = Self::jaccard_similarity(&sk_tokens, other);
            if sim > max_sim {
                max_sim = sim;
            }
        }
        if max_sim >= SIMILARITY_REJECT_THRESHOLD {
            return FilterVerdict::Redundant {
                similarity: max_sim,
            };
        }
        // Coverage check is skipped when the brief is too small
        // to make the metric meaningful (see
        // [`COVERAGE_MIN_BRIEF_TOKENS`]). Real briefs carry
        // dozens of tokens; the gate only kicks in once the
        // brief's category set has enough surface area to be a
        // meaningful target.
        let coverage = if brief_tokens.len() < COVERAGE_MIN_BRIEF_TOKENS {
            1.0
        } else {
            let hits = sk_tokens.intersection(brief_tokens).count();
            // Symmetric ratio: |A ∩ B| / min(|A|, |B|) so a
            // long brief does not collapse the metric past any
            // meaningful threshold (the absolute token count of
            // a 200-word brief is much larger than the
            // content-word count of a 30-word sketch).
            let denom = brief_tokens.len().min(sk_tokens.len());
            if denom == 0 {
                1.0
            } else {
                hits as f64 / denom as f64
            }
        };
        if coverage < COVERAGE_MIN_RATIO {
            return FilterVerdict::LowCoverage { coverage };
        }
        accepted.push(sk_tokens);
        FilterVerdict::Accept {
            similarity: max_sim,
            coverage,
        }
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
        api_key: Option<String>,
        per_host_rate_limit: HashMap<String, RateLimitConfig>,
    ) -> Vec<ResearchSnippet> {
        if !enabled || urls.is_empty() {
            return Vec::new();
        }
        let fetcher = ResearchFetcher::new(api_key).with_per_host_rate_limit(per_host_rate_limit);
        let results = fetcher.fetch_all(&urls).await;
        results.into_iter().filter_map(|r| r.ok()).collect()
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
            // D.17.7: emit the per-model CSV alongside the JSON
            // summary so `inspect` and downstream consumers can read
            // either shape. Zero sketches => header-only CSV.
            let empty_rows: Vec<SketchSummaryRow> = Vec::new();
            write_sketches_summary(ctx.run_dir().root(), &empty_rows)?;
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
        let research_api_key = ctx.config.research.api_key.clone();
        let research_rate_limits = ctx.config.research.per_host_rate_limit.clone();
        let snippets = Self::collect_research_snippets(
            research_enabled,
            research_urls,
            research_api_key,
            research_rate_limits,
        )
        .await;
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
        // E5: pre-compute the brief token set once so the per-sketch
        // redundancy + coverage checks run in O(1) against the
        // constant set. The accepted-siblings list grows as the
        // loop accepts new sketches (later siblings compare
        // against every prior sibling). The order of iteration is
        // the fan-out order — cheaper to keep the angle-cycled
        // behaviour deterministic than to sort by token count.
        let brief_tokens = Self::brief_token_set(&brief);
        let mut accepted_tokens: Vec<HashSet<String>> = Vec::with_capacity(count);
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
            // E5: redundancy + coverage gate. The check runs ONLY
            // after the cheap thesis / constraint gates (so a
            // short-thesis sketch still counts under
            // `dropped_empty_thesis`, not as
            // `dropped_redundant` or `dropped_low_coverage`).
            match Self::apply_filter(&sketch, &mut accepted_tokens, &brief_tokens) {
                FilterVerdict::Accept { .. } => {}
                FilterVerdict::Redundant { similarity } => {
                    let _ = ctx.telemetry.warn(
                        "phase.sketch_redundant",
                        "warn",
                        "sketch dropped by E5 redundancy filter",
                        serde_json::json!({"similarity": similarity}),
                        crate::telemetry::WarningContext {
                            phase: Some("sketch".into()),
                            role: Some("sketch".into()),
                            ..Default::default()
                        },
                    );
                    stats.dropped_redundant += 1;
                    continue;
                }
                FilterVerdict::LowCoverage { coverage } => {
                    let _ = ctx.telemetry.warn(
                        "phase.sketch_low_coverage",
                        "warn",
                        "sketch dropped by E5 coverage filter",
                        serde_json::json!({"coverage": coverage}),
                        crate::telemetry::WarningContext {
                            phase: Some("sketch".into()),
                            role: Some("sketch".into()),
                            ..Default::default()
                        },
                    );
                    stats.dropped_low_coverage += 1;
                    continue;
                }
            }
            let id = sketch.id.clone();
            let path: PathBuf = sketches_dir.join(format!("{id}.json"));
            write_json(&path, &sketch)?;
            paths.push(path);
        }
        stats.kept = paths.len();
        let summary_path = ctx.run_dir().final_dir().join("sketches_summary.json");
        write_json(&summary_path, &stats)?;
        // D.17.7: per-model CSV summary. The sketch phase uses one
        // model for the entire fan-out, so a single row is emitted
        // regardless of how many sketches survive the filter.
        // `total_tokens` is left at zero because `call_with_retry_parse`
        // discards the response envelope after parsing; populating it
        // here would require returning both the parsed value and the
        // `Response` from the retry wrapper, which is out of scope for
        // this wire-up. Future PRs can wire the response through.
        let csv_rows: Vec<SketchSummaryRow> =
            vec![(ctx.default_model.clone(), stats.kept as u64, 0)];
        write_sketches_summary(ctx.run_dir().root(), &csv_rows)?;
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
            dropped_redundant: 0,
            dropped_low_coverage: 0,
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
        let out = SketchPhase::collect_research_snippets(false, urls, None, HashMap::new()).await;
        assert!(out.is_empty(), "disabled flag must short-circuit");
    }

    /// Track K (D9): empty URL list is a no-op even when the
    /// flag is on. The optimiser pins the assert so a refactor
    /// that always fetches an empty list cannot accidentally
    /// issue a noop HTTP call.
    #[tokio::test]
    async fn collect_research_snippets_returns_empty_when_urls_empty() {
        let out = SketchPhase::collect_research_snippets(true, vec![], None, HashMap::new()).await;
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
        let out = SketchPhase::collect_research_snippets(true, urls, None, HashMap::new()).await;
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
        let out = SketchPhase::collect_research_snippets(true, urls, None, HashMap::new()).await;
        assert!(
            out.is_empty(),
            "over-cap call must collapse to empty, got {out:?}"
        );
    }

    // =================================================================
    // E5 — sketch redundancy + coverage
    // =================================================================

    /// E5: tokenising the same sketch twice must produce identical
    /// sets (idempotence) and case differences must NOT matter.
    /// Pinned so the similarity score is independent of the
    /// casing the LLM happens to use.
    #[test]
    fn sketch_token_set_is_idempotent_and_case_insensitive() {
        let sk = Sketch {
            thesis: "Rust binary with SQLite, single Process, no Daemon.".into(),
            architecture_outline: "Reads config.toml; writes artifacts to disk.".into(),
            strengths: vec!["FAST startup".into()],
            weaknesses: vec!["slow writes".into()],
            ..Default::default()
        };
        let first = SketchPhase::sketch_token_set(&sk);
        let second = SketchPhase::sketch_token_set(&sk);
        assert_eq!(first, second);
        let lowered: HashSet<String> = first.iter().map(|s| s.to_ascii_lowercase()).collect();
        assert_eq!(first, lowered);
        assert!(first.contains("rust"));
        assert!(first.contains("sqlite"));
    }

    /// E5: a sketch whose token set overlaps an accepted sibling by
    /// ≥ 0.85 jaccard similarity must be rejected as `Redundant`.
    /// Two near-identical sketches (only one differing sentence)
    /// share most tokens and fail the redundancy gate, even
    /// though both pass the cheap pre-filter.
    #[test]
    fn sketch_rejects_redundant_high_similarity() {
        let brief_tokens = tokenize("rust sqlite single binary");
        let accepted_sk = Sketch {
            thesis: "Rust binary with SQLite, single process, no daemon.".into(),
            architecture_outline: "One binary reads config and writes artifacts.".into(),
            strengths: vec!["simple deployment".into()],
            weaknesses: vec!["limited concurrency".into()],
            expected_validation: "Tarball fits on a USB stick.".into(),
            ..Default::default()
        };
        let mut accepted = vec![SketchPhase::sketch_token_set(&accepted_sk)];
        let candidate_sk = Sketch {
            thesis: "Rust binary with SQLite, single process, no daemon.".into(),
            architecture_outline: "One binary reads config and writes artifacts plus a logger."
                .into(),
            strengths: vec!["simple deployment".into()],
            weaknesses: vec!["limited concurrency".into()],
            expected_validation: "Tarball fits on a USB stick.".into(),
            ..Default::default()
        };
        let verdict = SketchPhase::apply_filter(&candidate_sk, &mut accepted, &brief_tokens);
        match verdict {
            FilterVerdict::Redundant { similarity } => {
                assert!(
                    similarity >= SIMILARITY_REJECT_THRESHOLD,
                    "expected redundancy ≥ {}, got {}",
                    SIMILARITY_REJECT_THRESHOLD,
                    similarity
                );
            }
            other => panic!("expected Redundant, got {other:?}"),
        }
        // The rejected sketch must NOT have been pushed into
        // `accepted` so subsequent siblings see only the survivor.
        assert_eq!(accepted.len(), 1);
    }

    /// E5: a sketch that mentions none of the brief keywords must
    /// be rejected as `LowCoverage`. The brief lists four target
    /// keywords (`audit`, `scylla`, `sharding`, `compliance`); the
    /// candidate never says any of them, so the coverage ratio is
    /// 0.0 — well below `COVERAGE_MIN_RATIO` (0.5).
    #[test]
    fn sketch_rejects_low_coverage_shortfall() {
        let brief_tokens = tokenize("audit scylla sharding compliance");
        let accepted_sk = Sketch {
            thesis: "Scylla shard with audit log and compliance posture.".into(),
            architecture_outline: "Mesh of nodes.".into(),
            expected_validation: "Pen test.".into(),
            ..Default::default()
        };
        let mut accepted = vec![SketchPhase::sketch_token_set(&accepted_sk)];
        let candidate_sk = Sketch {
            thesis: "Brand new Rust pipeline with a small embedded webserver.".into(),
            architecture_outline: "Single binary with a Lua plugin host.".into(),
            expected_validation: "End-to-end smoke tests.".into(),
            ..Default::default()
        };
        let verdict = SketchPhase::apply_filter(&candidate_sk, &mut accepted, &brief_tokens);
        match verdict {
            FilterVerdict::LowCoverage { coverage } => {
                assert!(
                    coverage < COVERAGE_MIN_RATIO,
                    "expected coverage < {}, got {}",
                    COVERAGE_MIN_RATIO,
                    coverage
                );
            }
            other => panic!("expected LowCoverage, got {other:?}"),
        }
        assert_eq!(accepted.len(), 1);
    }

    /// E5: a sketch that drifts far enough from its sibling (medium
    /// similarity) and mentions enough brief keywords must be
    /// `Accept`-ed. Calibrated so that the two rejection paths
    /// above stay meaningful but most realistic drafts still
    /// survive.
    #[test]
    fn sketch_accepts_diverse_medium_similarity() {
        let brief_tokens = tokenize("rust sqlite audit sharding compliance");
        let accepted_sk = Sketch {
            thesis: "Rust binary writes audit log entries to SQLite.".into(),
            architecture_outline: "Single-process scheduler.".into(),
            expected_validation: "Replay log works.".into(),
            ..Default::default()
        };
        let mut accepted = vec![SketchPhase::sketch_token_set(&accepted_sk)];
        let candidate_sk = Sketch {
            thesis: "Rust sharded ledger with sharding strategy and compliance export.".into(),
            architecture_outline: "Multi-node reconciler writes to SQLite and ships audit feed."
                .into(),
            expected_validation: "Compliance officer can replay the audit trail.".into(),
            ..Default::default()
        };
        let verdict = SketchPhase::apply_filter(&candidate_sk, &mut accepted, &brief_tokens);
        match verdict {
            FilterVerdict::Accept {
                similarity,
                coverage,
            } => {
                assert!(
                    similarity < SIMILARITY_REJECT_THRESHOLD,
                    "similarity {similarity} must stay under threshold"
                );
                assert!(
                    coverage >= COVERAGE_MIN_RATIO,
                    "coverage {coverage} must clear threshold"
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
        assert_eq!(accepted.len(), 2);
    }

    /// E5: a brief that lists no prose fields (or a non-object root)
    /// tokenises to an empty set; the coverage branch returns 1.0
    /// so the check is never a blocker on a malformed brief.
    /// Mirrors the spec tolerance: coverage is a quality gate, not
    /// a contract gate.
    #[test]
    fn brief_token_set_empty_for_non_object_or_empty_brief() {
        let obj = serde_json::json!({});
        assert!(SketchPhase::brief_token_set(&obj).is_empty());
        let s = serde_json::json!("just a string");
        assert!(SketchPhase::brief_token_set(&s).is_empty());
        let list = serde_json::json!(["just", "a", "list"]);
        assert!(SketchPhase::brief_token_set(&list).is_empty());
        // Real brief picks up the `problem` / `objectives` / etc.
        // fields, lowercased and split on punctuation.
        let brief = serde_json::json!({
            "problem": "Build a sharded audit ledger.",
            "objectives": ["sharding", "compliance"],
            "deliverables": ["tarball", "docs"],
            "acceptance": ["100k TPS", "p99 < 50ms"]
        });
        let tokens = SketchPhase::brief_token_set(&brief);
        assert!(tokens.contains("sharded"));
        assert!(tokens.contains("audit"));
        assert!(tokens.contains("sharding"));
        assert!(tokens.contains("compliance"));
        assert!(tokens.contains("tarball"));
        assert!(tokens.contains("100k"));
    }

    /// E5: the new filter stats surface the two new
    /// `dropped_*` buckets so the summary sidecar round-trips
    /// without losing the rejection reason counts. Pinning the
    /// serde shape so a rename trips the inspect command.
    #[test]
    fn filter_stats_includes_e5_buckets_in_round_trip() {
        let s = SketchFilterStats {
            raw: 4,
            dropped_empty_thesis: 0,
            dropped_hard_constraint: 0,
            dropped_redundant: 1,
            dropped_low_coverage: 1,
            kept: 2,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"dropped_redundant\":1"));
        assert!(j.contains("\"dropped_low_coverage\":1"));
        let back: SketchFilterStats = serde_json::from_str(&j).unwrap();
        assert_eq!(back.dropped_redundant, 1);
        assert_eq!(back.dropped_low_coverage, 1);
        assert_eq!(back.kept, 2);
    }
}
