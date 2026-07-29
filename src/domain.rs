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
}
