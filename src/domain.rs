//! Domain types — every JSON shape the phases read or write is defined
//! here. The fields are intentionally permissive: every LLM role is
//! allowed to surface extra information as long as the contract keys
//! are present.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::RunId;

/// Output of the intake phase.
///
/// All fields are lenient: `#[serde(default)]` lets missing fields
/// become empty, and unknown fields from the model are silently
/// ignored. The `MiniMax-M3` model with `thinking` enabled routinely
/// mixes `Intake` and `Brief` fields in the same response, so the
/// schema must tolerate that.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Intake {
    /// The user's problem statement, rephrased by the LLM.
    pub problem: String,
    /// Concrete objectives extracted from the prompt.
    pub objectives: Vec<String>,
    /// Constraints (hard or soft) the user mentioned.
    pub constraints: Vec<String>,
    /// Non-goals explicitly or implicitly stated.
    pub non_goals: Vec<String>,
    /// Open questions the LLM flagged for clarification.
    pub open_questions: Vec<String>,
    /// Verbatim user prompt.
    pub raw_prompt: String,
}

/// Output of the clarify phase — the canonical brief.
///
/// Same leniency as `Intake`. `acceptance` is a `Vec<String>` because
/// the model returns it as a list, not a single string.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Brief {
    /// Concrete problem.
    pub problem: String,
    /// Concrete objectives.
    pub objectives: Vec<String>,
    /// Concrete deliverables.
    pub deliverables: Vec<String>,
    /// Hard and soft constraints.
    pub constraints: Vec<String>,
    /// Assumptions the team is making.
    pub assumptions: Vec<String>,
    /// Explicit non-goals.
    pub non_goals: Vec<String>,
    /// Acceptance criteria (one bullet per item; the model emits a list).
    pub acceptance: Vec<String>,
    /// Known risks.
    pub risks: Vec<String>,
}

/// Output of the route phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    /// Mode name (`"fast"` or `"standard"`).
    pub mode: String,
    /// Human-readable reason.
    pub reason: String,
    /// Number of sketches the router wants.
    pub sketches: u32,
    /// Number of proposals to generate.
    pub proposals: u32,
    /// Number of judges to consult.
    pub judges: u32,
}

/// Output of the propose phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Proposal {
    /// Stable id (e.g. `"p_001"`).
    pub id: String,
    /// One-line summary.
    pub summary: String,
    /// Detailed approach.
    pub approach: String,
    /// Trade-offs considered.
    pub tradeoffs: Vec<String>,
    /// Evidence backing the proposal.
    pub evidence: Vec<String>,
    /// Sketch id this proposal was derived from (`"sk_xxx"`). Empty
    /// for `fast` mode where the sketch phase is skipped. Filled by
    /// `ProposePhase` so the deliver / inspect surface can show the
    /// lineage even after the artefacts are flattened.
    pub source_sketch: String,
    /// Code artefacts attached to this proposal. `ProposePhase`
    /// extracts fenced ```rust / ```python / ```typescript /
    /// ```ts blocks out of the model's `approach` field and stores
    /// them here so the validate phase can hand each one to the
    /// matching language validator. Empty when the proposal has
    /// no executable code (the common case for architecture-only
    /// proposals).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub artifacts: Vec<crate::validators::CodeArtifact>,
}

/// Output of the sketch phase — a short, opinionated exploration
/// artefact produced by the `sketcher` role (T01-06 §5.5). Each sketch
/// is a self-contained 400-800 token hypothesis that does NOT see
/// other sketches; isolation prevents premature convergence across the
/// fan-out.
///
/// Leniency: every field is `#[serde(default)]` so a model that omits
/// `weaknesses` or `expected_validation` (common with `MiniMax-M3`
/// when it treats the field as optional) still parses. The
/// `hard_constraint_check` map is permissive about its value type so
/// the model can return either `true`/`false` or a richer verdict
/// string.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sketch {
    /// Stable id (`"sk_<uuid7>"`), assigned by `SketchPhase` after
    /// the LLM call returns. Not part of the LLM contract; the
    /// `SketchPhase` fills it before persistence.
    pub id: String,
    /// One-sentence thesis the rest of the sketch defends.
    pub thesis: String,
    /// Key architectural decisions (3-6 entries typically).
    pub key_decisions: Vec<String>,
    /// Architectural outline in prose (50-2000 chars).
    pub architecture_outline: String,
    /// Assumptions the sketch relies on.
    pub assumptions: Vec<String>,
    /// Strengths.
    pub strengths: Vec<String>,
    /// Weaknesses (honest accounting; used by the selection step).
    pub weaknesses: Vec<String>,
    /// Hard-constraint check: constraint_id → passes? Map keyed by
    /// the brief's hard-constraint identifier (or a free-form label
    /// when the model cannot enumerate them).
    pub hard_constraint_check: std::collections::BTreeMap<String, bool>,
    /// What kind of evidence would falsify this sketch.
    pub expected_validation: String,
    /// Model angle used (e.g. `"minimalist"`, `"pragmatic"`,
    /// `"production-grade"`, `"security-first"`). Set by
    /// `SketchPhase` from the fan-out schedule, NOT by the model —
    /// helps the `epistemic_legacy` aggregator recognise duplicates.
    pub angle: String,
}

/// Output of the gate phase (one per proposal).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    /// Did the proposal pass the structural check?
    pub pass: bool,
    /// Issues found.
    pub issues: Vec<String>,
    /// Missing fields.
    pub missing: Vec<String>,
}

/// Output of the critique phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Critique {
    /// Verdict.
    pub verdict: String,
    /// Issues found.
    pub issues: Vec<String>,
    /// Suggestions.
    pub suggestions: Vec<String>,
}

/// Output of the repair phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Repair {
    /// Repaired proposal id.
    pub id: String,
    /// One-line summary.
    pub summary: String,
    /// Detailed approach.
    pub approach: String,
    /// Trade-offs.
    pub tradeoffs: Vec<String>,
    /// Evidence.
    pub evidence: Vec<String>,
    /// What changed vs the original.
    pub changes: Vec<String>,
}

/// Output of the judge phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgeScore {
    /// Overall score (0..=10).
    pub score: f32,
    /// Per-criterion breakdown.
    pub criteria: JudgeCriteria,
    /// Free-form comments.
    pub comments: String,
}

/// Per-criterion breakdown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgeCriteria {
    /// 0..=10.
    pub correctness: f32,
    /// 0..=10.
    pub completeness: f32,
    /// 0..=10.
    pub fit: f32,
    /// 0..=10.
    pub evidence: f32,
    /// 0..=10.
    pub clarity: f32,
}

/// Output of the rank phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ranking {
    /// Ranked proposals (highest first).
    pub ranked: Vec<RankEntry>,
    /// Diversity-preserving top-3 representatives (Pareto front →
    /// cluster → crowding). The deliver phase consumes this list;
    /// when empty (front smaller than 3) it falls back to `ranked`.
    pub representatives: Vec<RankEntry>,
    /// Winning proposal id.
    pub winner: String,
}

/// One entry in the ranking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RankEntry {
    /// Proposal id.
    pub id: String,
    /// Score.
    pub score: f32,
    /// Human-readable reason.
    pub reason: String,
}

/// Output of the deliver phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FinalReport {
    /// Title.
    pub title: String,
    /// Summary.
    pub summary: String,
    /// Recommendation.
    pub recommendation: String,
    /// Alternative candidates.
    pub alternatives: Vec<String>,
    /// Next steps.
    pub next_steps: Vec<String>,
}

/// Top-level run manifest, written to `manifest.json` per T01-06 §33.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version.
    pub schema_version: String,
    /// Run id.
    pub run_id: RunId,
    /// Mode name.
    pub mode: String,
    /// Status.
    pub status: String,
    /// Created at.
    pub created_at: DateTime<Utc>,
    /// Updated at.
    pub updated_at: DateTime<Utc>,
    /// Client version.
    pub client_version: String,
    /// SHA-256 of the brief (export).
    pub brief_sha256: String,
    /// BLAKE3 of the brief (internal).
    pub brief_blake3: String,
    /// Provider used.
    pub provider: String,
    /// Model used.
    pub model: String,
    /// Per-phase history.
    pub phases: Vec<ManifestPhase>,
    /// Aggregate token usage.
    pub usage: ManifestUsage,
    /// Manifest hash (BLAKE3 over canonical JSON minus this field).
    pub manifest_blake3: String,
}

/// One row in `Manifest.phases`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestPhase {
    /// Phase name.
    pub phase: String,
    /// Start unix seconds.
    pub started_unix: i64,
    /// End unix seconds.
    pub ended_unix: i64,
    /// Status (`"end"`, `"error"`, `"cancelled"`).
    pub status: String,
    /// Calls performed.
    pub calls: u32,
    /// Error message if any.
    pub error: Option<String>,
}

/// Token usage rollup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestUsage {
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total cache-read tokens.
    pub cache_read: u64,
    /// Total cache-creation tokens.
    pub cache_creation: u64,
}

// =====================================================================
// Discovery (Plan B sub-phase B) — domain types.
//                                                See V4 §6.5–§6.10 and
// proposal-02-rust.md §9.4–§9.10.
// =====================================================================

/// Output of the discovery tagger phase. One per sketch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SketchTags {
    /// Sketch id (`sk_<uuid7>`).
    pub sketch_id: String,
    /// Primary category (e.g. "auth", "storage", "deployment").
    pub primary: String,
    /// Secondary categories (free-form).
    pub secondary: Vec<String>,
    /// Subcategory inside the primary (e.g. "session-mgmt").
    pub subcategory: String,
    /// Difficulty — `"low"`, `"medium"`, or `"high"`.
    pub difficulty: String,
    /// Cosine-like similarity score against the primary category's
    /// centroid (0..=1). Below `0.6` the sketch is bucketed as
    /// `uncategorized` (V4 §6.5).
    pub similarity_to_category: f32,
    /// Optional free-form notes from the tagger.
    pub notes: String,
    /// Schema version. Always `"v1"` for v0.2.
    pub schema_version: String,
}

/// Output of the discovery cluster phase. One per cluster.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cluster {
    /// Stable cluster id (`cluster_<NN>`).
    pub id: String,
    /// Human-readable label produced by the LLM refinement pass.
    pub label: String,
    /// Short summary produced by the LLM refinement pass.
    pub summary: String,
    /// Projected category id (filled by the integrator phase).
    pub category_id: String,
    /// Sketch ids that belong to this cluster.
    pub members: Vec<String>,
    /// SimHash centroid (hex). Optional, only present when the
    /// SimHash refinement produced one.
    pub centroid_simhash: String,
    /// Mean intra-cluster similarity score (0..=1).
    pub cohesion: f32,
    /// Schema version.
    pub schema_version: String,
}

/// Output of the discovery contradiction phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Contradiction {
    /// Stable id (`c_<NN>`).
    pub id: String,
    /// Cluster id on the "a" side of the contradiction.
    pub cluster_a: String,
    /// Cluster id on the "b" side of the contradiction.
    pub cluster_b: String,
    /// Sketch ids that triggered the contradiction (drawn from
    /// `cluster_a` and `cluster_b`).
    pub representatives: Vec<String>,
    /// Topic of the contradiction (e.g. "consistency", "deployment").
    pub topic: String,
    /// Human description of the disagreement.
    pub description: String,
    /// Severity: `"low"`, `"medium"`, `"high"`.
    pub severity: String,
    /// Schema version.
    pub schema_version: String,
}

/// A single facet for a category.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Facet {
    /// Stable id (kebab-case slug).
    pub id: String,
    /// Human description.
    pub description: String,
    /// `true` when the facet must appear in the final document.
    pub required: bool,
}

/// Facet list for a category. Cached per `sha256(brief + category_id)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FacetList {
    /// Category id (`cat_<NN>`).
    pub category_id: String,
    /// Cluster id this category was derived from.
    pub cluster_id: String,
    /// Facets.
    pub facets: Vec<Facet>,
    /// SHA-256 of `brief.json + category_id` (cache key).
    pub cache_key: String,
    /// Created unix seconds.
    pub created_unix: i64,
    /// Schema version.
    pub schema_version: String,
}

/// One extracted markdown section for a facet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FacetExtraction {
    /// Facet id.
    pub facet_id: String,
    /// Category id.
    pub category_id: String,
    /// Markdown body.
    pub body: String,
    /// Source sketch ids that contributed.
    pub sources: Vec<String>,
    /// Schema version.
    pub schema_version: String,
}

/// Final integrated document for a category.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CategoryDoc {
    /// Category id (`cat_<NN>`).
    pub category_id: String,
    /// Cluster id this category descended from.
    pub cluster_id: String,
    /// Markdown body.
    pub body: String,
    /// Source sketch ids.
    pub sources: Vec<String>,
    /// Density score (members / max_members). Higher = larger cluster.
    pub density: f32,
    /// Schema version.
    pub schema_version: String,
}

/// `uncategorized.md` payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UncategorizedDoc {
    /// Number of sketches that landed in this bucket.
    pub count: usize,
    /// Markdown body.
    pub body: String,
    /// Schema version.
    pub schema_version: String,
}

/// `summary.md` payload — overall executive index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverySummary {
    /// Run id.
    pub run_id: RunId,
    /// Total sketches generated.
    pub total_sketches: usize,
    /// Number of categories produced.
    pub category_count: usize,
    /// Number of uncategorized sketches.
    pub uncategorized_count: usize,
    /// Categories ordered by density (descending).
    pub categories_by_density: Vec<String>,
    /// Top-level executive summary in markdown.
    pub executive_summary: String,
    /// Schema version.
    pub schema_version: String,
}

// =====================================================================
// Phase D (Plan B sub-phase D) — domain types.
//                                       See V4 §5.12, §5.13 and
// proposal-02-rust.md §6.5, §8.4, §16.11.
// =====================================================================

/// Output of the synthesize phase. One per proposal cluster that
/// triggered synthesis. The integrator LLM role is reused here to
/// merge the cluster's proposals into one "best version"; the
/// synthesized proposal then competes against its sources per V4 §5.13.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SynthesizedProposal {
    /// Stable id (`s_<NN>`).
    pub id: String,
    /// Source proposal ids that fed this synthesis.
    pub source_proposals: Vec<String>,
    /// Cluster id (also used to keep the file name stable across
    /// re-runs of the same brief).
    pub cluster_id: String,
    /// Synthesis strategy. The integrator role describes what kind
    /// of merge it performed: `merge_invariants`, `pick_strongest`,
    /// `concatenate_disjoint_sections`, etc.
    pub synthesis_strategy: String,
    /// Integrated proposal summary.
    pub summary: String,
    /// Integrated proposal approach (markdown).
    pub approach: String,
    /// Trade-offs inherited from the sources.
    pub tradeoffs: Vec<String>,
    /// Evidence (sketches / critiques / external refs).
    pub evidence: Vec<String>,
    /// Source proposal ids again, kept explicit for consumers that
    /// only want the lineage.
    pub sources: Vec<String>,
    /// Unix seconds when this file was written.
    pub created_unix: i64,
    /// Schema version.
    pub schema_version: String,
}

/// Output of the adversarial judge pass. Only emitted when the
/// disagreement_score between normal judges exceeds the configured
/// threshold; otherwise the proposal is left alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AdversaryReport {
    /// Proposal id this report is about.
    pub proposal_id: String,
    /// Did the adversary find a hidden weakness?
    pub consensus_check: String,
    /// Disagreement score that triggered the adversary (0..=10).
    pub disagreement_score: f32,
    /// Free-form weaknesses the adversary surfaced.
    pub weaknesses: Vec<String>,
    /// Claims the adversary considers under-verified.
    pub unverified_claims: Vec<String>,
    /// Score delta applied to the aggregated evaluation. Negative
    /// pulls the proposal down; positive boosts it. Range -2..=+2.
    pub score_delta: f32,
    /// Short rationale.
    pub rationale: String,
    /// Schema version.
    pub schema_version: String,
}

/// A persisted human checkpoint. `kind` follows the proposal-01 §6.5
/// list (`intake`, `clarify`, `final`, `custom`); the question and the
/// raw response are captured verbatim so the run remains reproducible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HumanCheckpoint {
    /// Checkpoint id (`h_<NN>`). Stable across re-runs.
    pub id: String,
    /// Phase that asked (`intake`, `clarify`, `final`, `custom`).
    pub phase: String,
    /// Kind, mirroring the SQLite enum: `intake | clarify | final | custom`.
    pub kind: String,
    /// Question shown to the user.
    pub question: String,
    /// Raw response captured from stdin (always a single line, no
    /// trailing newline).
    pub response: String,
    /// Unix seconds at the time of capture.
    pub at_unix: i64,
    /// Optional default the user accepted when they typed nothing.
    pub accepted_default: bool,
    /// Schema version.
    pub schema_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_round_trips() {
        let b = Brief {
            problem: "x".into(),
            objectives: vec!["a".into()],
            deliverables: vec!["d".into()],
            constraints: vec!["c".into()],
            assumptions: vec![],
            non_goals: vec![],
            acceptance: vec!["ok".into()],
            risks: vec!["r".into()],
        };
        let j = serde_json::to_string(&b).unwrap();
        let back: Brief = serde_json::from_str(&j).unwrap();
        assert_eq!(back.problem, "x");
    }

    /// Each LLM-output struct must accept an empty JSON object without
    /// failing the parse. The model frequently omits optional fields
    /// (especially `comments` on JudgeScore), and we tolerate that by
    /// defaulting to zero/empty values rather than hard-erroring.
    #[test]
    fn empty_object_parses_as_default_for_all_output_types() {
        let _: Proposal = serde_json::from_str("{}").unwrap();
        let _: Gate = serde_json::from_str("{}").unwrap();
        let _: Critique = serde_json::from_str("{}").unwrap();
        let _: Repair = serde_json::from_str("{}").unwrap();
        let _: JudgeScore = serde_json::from_str("{}").unwrap();
        let _: JudgeCriteria = serde_json::from_str("{}").unwrap();
        let _: Ranking = serde_json::from_str("{}").unwrap();
        let _: RankEntry = serde_json::from_str("{}").unwrap();
        let _: FinalReport = serde_json::from_str("{}").unwrap();
        let _: Route = serde_json::from_str("{}").unwrap();
        let _: Sketch = serde_json::from_str("{}").unwrap();
    }

    /// Sketch round-trips a realistic payload and preserves every
    /// field including the `BTreeMap` of hard-constraint verdicts.
    #[test]
    fn sketch_round_trips() {
        let payload = serde_json::json!({
            "thesis": "Use Rust + SQLite + a single binary.",
            "key_decisions": ["single binary", "SQLite only"],
            "architecture_outline": "CLI binary that owns SQLite, telemetry, and the agent registry.",
            "assumptions": ["users are comfortable with one process per run"],
            "strengths": ["simple deployment"],
            "weaknesses": ["no horizontal scaling"],
            "hard_constraint_check": {"no_serverless": true, "no_jvm": true},
            "expected_validation": "Build a 1k-line Rust crate that compiles in <2s and runs a fast smoke.",
            "angle": "minimalist",
            "id": "sk_001"
        });
        let sk: Sketch = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(sk.thesis, "Use Rust + SQLite + a single binary.");
        assert_eq!(sk.angle, "minimalist");
        assert_eq!(sk.hard_constraint_check.len(), 2);
        assert!(sk.hard_constraint_check["no_serverless"]);
        let back = serde_json::to_value(&sk).unwrap();
        assert_eq!(back["id"], "sk_001");
        assert_eq!(back["angle"], "minimalist");
    }

    /// When the model returns only the LLM-visible fields (no
    /// `id`/`angle`), `SketchPhase` fills them post-parse. This test
    /// pins the assumption: the LLM payload is enough on its own.
    #[test]
    fn sketch_minimal_payload_parses() {
        let payload = serde_json::json!({
            "thesis": "tiny thesis",
            "key_decisions": ["k1", "k2"],
            "architecture_outline": "outlined.",
            "assumptions": [],
            "strengths": ["s1"],
            "weaknesses": ["w1"],
            "hard_constraint_check": {},
            "expected_validation": "ev"
        });
        let sk: Sketch = serde_json::from_value(payload).unwrap();
        assert_eq!(sk.id, "");
        assert_eq!(sk.angle, "");
    }

    // -- Discovery types (Plan B sub-phase B) ---------------------------

    /// Every discovery struct must accept an empty JSON object.
    /// LLM calls return `{}` when the model skips the role, and the
    /// pipeline must keep going without panicking.
    #[test]
    fn empty_object_parses_for_discovery_types() {
        let _: SketchTags = serde_json::from_str("{}").unwrap();
        let _: Cluster = serde_json::from_str("{}").unwrap();
        let _: Contradiction = serde_json::from_str("{}").unwrap();
        let _: Facet = serde_json::from_str("{}").unwrap();
        let _: FacetList = serde_json::from_str("{}").unwrap();
        let _: FacetExtraction = serde_json::from_str("{}").unwrap();
        let _: CategoryDoc = serde_json::from_str("{}").unwrap();
        let _: UncategorizedDoc = serde_json::from_str("{}").unwrap();
        let _: DiscoverySummary = serde_json::from_str("{}").unwrap();
    }

    /// SketchTags round-trips a realistic payload produced by the
    /// tagger role. The similarity score is preserved verbatim so
    /// the integrator can decide on `uncategorized` based on a
    /// plain number comparison.
    #[test]
    fn sketch_tags_round_trip() {
        let payload = serde_json::json!({
            "sketch_id": "sk_001",
            "primary": "auth",
            "secondary": ["session-mgmt", "rbac"],
            "subcategory": "session-mgmt",
            "difficulty": "medium",
            "similarity_to_category": 0.82,
            "notes": "uses JWT and short-lived tokens",
            "schema_version": "v1"
        });
        let tags: SketchTags = serde_json::from_value(payload).unwrap();
        assert_eq!(tags.primary, "auth");
        assert_eq!(tags.subcategory, "session-mgmt");
        assert!((tags.similarity_to_category - 0.82).abs() < 1e-6);
        let back = serde_json::to_value(&tags).unwrap();
        assert_eq!(back["sketch_id"], "sk_001");
    }

    /// Cluster carries the LLM refinement label and a list of
    /// member sketch ids. The `centroid_simhash` field is optional
    /// so the integrator can still emit a document when the
    /// refinement pass skipped it.
    #[test]
    fn cluster_round_trip() {
        let c = Cluster {
            id: "cluster_01".into(),
            label: "auth strategies".into(),
            summary: "Three sketches propose JWT-based auth.".into(),
            category_id: String::new(),
            members: vec!["sk_001".into(), "sk_004".into()],
            centroid_simhash: String::new(),
            cohesion: 0.75,
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: Cluster = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, "cluster_01");
        assert_eq!(back.members.len(), 2);
    }

    /// Contradiction preserves cluster_a/b and the severity.
    #[test]
    fn contradiction_round_trip() {
        let c = Contradiction {
            id: "c_01".into(),
            cluster_a: "cluster_01".into(),
            cluster_b: "cluster_05".into(),
            representatives: vec!["sk_001".into(), "sk_022".into()],
            topic: "consistency".into(),
            description: "ACID vs eventual".into(),
            severity: "high".into(),
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: Contradiction = serde_json::from_str(&j).unwrap();
        assert_eq!(back.severity, "high");
        assert_eq!(back.cluster_a, "cluster_01");
    }

    /// SketchTags tolerates the LLM omitting optional fields.
    #[test]
    fn sketch_tags_partial_payload_parses() {
        let payload = serde_json::json!({
            "sketch_id": "sk_002",
            "primary": "uncategorized",
            "difficulty": "low"
        });
        let tags: SketchTags = serde_json::from_value(payload).unwrap();
        assert_eq!(tags.primary, "uncategorized");
        assert_eq!(tags.subcategory, "");
        assert!(tags.secondary.is_empty());
    }

    /// Facet keeps the slug, description, and required flag.
    #[test]
    fn facet_round_trip() {
        let f = Facet {
            id: "flujos".into(),
            description: "Data flow sequences".into(),
            required: true,
        };
        let j = serde_json::to_string(&f).unwrap();
        let back: Facet = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, "flujos");
        assert!(back.required);
    }

    /// FacetList preserves the cache key + facets.
    #[test]
    fn facet_list_round_trip() {
        let fl = FacetList {
            category_id: "cat_01".into(),
            cluster_id: "cluster_01".into(),
            cache_key: "deadbeef".into(),
            created_unix: 1_700_000_000,
            schema_version: "v1".into(),
            facets: vec![Facet {
                id: "flujos".into(),
                description: "Data flows".into(),
                required: true,
            }],
        };
        let j = serde_json::to_string(&fl).unwrap();
        let back: FacetList = serde_json::from_str(&j).unwrap();
        assert_eq!(back.facets.len(), 1);
        assert_eq!(back.cache_key, "deadbeef");
    }

    /// CategoryDoc round-trips with markdown body.
    #[test]
    fn category_doc_round_trip() {
        let d = CategoryDoc {
            category_id: "cat_01".into(),
            cluster_id: "cluster_01".into(),
            body: "# Auth\n\n...long markdown...".into(),
            sources: vec!["sk_001".into()],
            density: 0.42,
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&d).unwrap();
        let back: CategoryDoc = serde_json::from_str(&j).unwrap();
        assert!(back.body.contains("Auth"));
        assert!((back.density - 0.42).abs() < 1e-6);
    }

    /// DiscoverySummary preserves the run id and the category
    /// ordering by density.
    #[test]
    fn discovery_summary_round_trip() {
        let s = DiscoverySummary {
            run_id: RunId::new(),
            total_sketches: 80,
            category_count: 6,
            uncategorized_count: 4,
            categories_by_density: vec!["cat_01".into(), "cat_03".into(), "cat_02".into()],
            executive_summary: "# Executive\n\n...".into(),
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: DiscoverySummary = serde_json::from_str(&j).unwrap();
        assert_eq!(back.total_sketches, 80);
        assert_eq!(back.categories_by_density[0], "cat_01");
    }

    // -- Phase D types (Plan B sub-phase D) -------------------------------

    /// Every Phase D struct must accept an empty JSON object. Same
    /// leniency rationale as the discovery types: the LLM may emit
    /// `{}` when it skips the role and the pipeline must keep going.
    #[test]
    fn empty_object_parses_for_phase_d_types() {
        let _: SynthesizedProposal = serde_json::from_str("{}").unwrap();
        let _: AdversaryReport = serde_json::from_str("{}").unwrap();
        let _: HumanCheckpoint = serde_json::from_str("{}").unwrap();
    }

    /// SynthesizedProposal preserves the lineage (`source_proposals`)
    /// and the synthesis strategy so the deliver phase can surface
    /// "this came from merging X and Y" in the final report.
    #[test]
    fn synthesized_proposal_round_trip() {
        let s = SynthesizedProposal {
            id: "s_001".into(),
            source_proposals: vec!["p_001".into(), "p_002".into()],
            cluster_id: "cluster_01".into(),
            synthesis_strategy: "merge_invariants".into(),
            summary: "Best of both".into(),
            approach: "## Approach\n\nmerged".into(),
            tradeoffs: vec!["more tokens".into()],
            evidence: vec!["sk_001".into()],
            sources: vec!["p_001".into(), "p_002".into()],
            created_unix: 1_700_000_000,
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: SynthesizedProposal = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, "s_001");
        assert_eq!(back.source_proposals.len(), 2);
        assert_eq!(back.synthesis_strategy, "merge_invariants");
    }

    /// AdversaryReport keeps the score_delta so the rank phase can
    /// apply it without re-parsing comments.
    #[test]
    fn adversary_report_round_trip() {
        let a = AdversaryReport {
            proposal_id: "p_001".into(),
            consensus_check: "weak".into(),
            disagreement_score: 1.8,
            weaknesses: vec!["assumes no concurrent writers".into()],
            unverified_claims: vec!["throughput of 10k req/s".into()],
            score_delta: -0.6,
            rationale: "edge case under load".into(),
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&a).unwrap();
        let back: AdversaryReport = serde_json::from_str(&j).unwrap();
        assert_eq!(back.proposal_id, "p_001");
        assert!((back.score_delta + 0.6).abs() < 1e-6);
    }

    /// HumanCheckpoint captures the verbatim question + response so
    /// re-runs with the same brief can be audited later.
    #[test]
    fn human_checkpoint_round_trip() {
        let c = HumanCheckpoint {
            id: "h_001".into(),
            phase: "clarify".into(),
            kind: "clarify".into(),
            question: "Continue with assumption X?".into(),
            response: "y".into(),
            at_unix: 1_700_000_000,
            accepted_default: false,
            schema_version: "v1".into(),
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: HumanCheckpoint = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, "h_001");
        assert_eq!(back.response, "y");
        assert!(!back.accepted_default);
    }
}
