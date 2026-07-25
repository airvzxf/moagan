//! Domain types — every JSON shape the phases read or write is defined
//! here. The fields are intentionally permissive: every LLM role is
//! allowed to surface extra information as long as the contract keys
//! are present.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::RunId;

/// Output of the intake phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Acceptance criteria.
    pub acceptance: String,
    /// Known risks.
    pub risks: Vec<String>,
}

/// Output of the route phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Output of the gate phase (one per proposal).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    /// Did the proposal pass the structural check?
    pub pass: bool,
    /// Issues found.
    pub issues: Vec<String>,
    /// Missing fields.
    pub missing: Vec<String>,
}

/// Output of the critique phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Critique {
    /// Verdict.
    pub verdict: String,
    /// Issues found.
    pub issues: Vec<String>,
    /// Suggestions.
    pub suggestions: Vec<String>,
}

/// Output of the repair phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeScore {
    /// Overall score (0..=10).
    pub score: f32,
    /// Per-criterion breakdown.
    pub criteria: JudgeCriteria,
    /// Free-form comments.
    pub comments: String,
}

/// Per-criterion breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ranking {
    /// Ranked proposals (highest first).
    pub ranked: Vec<RankEntry>,
    /// Winning proposal id.
    pub winner: String,
}

/// One entry in the ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankEntry {
    /// Proposal id.
    pub id: String,
    /// Score.
    pub score: f32,
    /// Human-readable reason.
    pub reason: String,
}

/// Output of the deliver phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
            acceptance: "ok".into(),
            risks: vec!["r".into()],
        };
        let j = serde_json::to_string(&b).unwrap();
        let back: Brief = serde_json::from_str(&j).unwrap();
        assert_eq!(back.problem, "x");
    }
}
