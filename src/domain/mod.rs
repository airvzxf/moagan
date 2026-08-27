//! Domain types — every JSON shape the phases read or write is defined
//! here. The fields are intentionally permissive: every LLM role is
//! allowed to surface extra information as long as the contract keys
//! are present.

pub mod constraint;
/// Reexports for problem graph domain types.
pub mod graph;
pub mod synthesis_request;

use std::collections::BTreeMap;

use crate::ids::RunId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// Lenient deserializer for `Vec<String>` fields.
///
/// The upstream MiniMax LLM occasionally returns a bare string where
/// the schema expects an array of items — most often for the
/// `objectives`, `constraints`, and `non_goals` fields of the Intake
/// and Brief roles (regression observed in CI run 32279237605 and
/// documented in issue #556). Strict `Vec<String>` deserialization
/// errors out with a `schema violation` whenever this happens, which
/// aborts the whole run before the audit log fills and breaks the
/// `proxy_e2e_*_audit_log_*_count_ge_5` smoke assertions.
///
/// This helper accepts, without erroring:
/// - a JSON array of strings → returned as-is,
/// - a single JSON string → wrapped into a one-element `Vec`,
/// - `null` / missing → empty `Vec` (defers to `#[serde(default)]`).
///
/// Anything else (`bool`, `number`, an object, …) is coerced to an
/// empty `Vec` as a best-effort fallback — we never want one weird
/// LLM reply to fail an entire `moagan run`. Issue #556 tracks the
/// broader contract-violation story; this is the cheap, safe escape
/// hatch that unblocks the smoke gates today.
fn deserialize_lenient_vec_string<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde_json::Value;

    let value = Value::deserialize(deserializer).unwrap_or(Value::Null);
    let coerced: Vec<String> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(s) => s,
                // nested array → flatten by joining the inner strings
                Value::Array(inner) => inner
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => s,
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => other.to_string(),
            })
            .collect(),
        Value::String(s) => {
            // Multi-line string → one entry per non-empty line; this
            // matches how a model often flattens a single-string
            // "objective" into a multi-line paragraph.
            let lines: Vec<String> = s
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            if lines.is_empty() {
                vec![s.trim().to_owned()]
            } else {
                lines
            }
        }
        other => vec![other.to_string()],
    };
    Ok(coerced)
}

/// Output of the intake phase.
///
/// All fields are lenient: `#[serde(default)]` lets missing fields
/// become empty, and unknown fields from the model are silently
/// ignored. The `MiniMax-M3` model with `thinking` enabled routinely
/// mixes `Intake` and `Brief` fields in the same response, so the
/// schema must tolerate that. The `Vec<String>` fields also use
/// `deserialize_lenient_vec_string` so that a model reply with a
/// bare string (instead of an array) still parses — see the doc on
/// that helper and issue #556.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Intake {
    /// The user's problem statement, rephrased by the LLM.
    pub problem: String,
    /// Concrete objectives extracted from the prompt.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub objectives: Vec<String>,
    /// Constraints (hard or soft) the user mentioned.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub constraints: Vec<String>,
    /// Non-goals explicitly or implicitly stated.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub non_goals: Vec<String>,
    /// Open questions the LLM flagged for clarification.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub open_questions: Vec<String>,
    /// Verbatim user prompt.
    pub raw_prompt: String,
}

/// Output of the clarify phase — the canonical brief.
///
/// Same leniency as `Intake`. `acceptance` is a `Vec<String>` because
/// the model returns it as a list, not a single string. All
/// `Vec<String>` fields use `deserialize_lenient_vec_string` so a
/// model reply with a bare string still parses (issue #556).
///
/// Phase J (v0.3 «tercera etapa», sub-fase J): `context_block` is
/// the verbatim text the intake phase prepended to the LLM prompt
/// when `--context` was used. It is roundtripped through
/// `brief.json` so a post-execution review can reconstruct the
/// exact prompt the model saw without re-loading the context ref.
/// The field is `#[serde(default, skip_serializing_if = "Option::is_none")]`
/// so legacy sidecars (no `context_block`) parse cleanly into
/// `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Brief {
    /// Concrete problem.
    pub problem: String,
    /// Concrete objectives.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub objectives: Vec<String>,
    /// Concrete deliverables.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub deliverables: Vec<String>,
    /// Hard and soft constraints.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub constraints: Vec<String>,
    /// Assumptions the team is making.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub assumptions: Vec<String>,
    /// Explicit non-goals.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub non_goals: Vec<String>,
    /// Acceptance criteria (one bullet per item; the model emits a list).
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub acceptance: Vec<String>,
    /// Known risks.
    #[serde(deserialize_with = "deserialize_lenient_vec_string")]
    pub risks: Vec<String>,
    /// Phase J: verbatim text prepended to the LLM prompt from a
    /// `--context` reference. Empty when the run had no context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_block: Option<String>,
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
    /// Phase F: when this proposal was superseded by a synthesis,
    /// the id of that synthesis (e.g. `"s_00"`). `None` for active
    /// proposals. Used by `RankPhase` to filter the final output
    /// without losing the lineage — the synthesized sidecar keeps
    /// `source_proposals` intact so the genealogy stays recoverable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub replaced_by: Option<String>,
    /// Phase G: ids of `ProblemGraph` nodes this proposal covers.
    /// Empty for non-deep runs (the field is `#[serde(default)]` so
    /// legacy sidecars parse cleanly). A proposal that addresses
    /// every node is the most general; a proposal that addresses a
    /// single node is the most focused. The deliver phase can use
    /// this to surface coverage in the final report.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub source_nodes: Vec<String>,
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
    /// Overall score (0..=10). Serialised via Ryu's shortest
    /// round-trip decimal so the sidecar carries `0.85`, not
    /// `0.8500000238418579`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
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
    /// Per-proposal stability score in `[0.0, 1.0]` (fraction of
    /// weight perturbations under which the proposal kept its rank).
    /// `None` when the stability check was skipped (weights fixed or
    /// `Config::stability.enabled == false`). V4 §5.12 paso 6.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stability_score: Option<std::collections::HashMap<String, f32>>,
    /// Coarse stability verdict. `None` mirrors `stability_score`'s
    /// semantics — present iff the check actually ran.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stability_label: Option<StabilityLabel>,
    /// Sigma used for the perturbations that produced `stability_score`.
    /// Recorded for telemetry so operators can correlate sensitivity
    /// with the perturbation magnitude. Serialised via Ryu's
    /// shortest round-trip decimal so the ranking sidecar carries
    /// `0.7`, not `0.7000000476837158`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        with = "crate::serde_util::clean_f32::opt_scalar"
    )]
    pub stability_sigma: Option<f32>,
}

/// One entry in the ranking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RankEntry {
    /// Proposal id.
    pub id: String,
    /// Score. Serialised via Ryu's shortest round-trip decimal
    /// so the ranking sidecar carries `0.85`, not
    /// `0.8500000238418579`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
    pub score: f32,
    /// Human-readable reason.
    pub reason: String,
}

/// Stability verdict produced by `ranking::stability::stability_label`.
/// `Sensitive` means the top-1 winner changed in more than
/// `(1.0 - threshold)` of the perturbations; `Stable` otherwise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StabilityLabel {
    /// Top-1 winner was invariant under perturbations.
    #[default]
    Stable,
    /// Top-1 winner changed in some perturbations — operator may
    /// want to re-rank with different weights.
    Sensitive,
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
///
/// Phase J (v0.3 «tercera etapa», sub-fase J) adds the lineage
/// block: `parent_run_id`, `shared_brief_hash`, `context_refs`,
/// and `lineage_paths`. All four are `#[serde(default,
/// skip_serializing_if = ...)]` so legacy v0.3 sidecars (pre-J)
/// parse cleanly into `None` / empty values. The line of code
/// that pins this behaviour is the
/// `manifest_parses_legacy_sidecar_without_lineage_block` test in
/// the unit-test module at the bottom of this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
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
    /// Phase J: parent run id when this run was launched with
    /// `--context <run_id>` or `moagan rerun`. `None` for runs that
    /// stand on their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    /// Phase J: SHA-256 of the canonical concatenation of every
    /// loaded context text (`shared_brief_hash`). Stable across
    /// re-runs of the same brief + context block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_brief_hash: Option<String>,
    /// Phase J: per-file hashes + byte counts for every context
    /// reference that fed into the run. Mirrors the SQLite
    /// `run_context_refs` table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<crate::context::ContextRefRecord>,
    /// Phase M (D.12.16): filesystem locations the lineage walked
    /// through. Populated from `RunPaths::resolve(home, run_id)` in
    /// `build_manifest` so every run ships with a typed table of its
    /// own well-known paths. The `relative` map is for human-readable
    /// paths (`brief`, `final`, `manifest`, ...); the `absolute`
    /// map stores the resolved `PathBuf`s for re-entry. `None`
    /// for runs that pre-date the field (legacy readers parse
    /// it as `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_paths: Option<LineagePaths>,
    /// D.14.6: the verbatim CLI prompt the user passed to
    /// `moagan run --prompt <text>`. Captured at run start so
    /// `moagan rerun` can re-feed the exact same input to the
    /// intake phase (the LLM cache key is derived from the user
    /// message, so re-running with the same prompt MUST replay
    /// the same cache keys; without this field the rerun would
    /// fall back to the LLM's `Intake.raw_prompt` echo, which
    /// differs from the CLI prompt in mock responses). `None`
    /// for runs that pre-date the field (legacy readers parse
    /// it as `None` and the rerun falls back to the recovered
    /// value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_prompt: Option<String>,
    /// F5: BLAKE3 hash of the canonical TOML serialization of the
    /// run's `Config` (see `intake.rs::compute_config_hash`).
    /// Pinned at run start so two runs with the same config
    /// produce the same `config_hash` and a `moagan rerun` with
    /// a different config can be detected by comparing the hash.
    /// `None` for runs that pre-date the field (legacy readers
    /// parse it as `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    /// F5: ISO-8601 UTC timestamp string for when the run was
    /// first created. Mirrors `created_at` (a `DateTime<Utc>`)
    /// but kept as an explicit string so external consumers can
    /// diff it against telemetry / SQLite without re-parsing
    /// RFC3339. Empty for runs that pre-date the field.
    pub created_at_iso: String,
    /// F5: ISO-8601 UTC timestamp of the most recent `moagan
    /// continue` invocation. `None` until the first resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resumed_at_iso: Option<String>,
    /// F5: number of times the run has been resumed via `moagan
    /// continue`. Default 0; saturates on overflow so a long-lived
    /// run cannot panic.
    pub resume_count: u32,
    /// D.22.2 (v0.5 PR-08): cumulative `prohibited_decisions`
    /// recorded by `moagan refine <run> tighten-constraint`. Each
    /// entry is the `verdict_detail` from the
    /// `RefineAction::TightenConstraint` dispatcher that the CLI
    /// has persisted back to disk. Mirrors the
    /// `SynthesisRequest::prohibited_decisions` block that the
    /// dispatcher augments so a subsequent `moagan rerun` can
    /// re-feed the same constraints. Empty for runs that pre-date
    /// the field; legacy readers parse it as empty via
    /// `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prohibited_decisions: Vec<String>,
}

/// Numeric manifest schema version. Bumped to `2` by F5 to add
/// the lifecycle / reproducibility block (`config_hash`,
/// `created_at_iso`, `last_resumed_at_iso`, `resume_count`). The
/// JSON sidecar still uses the string form (`"v2"`) for
/// human-readability; the typed constant is the single source of
/// truth for downstream consumers that want a numeric comparison.
pub const SCHEMA_VERSION: u32 = 2;

/// Back-compat alias for `SCHEMA_VERSION`. The F5 task brief
/// names the constant `MANIFEST_SCHEMA_VERSION`; the existing
/// `SCHEMA_VERSION` name matches the convention used by the other
/// schema-versioned artefacts (`ArtifactMeta::SCHEMA_VERSION`,
/// `Checkpoint::SCHEMA_VERSION`, `ValidationSidecar::SCHEMA_VERSION`).
pub const MANIFEST_SCHEMA_VERSION: u32 = SCHEMA_VERSION;
/// Numeric manifest version for v2 sidecars.
pub const MANIFEST_VERSION_V2: u32 = 2;

/// Lineage path block. Stored as two parallel maps so the JSON
/// sidecar remains human-readable while the in-memory
/// representation keeps `PathBuf` semantics.
///
/// The key type is `String` (not `&'static str`) so the sidecar
/// survives `Deserialize` — `HashMap<&'static str, _>` cannot be
/// deserialised because the deserialiser borrows from a `'de`
/// lifetime, not `'static`. Callers that want a typed label can
/// use the `well_known` constants below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LineagePaths {
    /// Stable relative labels → on-disk relative paths (e.g.
    /// `"brief" -> "brief.json"`). Used by the dashboard to render
    /// clickable breadcrumb links.
    pub relative: std::collections::HashMap<String, String>,
    /// Stable absolute labels → on-disk absolute paths (e.g.
    /// `"brief" -> "/home/wolf/.../019f.../brief.json"`). Used
    /// by `moagan rerun` and the inspect surface to re-open the
    /// parent.
    pub absolute: std::collections::HashMap<String, std::path::PathBuf>,
}

impl LineagePaths {
    /// Well-known label for the parent run directory.
    pub const LABEL_PARENT_RUN_DIR: &'static str = "parent_run_dir";
    /// Well-known label for the `final/` directory of the current run.
    pub const LABEL_FINAL_DIR: &'static str = "final_dir";
    /// Well-known label for the `sketches/` directory of the parent run.
    pub const LABEL_PARENT_SKETCHES: &'static str = "parent_sketches_dir";

    /// Convert a `RunPaths` (from `fs_layout::RunPaths::resolve`,
    /// sub-fase M's catalog) into a `LineagePaths` suitable for
    /// the `Manifest.lineage_paths` field. The two structs share
    /// the same dual-map shape; this adapter keeps them in sync
    /// after the M sub-fase merged in.
    pub fn from_run_paths(rp: &crate::fs_layout::RunPaths) -> Self {
        tracing::trace!(
            relative_keys = rp.relative.len(),
            absolute_keys = rp.absolute.len(),
            "domain::LineagePaths::from_run_paths: enter"
        );
        let relative = rp.relative.clone().into_iter().collect();
        let absolute = rp.absolute.clone().into_iter().collect();
        Self { relative, absolute }
    }
}

impl Manifest {
    /// F5: numeric schema version. Matches the top-level constant
    /// `SCHEMA_VERSION` and is exposed here so downstream consumers
    /// that already carry a `Manifest` value can branch on the
    /// version without importing the constant.
    pub const SCHEMA_VERSION: u32 = SCHEMA_VERSION;

    /// F5: build the canonical `"vN"` string used in the JSON
    /// sidecar. Pinning the formatter on the type keeps the
    /// `"v1" -> "v2"` migration a single `const` change.
    pub fn schema_version_string() -> String {
        let s = format!("v{}", Self::SCHEMA_VERSION);
        tracing::trace!(version = %s, "domain::Manifest::schema_version_string");
        s
    }

    /// F5: stable accessor for the config hash. Returns the hex
    /// digest or `None` for runs that pre-date the field (legacy
    /// v1 sidecars parse it as `None`).
    pub fn config_hash(&self) -> Option<&str> {
        tracing::trace!(
            has_hash = self.config_hash.is_some(),
            "domain::Manifest::config_hash"
        );
        self.config_hash.as_deref()
    }

    /// F5: returns `true` when the manifest has been resumed at
    /// least once. Equivalent to `resume_count > 0` but reads
    /// more naturally in `moagan inspect` / dashboard code.
    pub fn has_been_resumed(&self) -> bool {
        let resumed = self.resume_count > 0;
        tracing::trace!(
            resume_count = self.resume_count,
            resumed,
            "domain::Manifest::has_been_resumed"
        );
        resumed
    }

    /// F5: record a `moagan continue` invocation. Saturates the
    /// counter on overflow so a long-lived run cannot panic and
    /// bumps `updated_at` so the manifest sidecar's
    /// `manifest_blake3` is recomputed by the next writer.
    pub fn mark_resumed(&mut self, iso: &str) {
        let prev = self.resume_count;
        self.resume_count = self.resume_count.saturating_add(1);
        self.last_resumed_at_iso = Some(iso.to_owned());
        self.updated_at = chrono::Utc::now();
        tracing::info!(
            prev,
            new = self.resume_count,
            "domain::Manifest::mark_resumed: incremented resume_count"
        );
    }

    /// F5: stamp the ISO-8601 creation timestamp. Idempotent on
    /// the JSON sidecar (the same string is written every call)
    /// and exposed so `build_manifest` can populate the field
    /// without reaching for `chrono::Utc` directly.
    pub fn stamp_created_at(&mut self) {
        if self.created_at_iso.is_empty() {
            self.created_at_iso = self.created_at.to_rfc3339();
            tracing::trace!(
                created_at_iso = %self.created_at_iso,
                "domain::Manifest::stamp_created_at: stamped"
            );
        } else {
            tracing::trace!("domain::Manifest::stamp_created_at: already stamped");
        }
    }
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
    /// `uncategorized` (V4 §6.5). Serialised via Ryu's shortest
    /// round-trip decimal so the sidecar carries `0.74`, not
    /// `0.74000000953…`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
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
    /// Mean intra-cluster similarity score (0..=1). Serialised
    /// via Ryu's shortest round-trip decimal so the clusters
    /// sidecar carries `0.74`, not `0.74000000953…`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
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

/// Captured human-checkpoint decision for the discovery sub-pipeline
/// (V4 §6.11 + T01-06 §9.11). The string form of `decision` is the
/// `Resolution` action that fired: `approve`, `review`, `block`,
/// `export`, `modify`, or `reject`. We keep it as a `String` (not an
/// enum) so a future action (e.g. `rerun`) lands without a schema
/// bump.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HumanCheckpointDecision {
    /// Decision action (`approve`, `review`, `block`, `export`,
    /// `modify`, `reject`).
    pub decision: String,
    /// Unix seconds at capture time. Mirrors
    /// `HumanCheckpoint.at_unix` for the parent sidecar.
    pub at_unix: i64,
    /// Id of the parent `HumanCheckpoint` (`h_<uuid7>`). The JSON
    /// sidecar is the canonical record; the field lets
    /// `moagan inspect` correlate the decision back to the
    /// verbatim question + response without re-parsing every
    /// `checkpoints/h_<NN>.json`.
    pub checkpoint_id: String,
}

/// Discovery sub-manifest written by `discover_summary` after the
/// human checkpoint fires (V4 §6.11 + T01-06 §9.11). Persisted at
/// `<run_dir>/discovery.json` so the post-execution review and the
/// `moagan inspect` CLI can answer "did the user approve the
/// discovery output?" without parsing every per-checkpoint sidecar.
/// The on-disk filename is the "discovery manifest"; the structure
/// is `Manifest.discovery` in spec-speak so the roadmap and the
/// integration test can refer to `discovery.approved = true` and
/// `discovery.human_checkpoint.decision = "approve"` uniformly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverySection {
    /// Number of `final/cat_NN.json` documents produced.
    pub cat_count: usize,
    /// Number of facet lists in `facets/`.
    pub facet_count: usize,
    /// Number of `Contradiction` entries in
    /// `contradictions/contradictions.json`.
    pub contradictions: usize,
    /// Captured human-checkpoint decision. `None` for runs that
    /// pre-date the field (e.g. legacy sidecars from before
    /// PR-20) and for non-interactive runs that short-circuited
    /// the prompt to a `<skipped:non_interactive>` marker (the
    /// `approved` flag captures that distinction).
    pub human_checkpoint: Option<HumanCheckpointDecision>,
    /// `true` when the user (or the CI script) approved the
    /// discovery output. The default `false` covers runs that
    /// exited via `<skipped:non_interactive>` so a dashboard
    /// query can still distinguish "approved" from "never
    /// asked".
    pub approved: bool,
    /// Schema version. Always `"v1"` for v0.5; bumping requires
    /// a sidecar migration.
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

/// Output of the `Role::MergeSynthesizer` role (catalog D.7.1).
/// Captures the merge plan the model produced plus the lineage of
/// sources that fed it. Used by `SynthesizePhase` to gate the
/// synthesis against the `HARD_INCOMPATIBILITIES` predicate and
/// to stamp the resulting `synthesized/s_<NN>.json` sidecar.
///
/// V1: this is the new MergeSynthesizer role's output shape. It
/// supersedes the legacy `SynthesizedProposal` contract: the
/// `hard_constraint_check` map is the structured replacement for
/// the `evidence: ["<key>:<ok>"]` pattern that `SynthesizedProposal`
/// used, and the `sources` list is now mandatory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MergePlan {
    /// Executive summary (1-3 sentences).
    pub summary: String,
    /// Merged approach (2-5 sentences).
    pub approach: String,
    /// Trade-offs preserved from the cluster.
    pub tradeoffs: Vec<String>,
    /// Per-source evidence excerpts.
    pub evidence: Vec<String>,
    /// Source proposal ids that fed this merge.
    pub sources: Vec<String>,
    /// Map of hard constraint key -> satisfied flag.
    pub hard_constraint_check: BTreeMap<String, bool>,
    /// Operator note on how to verify the merge locally.
    pub expected_validation: String,
    /// Schema version.
    pub schema_version: String,
}

/// Output of the `Role::TiefighterCritic` role (D.7.1 catalog).
///
/// Carries the proposal the critic is attacking plus the structured
/// adversarial findings. The critic is deterministic (T=0.0, top_p=0.1,
/// max_tokens=1000000), so two runs against the same input produce the
/// same payload. `#[serde(default)]` keeps the validator accepting
/// empty objects (the same contract as the other P-role types).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TiefighterCriticReport {
    /// The proposal text the critic is attacking (echoed for
    /// downstream phases that want to correlate critique -> source).
    pub proposal: String,
    /// Verdict headline (e.g. "weak", "mixed", "strong").
    pub verdict: String,
    /// Concrete weaknesses the critic surfaced, ordered by impact.
    pub weaknesses: Vec<String>,
    /// Concrete suggestions for closing the weaknesses.
    pub suggestions: Vec<String>,
    /// Per-evidence key -> excerpt pairs from the proposal.
    pub evidence: Vec<String>,
    /// Schema version.
    pub schema_version: String,
}
/// Output of the `Role::PersonaPicker` role (D.7.1 catalog).
///
/// Picks which persona (system prompt variant) a downstream phase
/// should adopt for the current run. Sampling contract
/// (T=0.3, top_p=0.9, max_tokens=1000000) balances determinism with
/// enough variance to escape obvious ties; callers that want a
/// hard lock can re-run with T=0.0 in `role_settings`.
/// `#[serde(default)]` keeps the validator accepting empty
/// objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonaPickerReport {
    /// Candidate persona ids supplied by the caller (echoed for
    /// downstream phases that want to audit which pool the picker
    /// saw).
    pub candidates: Vec<String>,
    /// Persona id the picker selected (must be one of `candidates`).
    pub selected: String,
    /// One-line rationale for the selection.
    pub rationale: String,
    /// Schema version.
    pub schema_version: String,
}
/// Output of the `Role::AnglePicker` role (D.7.1 catalog).
///
/// Picks which exploration angle a downstream phase should chase
/// for the current problem. Higher variance than `PersonaPicker`
/// (T=0.7, top_p=0.95) because the picker is meant to escape the
/// obvious angles and surface the *next* one — the caller's
/// `existing_angles` list deliberately anchors the model away from
/// the obvious. `#[serde(default)]` keeps the validator accepting
/// empty objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnglePickerReport {
    /// Problem statement the picker is anchoring against (echoed
    /// for downstream phases that want to correlate angle -> brief).
    pub problem: String,
    /// Existing angles the caller already tried (echoed for
    /// audit; the picker is expected to *not* repeat them).
    pub existing_angles: Vec<String>,
    /// The next exploration angle the picker recommends.
    pub selected: String,
    /// One-line rationale: why this angle vs the existing set.
    pub rationale: String,
    /// Schema version.
    pub schema_version: String,
}
/// Output of the `Role::FinalDisagreement` role (D.7.1 catalog).
///
/// Tiebreaker used when the 3 base judges disagree so strongly that
/// the normal weighted-aggregation cannot pick a winner. The
/// `judge_scores` echo the raw panel and `candidates` echo the
/// shortlist the panel voted on so downstream phases can audit the
/// decision. Sampling (T=0.2, top_p=0.85, max_tokens=1000000) keeps the
/// tiebreaker stable while leaving room for a small amount of
/// variance when the disagreement is genuine.
/// `#[serde(default)]` keeps the validator accepting empty objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FinalDisagreementReport {
    /// Raw scores the 3 base judges assigned (echoed for audit).
    pub judge_scores: Vec<JudgeScoreEntry>,
    /// Candidate shortlist the panel voted on (echoed for audit).
    pub candidates: Vec<CandidateEntry>,
    /// Candidate id the tiebreaker picked (must be one of
    /// `candidates`).
    pub winner_id: String,
    /// Absolute score gap on the 0..=10 scale between the chosen
    /// candidate and the runner-up. Informational only.
    pub margin: f32,
    /// One-paragraph rationale referencing concrete properties of
    /// the chosen candidate.
    pub rationale: String,
    /// Schema version.
    pub schema_version: String,
}
/// Per-judge score entry carried by `FinalDisagreementReport`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgeScoreEntry {
    /// Judge identifier (e.g. "judge-a").
    pub judge: String,
    /// Score the judge assigned on the 0..=10 scale. Serialised
    /// via Ryu's shortest round-trip decimal so the report
    /// sidecar carries `0.85`, not `0.8500000238418579`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
    pub score: f32,
}
/// Per-candidate entry carried by `FinalDisagreementReport`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CandidateEntry {
    /// Candidate identifier the caller is voting on.
    pub id: String,
    /// One-line summary of the candidate.
    pub summary: String,
    /// Approach the candidate is taking (mirrors `Proposal::approach`).
    pub approach: String,
}
/// Output of the `Role::Continuation` role (PR-C2).
///
/// Focused re-call issued by
/// `phases::phase::call_with_retry_parse` when the original
/// response comes back with `Response.truncated = true`. The
/// dispatcher hands the model the last ~500 bytes of the truncated
/// payload, and the model returns ONLY the bytes that would have
/// come next (no greetings, no repetition, no fences) wrapped in
/// this envelope so the dispatcher can stitch them onto the
/// original text before re-running the JSON parse pipeline.
///
/// `finished` is a *hint*: a `true` value stops the continuation
/// loop early. The real recovery signal is whether the
/// concatenated text parses; if it does not, the loop will try a
/// second continuation regardless of the hint.
///
/// `#[serde(default)]` keeps the validator accepting empty objects
/// (parity with every other opt-in catalog role under Track H).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContinuationReport {
    /// Bytes the model emits to follow the excerpt. The dispatcher
    /// appends this string to `accumulated` without a separator;
    /// the iterative bracket repair (`phases::util::repair_m3_brackets`)
    /// handles the missing-comma case if needed.
    pub continued: String,
    /// `true` when the model believes the response is now complete.
    /// Hint only; the parse pipeline is the source of truth.
    pub finished: bool,
    /// Echo of the last 50 chars of the input verbatim. Kept so
    /// the operator can audit that the model actually picked up at
    /// the right byte offset when looking at `telemetry/warnings.jsonl`.
    pub raw_excerpt: String,
    /// Schema version (`continuation.v1`).
    pub schema_version: String,
}
/// Output of the `Role::JsonRepairV2` role (D.7.1 catalog).
///
/// Optional second-pass LLM call used when the local heuristic
/// in `src/phases/util.rs::repair_m3_brackets` cannot turn a
/// malformed model output into valid JSON. The repair is
/// mechanical (T=0.0, top_p=0.5, max_tokens=1000000), so two runs
/// against the same malformed text must produce the same
/// `repaired` payload. `#[serde(default)]` keeps the validator
/// accepting empty objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JsonRepairV2Report {
    /// Echo of the raw text that failed to parse (kept for audit
    /// and snapshot tests).
    pub malformed: String,
    /// Role name whose shape we are repairing to. Must be one of
    /// `Role::as_str()` (e.g. `propose`, `judge`).
    pub target_schema: String,
    /// Repaired JSON string the caller can hand back to
    /// `serde_json::from_str` to deserialize into the target
    /// schema's domain type.
    pub repaired: String,
    /// Short note describing the edits the repair made.
    pub notes: String,
    /// Schema version.
    pub schema_version: String,
}
/// Output of the `Role::HostilePromptDetector` role (D.7.1 catalog).
///
/// Pre-processor that classifies incoming text as `safe`,
/// `suspicious`, or `hostile` so the orchestrator can
/// short-circuit or quarantine the request. Fully deterministic
/// (T=0.0, top_p=0.1, max_tokens=1000000) because a flaky detector
/// would cause false negatives in the quarantine path.
/// `#[serde(default)]` keeps the validator accepting empty
/// objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HostilePromptReport {
    /// Echo of the candidate text under inspection (kept for
    /// audit; PII / secrets are redacted by the prompt rules).
    pub input: String,
    /// Detector verdict. Exactly one of `safe`, `suspicious`,
    /// or `hostile`.
    pub verdict: String,
    /// Detector confidence on the 0..=1 scale. `0.0` means the
    /// input was empty and the detector could not decide.
    pub confidence: f32,
    /// Ordered list of reasons supporting the verdict. The first
    /// entry is the strongest signal the detector saw.
    pub reasons: Vec<String>,
    /// Recommended action for the orchestrator. MUST align with
    /// the verdict (safe -> allow, suspicious -> sanitize,
    /// hostile -> reject) except for the empty-input case.
    pub recommended_action: String,
    /// Schema version.
    pub schema_version: String,
}
/// Output of the `Role::ContradictionJudge` role (A#11).
///
/// Discovery-mode LLM-as-judge used by
/// `crate::discovery::contradiction::find_contradictions_against`.
/// Given one focal sketch and a list of candidate sketches the
/// judge produces a `findings` array — each finding pins a
/// contradiction pair against one of three severity buckets.
/// The wrapper exists so `Role::ContradictionJudge.validate_json`
/// can confirm the schema before the detector's own parse helper
/// unwraps the array. `#[serde(default)]` keeps the validator
/// accepting empty objects (the model is told it can emit zero
/// findings when the focal sketch has no contradictions).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContradictionJudgeReport {
    /// Per-pair contradiction findings the judge emitted. Empty
    /// when the judge decided nothing contradicts the focal
    /// sketch.
    pub findings: Vec<ContradictionFinding>,
    /// Wire-form schema version. Always
    /// `"contradiction_judge.v1"` for the v1 prompt.
    pub schema_version: String,
}

/// One contradiction the `Role::ContradictionJudge` surfaced.
///
/// The two sketch ids in `pair` are drawn verbatim from the user
/// payload (the focal sketch is always position 0; the candidate it
/// contradicts is at position 1). `severity` is one of `minor`,
/// `major`, `critical` — the enum is reused by the deterministic
/// parsing path so a malformed severity string does not silently
/// land in `domain::Contradiction` as a free-form string. The
/// detector accepts the legacy strings via [`Self::from_str_lossy`]
/// so a model that emits `"high"` (the old severity vocabulary) is
/// upgraded to `Major` instead of dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionFinding {
    /// Tuple of exactly two sketch ids (focal, candidate).
    pub pair: [String; 2],
    /// Severity bucket. Serialised as the lowercase snake_case
    /// variant name on the wire.
    pub severity: ContradictionSeverity,
    /// Short verbatim or paraphrased excerpt that backs the call.
    pub evidence: String,
    /// One-line actionable reconciliation hint.
    pub suggestion: String,
}

/// Severity buckets the contradiction judge can emit.
///
/// The discriminant ordering (`Minor`, `Major`, `Critical`) doubles
/// as the rank used to sort findings for persistence — `Critical`
/// surfaces first so the integrator / deliver phases see the most
/// urgent fixes up top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionSeverity {
    /// Cosmetic or single trade-off disagreement. Persisted as
    /// `"low"` so the existing `Contradiction` sidecar (which only
    /// stores `"low"|"medium"|"high"`) keeps its wire form stable.
    Minor,
    /// Architecture-changing disagreement. Persisted as `"medium"`.
    Major,
    /// Mutually exclusive guarantees — both sketches cannot be
    /// valid for the same brief. Persisted as `"high"`.
    Critical,
}

impl ContradictionSeverity {
    /// Parse a string into a severity bucket. Unknown strings fall
    /// back to [`Self::Minor`] so the detector never silently
    /// drops a finding on an unexpected label.
    pub fn from_str_lossy(s: &str) -> Self {
        let out = match s.trim().to_ascii_lowercase().as_str() {
            "critical" | "high" | "h" => Self::Critical,
            "major" | "medium" | "med" | "m" => Self::Major,
            "minor" | "low" | "l" => Self::Minor,
            _ => Self::Minor,
        };
        tracing::trace!(input = %s, ?out, "domain::ContradictionSeverity::from_str_lossy");
        out
    }

    /// Map the new severity vocabulary back to the legacy string
    /// the existing `domain::Contradiction` sidecar expects
    /// (`"low"|"medium"|"high"`). The mapping keeps the
    /// `contradictions.json` schema unchanged so the integrator
    /// phase can keep consuming it byte-for-byte.
    pub fn legacy_label(&self) -> &'static str {
        let out = match self {
            Self::Minor => "low",
            Self::Major => "medium",
            Self::Critical => "high",
        };
        tracing::trace!(?self, label = %out, "domain::ContradictionSeverity::legacy_label");
        out
    }

    /// Rank used to sort findings so `Critical` lands first.
    pub fn rank(&self) -> u8 {
        let out = match self {
            Self::Minor => 1,
            Self::Major => 2,
            Self::Critical => 3,
        };
        tracing::trace!(?self, rank = out, "domain::ContradictionSeverity::rank");
        out
    }
}

impl ContradictionFinding {
    /// Lenient parse from a generic JSON value. Used by the
    /// detector's own parse helper so a wrapper object whose
    /// `findings` items have partially-missing fields still
    /// surfaces the well-formed ones. Pairs whose `severity`
    /// string is empty default to [`ContradictionSeverity::Minor`].
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        tracing::trace!("domain::ContradictionFinding::from_json: enter");
        let pair = value.get("pair")?;
        let arr = pair.as_array()?;
        if arr.len() != 2 {
            tracing::trace!(
                arr_len = arr.len(),
                "domain::ContradictionFinding::from_json: pair must be length 2"
            );
            return None;
        }
        let id0 = arr.first()?.as_str()?.to_owned();
        let id1 = arr.get(1)?.as_str()?.to_owned();
        let severity = value
            .get("severity")
            .and_then(|v| v.as_str())
            .map(ContradictionSeverity::from_str_lossy)
            .unwrap_or(ContradictionSeverity::Minor);
        let evidence = value
            .get("evidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let suggestion = value
            .get("suggestion")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        tracing::trace!(
            id0 = %id0,
            id1 = %id1,
            ?severity,
            "domain::ContradictionFinding::from_json: parsed"
        );
        Some(Self {
            pair: [id0, id1],
            severity,
            evidence,
            suggestion,
        })
    }
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
    /// Serialised via Ryu's shortest round-trip decimal so the
    /// report sidecar carries `0.85`, not `0.8500000238418579`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
    pub disagreement_score: f32,
    /// Free-form weaknesses the adversary surfaced.
    pub weaknesses: Vec<String>,
    /// Claims the adversary considers under-verified.
    pub unverified_claims: Vec<String>,
    /// Score delta applied to the aggregated evaluation. Negative
    /// pulls the proposal down; positive boosts it. Range -2..=+2.
    /// Serialised via Ryu's shortest round-trip decimal so the
    /// report sidecar carries `0.6`, not `0.6000000238418579`.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
    pub score_delta: f32,
    /// Short rationale.
    pub rationale: String,
    /// Schema version.
    pub schema_version: String,
    /// Provenance drift score from the
    /// [`AdversaryPattern::ProvenanceDrift`](crate::ranking::adversary_patterns::AdversaryPattern::ProvenanceDrift)
    /// pass. Range 0.0..=1.0; higher means more cited sources
    /// landed outside the brief context. Defaulted so legacy
    /// sidecars continue to parse.
    #[serde(default)]
    pub provenance_drift_score: f64,
    /// Audience match score from the
    /// [`AdversaryPattern::AudienceMismatch`](crate::ranking::adversary_patterns::AdversaryPattern::AudienceMismatch)
    /// pass. Range 0.0..=1.0; higher means the tone / scope
    /// diverges further from the brief's audience cue. Defaulted
    /// so legacy sidecars continue to parse.
    #[serde(default)]
    pub audience_match_score: f64,
}

/// Why a run entered a paused state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// A human checkpoint requires a decision.
    HumanCheckpoint,
    /// A phase-specific timeout fired.
    TimeoutPhase,
    /// The total run timeout fired.
    TimeoutTotal,
    /// The configured plan limit was exceeded.
    PlanExceeded,
    /// The configured token budget was exhausted.
    BudgetExhausted,
    /// A provider failure requires operator attention.
    ProviderError,
    /// The user explicitly paused the run.
    UserPause,
    /// The prompt was rejected as hostile.
    HostilePrompt,
    /// The run cannot continue without more input.
    NeedsInput,
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

// =====================================================================
// Phase G types (v0.3 «tercera etapa», Plan B sub-fase G) — DAG
// decomposition for `deep` mode.
//                                                See V4 §5.3
// "Descomposición condicional" and proposal-02-rust.md §8.1 (step 3)
// + §16.4. The phase only runs in `deep` mode; other modes skip it
// (and `ProblemGraph::trivial` is the no-op default).
// =====================================================================

/// How a `GraphNode` should be validated when its work is done. The
/// `decompose` role returns one of these so the downstream
/// `SketchPhase` / `ProposePhase` know which validator to dispatch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationMethod {
    /// No external validation; the prose is its own evidence.
    #[default]
    None,
    /// The phase structural / constraints / sketch-shape validator.
    Structural,
    /// The `src/validators/` sandbox (Rust, Python, TS, SQL).
    Executable,
}

/// A single node in the `ProblemGraph` DAG. Each node is a sub-question
/// the `deep` pipeline is expected to answer; the `dependencies` list
/// is the parent set in the directed acyclic graph. The pipeline
/// executes the topological layers in parallel up to
/// `RunContext::parallelism`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphNode {
    /// Stable id (`n<NN>`). The `decompose` role is free to choose any
    /// slug; the phase normalises duplicates.
    pub id: String,
    /// The sub-question the node answers.
    pub question: String,
    /// What artefact the node is expected to emit (markdown, code,
    /// schema, etc.).
    pub expected_output: String,
    /// Hard constraints that apply specifically to this node.
    pub constraints: Vec<String>,
    /// Parent node ids in the DAG. Empty for root nodes.
    pub dependencies: Vec<String>,
    /// How the node's output should be validated. Defaults to
    /// `ValidationMethod::None`.
    pub validation_method: ValidationMethod,
}

/// A single integration rule that wires two adjacent layers of the DAG
/// together. Persisted verbatim for the `DeliverPhase` so the final
/// report can surface "how layer N feeds into layer N+1".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationRule {
    /// Source node id.
    pub from: String,
    /// Target node id.
    pub to: String,
    /// Human description of what flows from `from` to `to`.
    pub description: String,
}

/// Output of the `decompose` phase. Lives in `problem_graph.json` per
/// T01-06 §1.2. When `should_decompose` is `false` (or the brief is
/// trivial) the graph collapses to a single root node and every
/// downstream phase behaves as if no decomposition happened.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProblemGraph {
    /// Schema version. Always `"v1"` for v0.3.
    pub schema_version: String,
    /// Did the model judge the brief worth decomposing? `false` makes
    /// the whole graph collapse to a trivial single-node graph; the
    /// phase never calls the LLM in that case.
    pub should_decompose: bool,
    /// Nodes of the DAG. Empty when `should_decompose` is `false`.
    pub nodes: Vec<GraphNode>,
    /// Integration rules between nodes. Optional.
    pub integration_rules: Vec<IntegrationRule>,
    /// Optional critical path (`n0`, `n1`, ...). Best-effort from
    /// the model; the phase re-derives a deterministic path from the
    /// DAG when this is empty.
    pub critical_path: Vec<String>,
    /// Brief hash this graph was derived from. The phase fills it
    /// post-hoc so a second run with the same brief can re-use the
    /// graph (cross-run cache, opt-in for v0.3).
    pub brief_blake3: String,
    /// Unix seconds when this file was written.
    pub created_unix: i64,
}

impl ProblemGraph {
    /// A trivial graph: `should_decompose=false`, one synthetic root
    /// node. Every downstream phase sees this as "no decomposition
    /// happened" and falls back to its non-DAG behaviour.
    pub fn trivial(brief_blake3: impl Into<String>, now_unix: i64) -> Self {
        tracing::trace!(
            brief_blake3 = %String::new(),
            now_unix,
            "domain::ProblemGraph::trivial: building trivial graph"
        );
        Self {
            schema_version: "v1".into(),
            should_decompose: false,
            nodes: Vec::new(),
            integration_rules: Vec::new(),
            critical_path: Vec::new(),
            brief_blake3: brief_blake3.into(),
            created_unix: now_unix,
        }
    }

    /// The number of nodes the graph contains (0 for trivial).
    pub fn len(&self) -> usize {
        let n = self.nodes.len();
        tracing::trace!(n, "domain::ProblemGraph::len");
        n
    }

    /// True when there is nothing to do.
    pub fn is_empty(&self) -> bool {
        let empty = self.nodes.is_empty();
        tracing::trace!(empty, "domain::ProblemGraph::is_empty");
        empty
    }

    /// Find the indices of root nodes (no dependencies). Returns
    /// `Vec<usize>` of positions in `self.nodes`. A graph with no
    /// roots but non-empty `nodes` is malformed — callers should run
    /// `validate_no_cycles` first.
    pub fn roots(&self) -> Vec<usize> {
        let roots: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.dependencies.is_empty())
            .map(|(i, _)| i)
            .collect();
        tracing::trace!(
            node_count = self.nodes.len(),
            root_count = roots.len(),
            "domain::ProblemGraph::roots"
        );
        roots
    }

    /// Topological layers (Kahn's algorithm). Each layer is a `Vec<usize>`
    /// of node indices in `self.nodes`; the i-th layer has no edges
    /// to the (i-1)-th. Returns `Err` with the first orphan id when
    /// the graph has a cycle or a dangling reference.
    pub fn topological_layers(&self) -> Result<Vec<Vec<usize>>, String> {
        tracing::trace!(
            node_count = self.nodes.len(),
            "domain::ProblemGraph::topological_layers: enter"
        );
        let n = self.nodes.len();
        // Index nodes by id for O(1) lookup.
        let index: std::collections::HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (node.id.as_str(), i))
            .collect();
        // Reverse edges: for each node, who depends on it?
        let mut rev: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut in_degree = vec![0usize; n];
        for (i, node) in self.nodes.iter().enumerate() {
            for dep in &node.dependencies {
                let parent = index.get(dep.as_str()).ok_or_else(|| {
                    tracing::warn!(
                        node_id = %node.id,
                        missing_dep = %dep,
                        "domain::ProblemGraph::topological_layers: dangling dependency"
                    );
                    format!("node '{}' depends on missing node '{}'", node.id, dep)
                })?;
                rev[*parent].push(i);
                in_degree[i] += 1;
            }
        }
        // Initial frontier: nodes with no remaining dependencies.
        let mut frontier: Vec<usize> = (0..n).filter(|i| in_degree[*i] == 0).collect();
        let mut layers: Vec<Vec<usize>> = Vec::new();
        while !frontier.is_empty() {
            // Stable order so identical graphs produce identical layers
            // (this is the property `SketchPhase` and tests rely on).
            frontier.sort();
            let next_layer = frontier.clone();
            layers.push(std::mem::take(&mut frontier));
            for &node in &next_layer {
                for &child in &rev[node] {
                    in_degree[child] -= 1;
                    if in_degree[child] == 0 {
                        frontier.push(child);
                    }
                }
            }
        }
        let visited: usize = layers.iter().map(|l| l.len()).sum();
        if visited != n {
            let mut stuck: Vec<String> = (0..n)
                .filter(|i| in_degree[*i] > 0)
                .map(|i| self.nodes[i].id.clone())
                .collect();
            stuck.sort();
            stuck.dedup();
            tracing::warn!(
                visited,
                total = n,
                stuck = ?stuck,
                "domain::ProblemGraph::topological_layers: cycle detected"
            );
            return Err(format!("graph has a cycle; stuck at: {stuck:?}"));
        }
        tracing::debug!(
            layer_count = layers.len(),
            "domain::ProblemGraph::topological_layers: ok"
        );
        Ok(layers)
    }

    /// Return a parent-to-children adjacency map for DAG traversal.
    pub fn adjacency(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut adjacency = std::collections::HashMap::new();
        for node in &self.nodes {
            adjacency.entry(node.id.clone()).or_insert_with(Vec::new);
            for dependency in &node.dependencies {
                adjacency
                    .entry(dependency.clone())
                    .or_insert_with(Vec::new)
                    .push(node.id.clone());
            }
        }
        tracing::trace!(
            node_count = self.nodes.len(),
            adjacency_keys = adjacency.len(),
            "domain::ProblemGraph::adjacency"
        );
        adjacency
    }

    /// Detect cycles. Returns `Ok(())` when the DAG is acyclic and
    /// well-formed, `Err(message)` otherwise.
    pub fn validate_no_cycles(&self) -> Result<(), String> {
        tracing::trace!("domain::ProblemGraph::validate_no_cycles: enter");
        let result = self.topological_layers().map(|_| ());
        match &result {
            Ok(()) => tracing::trace!("domain::ProblemGraph::validate_no_cycles: ok"),
            Err(e) => tracing::warn!(
                error = %e,
                "domain::ProblemGraph::validate_no_cycles: cycle"
            ),
        }
        result
    }
}

/// Should the `decompose` phase actually call the LLM? V4 §5.3
/// defines the trigger conditions; the canonical brief drives them.
///
/// The implementation is **deliberately conservative**: a brief that
/// looks simple, has few deliverables, and lacks a clear dependency
/// graph will short-circuit to a trivial graph without spending a
/// LLM call. The thresholds were calibrated on the v0.2 mock-provider
/// fixtures so a typical 1-deliverable brief does not pay the cost.
pub fn should_decompose(brief: &Brief) -> bool {
    tracing::trace!(
        constraints = brief.constraints.len(),
        deliverables = brief.deliverables.len(),
        assumptions = brief.assumptions.len(),
        "domain::should_decompose: enter"
    );
    // Heuristic ladder (any condition makes the brief a candidate):
    //  1. ≥ 3 hard constraints → the LLM benefits from separation.
    //  2. ≥ 3 deliverables → multiple independent outputs.
    //  3. ≥ 2 assumptions that mention "depends on", "after", or
    //     "once" → explicit dependency hints from the user.
    //  4. Brief contains the magic word "subproblem" or "phase" → the
    //     user is already thinking in stages.
    if brief.constraints.len() >= 3 {
        tracing::debug!("domain::should_decompose: true via constraint count");
        return true;
    }
    if brief.deliverables.len() >= 3 {
        tracing::debug!("domain::should_decompose: true via deliverable count");
        return true;
    }
    for assumption in &brief.assumptions {
        let lower = assumption.to_lowercase();
        if lower.contains("depends on")
            || lower.contains("after ")
            || lower.contains("once ")
            || lower.contains(" subproblem")
            || lower.contains("phase ")
        {
            tracing::debug!("domain::should_decompose: true via assumption keyword");
            return true;
        }
    }
    tracing::debug!("domain::should_decompose: false");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_reason_serializes_to_snake_case() {
        let json = serde_json::to_string(&PauseReason::HumanCheckpoint).unwrap();
        assert_eq!(json, "\"human_checkpoint\"");
    }

    #[test]
    fn pause_reason_round_trips_all_variants() {
        let reasons = [
            PauseReason::HumanCheckpoint,
            PauseReason::TimeoutPhase,
            PauseReason::TimeoutTotal,
            PauseReason::PlanExceeded,
            PauseReason::BudgetExhausted,
            PauseReason::ProviderError,
            PauseReason::UserPause,
            PauseReason::HostilePrompt,
            PauseReason::NeedsInput,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let back: PauseReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

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
            context_block: Some("ctx".into()),
        };
        let j = serde_json::to_string(&b).unwrap();
        let back: Brief = serde_json::from_str(&j).unwrap();
        assert_eq!(back.problem, "x");
        assert_eq!(back.context_block.as_deref(), Some("ctx"));
    }

    /// The Intake / Brief Vec<String> fields use the lenient deserializer
    /// (issue #556). Verify each input shape it accepts.
    #[test]
    fn intake_objectives_accepts_string_instead_of_array() {
        // The exact failure mode from run 32279237605: a bare string
        // where the schema wants an array of objectives.
        let raw = r#"{
            "problem": "Build e-commerce",
            "objectives": "Identify the core services needed to support typical e-commerce functionality",
            "constraints": [],
            "non_goals": [],
            "open_questions": [],
            "raw_prompt": "x"
        }"#;
        let intake: Intake = serde_json::from_str(raw).expect("must not error on string-array");
        assert_eq!(
            intake.objectives,
            vec!["Identify the core services needed to support typical e-commerce functionality"]
        );
    }

    #[test]
    fn intake_objectives_splits_multiline_string_into_entries() {
        // A multi-line objective string should be split on non-empty
        // lines so the model can flatten a paragraph into per-line items.
        let raw = r#"{
            "problem": "x",
            "objectives": "- ship fast\n- ship safe\n- ship cheap",
            "constraints": [],
            "non_goals": [],
            "open_questions": [],
            "raw_prompt": "x"
        }"#;
        let intake: Intake = serde_json::from_str(raw).unwrap();
        assert_eq!(
            intake.objectives,
            vec![
                "- ship fast".to_string(),
                "- ship safe".to_string(),
                "- ship cheap".to_string(),
            ]
        );
    }

    #[test]
    fn intake_objectives_accepts_array_of_strings() {
        // The happy path stays unchanged: a JSON array of strings is
        // returned as-is, no semantic regression.
        let raw = r#"{
            "problem": "x",
            "objectives": ["fast", "safe"],
            "constraints": ["c1"],
            "non_goals": [],
            "open_questions": ["q1", "q2"],
            "raw_prompt": "x"
        }"#;
        let intake: Intake = serde_json::from_str(raw).unwrap();
        assert_eq!(intake.objectives, vec!["fast", "safe"]);
        assert_eq!(intake.constraints, vec!["c1"]);
        assert_eq!(intake.open_questions, vec!["q1", "q2"]);
    }

    #[test]
    fn intake_objectives_tolerates_null_and_missing() {
        // null and missing fields must both yield empty Vec, never errors.
        let from_null: Intake =
            serde_json::from_str(r#"{"problem": "x", "objectives": null, "raw_prompt": "x"}"#)
                .unwrap();
        assert!(from_null.objectives.is_empty());

        let from_missing: Intake =
            serde_json::from_str(r#"{"problem": "x", "raw_prompt": "x"}"#).unwrap();
        assert!(from_missing.objectives.is_empty());
    }

    #[test]
    fn brief_objectives_accepts_string_instead_of_array() {
        // Same lenient behaviour applies to the Brief struct.
        let raw = r#"{
            "problem": "x",
            "objectives": "ship the thing",
            "deliverables": ["d"],
            "constraints": "stay within budget",
            "assumptions": [],
            "non_goals": [],
            "acceptance": "release notes filed",
            "risks": []
        }"#;
        let brief: Brief = serde_json::from_str(raw).expect("lenient on bare strings");
        assert_eq!(brief.objectives, vec!["ship the thing"]);
        assert_eq!(brief.deliverables, vec!["d"]);
        assert_eq!(brief.constraints, vec!["stay within budget"]);
        assert_eq!(brief.acceptance, vec!["release notes filed"]);
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
            provenance_drift_score: 0.25,
            audience_match_score: 0.10,
        };
        let j = serde_json::to_string(&a).unwrap();
        let back: AdversaryReport = serde_json::from_str(&j).unwrap();
        assert_eq!(back.proposal_id, "p_001");
        assert!((back.score_delta + 0.6).abs() < 1e-6);
        assert!((back.provenance_drift_score - 0.25).abs() < 1e-9);
        assert!((back.audience_match_score - 0.10).abs() < 1e-9);
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

    // -- Phase G types (v0.3, Plan B sub-fase G) --------------------------

    /// Trivial graph is empty and well-formed; downstream phases see
    /// `is_empty() == true` and fall back to non-DAG behaviour.
    #[test]
    fn problem_graph_trivial_is_empty() {
        let g = ProblemGraph::trivial("abc", 1_700_000_000);
        assert!(g.is_empty());
        assert!(!g.should_decompose);
        assert!(g.roots().is_empty());
        assert!(g.topological_layers().unwrap().is_empty());
    }

    /// Empty `nodes` with `should_decompose=true` is malformed;
    /// `topological_layers` should not loop forever.
    #[test]
    fn problem_graph_empty_with_decompose_returns_no_layers() {
        let g = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: Vec::new(),
            ..Default::default()
        };
        assert!(g.topological_layers().unwrap().is_empty());
    }

    /// Single-node graph: the node is the only root and only layer.
    #[test]
    fn problem_graph_single_node_is_a_single_layer() {
        let g = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![GraphNode {
                id: "n0".into(),
                question: "what?".into(),
                expected_output: "an answer".into(),
                constraints: Vec::new(),
                dependencies: Vec::new(),
                validation_method: ValidationMethod::Structural,
            }],
            ..Default::default()
        };
        let layers = g.topological_layers().unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0], vec![0]);
    }

    /// Two roots + one shared child: the first layer is both roots,
    /// the second is the child.
    #[test]
    fn problem_graph_two_layers_kahn() {
        let g = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    dependencies: vec![],
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    dependencies: vec![],
                    ..Default::default()
                },
                GraphNode {
                    id: "c".into(),
                    dependencies: vec!["a".into(), "b".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let layers = g.topological_layers().unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0], vec![0, 1]);
        assert_eq!(layers[1], vec![2]);
    }

    /// A cycle surfaces as `Err` with a non-empty list of stuck ids.
    #[test]
    fn problem_graph_cycle_reports_error() {
        let g = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    dependencies: vec!["b".into()],
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    dependencies: vec!["a".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let err = g.topological_layers().unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    /// A dangling dependency surfaces as `Err` mentioning the missing id.
    #[test]
    fn problem_graph_dangling_dependency_reports_error() {
        let g = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![GraphNode {
                id: "a".into(),
                dependencies: vec!["ghost".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let err = g.topological_layers().unwrap_err();
        assert!(err.contains("ghost"), "got: {err}");
    }

    /// Round-trip: graph → JSON → graph preserves node ids, layers,
    /// and the schema version.
    #[test]
    fn problem_graph_round_trip() {
        let g = ProblemGraph {
            schema_version: "v1".into(),
            should_decompose: true,
            nodes: vec![
                GraphNode {
                    id: "a".into(),
                    question: "Q1".into(),
                    ..Default::default()
                },
                GraphNode {
                    id: "b".into(),
                    question: "Q2".into(),
                    dependencies: vec!["a".into()],
                    validation_method: ValidationMethod::Executable,
                    ..Default::default()
                },
            ],
            integration_rules: vec![IntegrationRule {
                from: "a".into(),
                to: "b".into(),
                description: "Q1's output is Q2's input".into(),
            }],
            critical_path: vec!["a".into(), "b".into()],
            brief_blake3: "deadbeef".into(),
            created_unix: 1_700_000_000,
        };
        let j = serde_json::to_string(&g).unwrap();
        let back: ProblemGraph = serde_json::from_str(&j).unwrap();
        assert_eq!(back.nodes.len(), 2);
        assert_eq!(back.nodes[1].dependencies, vec!["a"]);
        assert_eq!(
            back.nodes[1].validation_method,
            ValidationMethod::Executable
        );
        assert_eq!(back.integration_rules.len(), 1);
        assert_eq!(back.critical_path, vec!["a", "b"]);
        assert_eq!(back.brief_blake3, "deadbeef");
    }

    /// `should_decompose` mirrors the V4 §5.3 ladder.
    #[test]
    fn should_decompose_threshold_ladder() {
        // Empty brief → false.
        let brief = Brief::default();
        assert!(!should_decompose(&brief));
        // 1 deliverable + 1 constraint → still false.
        let brief = Brief {
            deliverables: vec!["deliver one thing".into()],
            constraints: vec!["must be fast".into()],
            ..Default::default()
        };
        assert!(!should_decompose(&brief));
        // 3 deliverables → true.
        let brief = Brief {
            deliverables: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert!(should_decompose(&brief));
        // 3 constraints → true.
        let brief = Brief {
            constraints: vec!["c1".into(), "c2".into(), "c3".into()],
            ..Default::default()
        };
        assert!(should_decompose(&brief));
        // Magic-word in assumption → true.
        let brief = Brief {
            assumptions: vec!["this is a subproblem we tackle in two parts".into()],
            ..Default::default()
        };
        assert!(should_decompose(&brief));
    }

    /// `ValidationMethod` round-trips through JSON with snake_case
    /// representation (so a downstream validator can parse it).
    #[test]
    fn validation_method_serialises_snake_case() {
        let m = ValidationMethod::Executable;
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(j, "\"executable\"");
        let back: ValidationMethod = serde_json::from_str(&j).unwrap();
        assert_eq!(back, ValidationMethod::Executable);
    }

    /// `Proposal.source_nodes` (Phase G) round-trips through JSON
    /// and is `#[serde(skip_serializing_if = "Vec::is_empty")]` so
    /// legacy v0.2 sidecars (which never emit the field) stay
    /// compact.
    #[test]
    fn proposal_source_nodes_round_trip() {
        let p = Proposal {
            id: "p_007".into(),
            source_nodes: vec!["n0".into(), "n1".into()],
            ..Default::default()
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains("source_nodes"), "missing field: {j}");
        let back: Proposal = serde_json::from_str(&j).unwrap();
        assert_eq!(back.source_nodes, vec!["n0", "n1"]);
    }

    /// Empty `source_nodes` is skipped in the JSON representation
    /// (the field's `skip_serializing_if` is `Vec::is_empty`).
    #[test]
    fn proposal_source_nodes_omitted_when_empty() {
        let p = Proposal::default();
        let j = serde_json::to_string(&p).unwrap();
        assert!(!j.contains("source_nodes"), "leaked field: {j}");
    }

    /// Legacy v0.2 sidecars (which never had `source_nodes`) parse
    /// into a Proposal with an empty vec, not a deserialise error.
    #[test]
    fn proposal_parses_legacy_sidecar_without_source_nodes() {
        let legacy = serde_json::json!({
            "id": "p_legacy",
            "summary": "old shape",
            "approach": "rust",
            "tradeoffs": [],
            "evidence": [],
            "source_sketch": "",
            "artifacts": [],
            "replaced_by": null,
        });
        let p: Proposal = serde_json::from_value(legacy).unwrap();
        assert!(p.source_nodes.is_empty());
    }

    /// Brief.context_block is omitted when `None` (so the v0.2
    /// legacy sidecars stay compact).
    #[test]
    fn brief_context_block_omitted_when_none() {
        let b = Brief::default();
        let j = serde_json::to_string(&b).unwrap();
        assert!(!j.contains("context_block"), "leaked field: {j}");
    }

    /// Brief round-trips a v0.2 legacy sidecar (no
    /// `context_block`) into a Brief with `context_block == None`.
    #[test]
    fn brief_parses_legacy_sidecar_without_context_block() {
        let legacy = serde_json::json!({
            "problem": "x",
            "objectives": ["a"],
            "deliverables": ["d"],
            "constraints": ["c"],
            "assumptions": [],
            "non_goals": [],
            "acceptance": ["ok"],
            "risks": ["r"],
        });
        let b: Brief = serde_json::from_value(legacy).unwrap();
        assert!(b.context_block.is_none());
    }

    /// Manifest round-trips a v0.3 pre-J legacy sidecar (no
    /// lineage block) into a Manifest with the four new fields all
    /// empty. The test pins the contract that the v0.3 → v0.3.1
    /// upgrade is forward-compatible: existing sidecars keep
    /// parsing.
    #[test]
    fn manifest_parses_legacy_sidecar_without_lineage_block() {
        let legacy = serde_json::json!({
            "schema_version": "v1",
            "run_id": "019f0000-0000-7000-8000-000000000001",
            "mode": "fast",
            "status": "completed",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:01Z",
            "client_version": "0.3.0",
            "brief_sha256": "",
            "brief_blake3": "",
            "provider": "minimax",
            "model": "MiniMax-M3",
            "phases": [],
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read": 0,
                "cache_creation": 0,
            },
            "manifest_blake3": "",
        });
        let m: Manifest = serde_json::from_value(legacy).unwrap();
        assert!(m.parent_run_id.is_none());
        assert!(m.shared_brief_hash.is_none());
        assert!(m.context_refs.is_empty());
        assert!(m.lineage_paths.is_none());
    }

    /// LineagePaths round-trips the dual-map shape used by
    /// `moagan rerun` to recover the parent run dir.
    #[test]
    fn lineage_paths_round_trip() {
        let mut p = LineagePaths::default();
        p.relative.insert(
            LineagePaths::LABEL_PARENT_RUN_DIR.into(),
            "../019f0000-0000-7000-8000-000000000001".into(),
        );
        p.absolute.insert(
            LineagePaths::LABEL_PARENT_RUN_DIR.into(),
            std::path::PathBuf::from("/tmp/.runs/019f0000-0000-7000-8000-000000000001"),
        );
        let j = serde_json::to_string(&p).unwrap();
        let back: LineagePaths = serde_json::from_str(&j).unwrap();
        assert_eq!(
            back.relative.get(LineagePaths::LABEL_PARENT_RUN_DIR),
            Some(&"../019f0000-0000-7000-8000-000000000001".to_string())
        );
        assert_eq!(
            back.absolute.get(LineagePaths::LABEL_PARENT_RUN_DIR),
            Some(&std::path::PathBuf::from(
                "/tmp/.runs/019f0000-0000-7000-8000-000000000001"
            ))
        );
    }

    // -- Phase M (D.12.16) — Manifest.lineage_paths populated from RunPaths

    /// A populated `lineage_paths` (built from
    /// `RunPaths::resolve(...)`) round-trips through serde
    /// unchanged. Pins that the M populate code (`build_manifest`
    /// in `src/cli/run.rs`) and the JSON sidecar agree on the
    /// shape.
    #[test]
    fn manifest_with_lineage_paths_round_trips() {
        crate::test_support::with_moagan_home("manifest_with_lineage_paths_round_trips", |_home| {
            use crate::fs_layout::{MoaganHome, RunPaths};
            let home = MoaganHome::resolve().unwrap();
            let paths = RunPaths::resolve(&home, RunId::new());
            let lineage = LineagePaths::from_run_paths(&paths);
            let m = Manifest {
                schema_version: "v1".into(),
                run_id: RunId::new(),
                mode: "fast".into(),
                status: "completed".into(),
                created_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
                updated_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
                client_version: "0.3.0".into(),
                brief_sha256: String::new(),
                brief_blake3: String::new(),
                provider: "mock".into(),
                model: "mock-1".into(),
                phases: Vec::new(),
                usage: crate::domain::ManifestUsage::default(),
                manifest_blake3: String::new(),
                parent_run_id: None,
                shared_brief_hash: None,
                context_refs: Vec::new(),
                lineage_paths: Some(lineage.clone()),
                cli_prompt: None,
                config_hash: None,
                created_at_iso: "2026-01-01T00:00:00+00:00".into(),
                last_resumed_at_iso: None,
                resume_count: 0,
                prohibited_decisions: Vec::new(),
            };
            let j = serde_json::to_string(&m).unwrap();
            let back: Manifest = serde_json::from_str(&j).unwrap();
            let lp = back.lineage_paths.expect("lineage_paths preserved");
            assert_eq!(lp, lineage);
            // Spot-check the keys.
            assert!(lp.relative.contains_key("brief"));
            assert!(lp.absolute.contains_key("final"));
        });
    }

    // -- F5: schema v2 + lifecycle metadata ---------------------------

    /// F5: bumping `SCHEMA_VERSION` to `2` propagates through the
    /// canonical string form (`"v2"`) and the typed constant. The
    /// four new fields (`config_hash`, `created_at_iso`,
    /// `last_resumed_at_iso`, `resume_count`) are present on a
    /// fresh Manifest with their documented defaults.
    #[test]
    fn manifest_schema_v2_includes_lifecycle_metadata() {
        assert_eq!(SCHEMA_VERSION, 2);
        assert_eq!(Manifest::SCHEMA_VERSION, 2);
        assert_eq!(MANIFEST_SCHEMA_VERSION, 2);
        assert_eq!(Manifest::schema_version_string(), "v2");

        let m = Manifest::default();
        assert_eq!(m.schema_version, "");
        assert!(m.config_hash().is_none());
        assert!(!m.has_been_resumed());
        assert_eq!(m.resume_count, 0);
        assert!(m.last_resumed_at_iso.is_none());
        assert_eq!(m.created_at_iso, "");
    }

    /// F5: `mark_resumed` is the canonical accessor for the
    /// lifecycle block. Each call increments the counter,
    /// overwrites `last_resumed_at_iso`, and refreshes
    /// `updated_at`. After three calls the counter saturates
    /// correctly when pushed past `u32::MAX` so the panic stays
    /// out of the operator's hot path.
    #[test]
    fn manifest_lifecycle_increments_resume_count() {
        let mut m = Manifest::default();
        assert!(!m.has_been_resumed());

        m.mark_resumed("2026-01-01T00:00:01+00:00");
        assert_eq!(m.resume_count, 1);
        assert_eq!(
            m.last_resumed_at_iso.as_deref(),
            Some("2026-01-01T00:00:01+00:00")
        );
        assert!(m.has_been_resumed());

        m.mark_resumed("2026-01-01T00:00:02+00:00");
        m.mark_resumed("2026-01-01T00:00:03+00:00");
        assert_eq!(m.resume_count, 3);
        assert_eq!(
            m.last_resumed_at_iso.as_deref(),
            Some("2026-01-01T00:00:03+00:00")
        );

        // Saturating add: pushing past u32::MAX must not panic.
        m.resume_count = u32::MAX;
        m.mark_resumed("2026-01-01T00:00:04+00:00");
        assert_eq!(m.resume_count, u32::MAX);
    }

    /// F5: a v1 legacy sidecar (no `config_hash`, no
    /// `created_at_iso`, no `last_resumed_at_iso`, no
    /// `resume_count`) parses into a v2 Manifest with the new
    /// fields populated from their documented defaults. The
    /// `#[serde(default, ...)]` annotations are what make this
    /// backwards-compatible load possible; this test pins the
    /// contract for downstream consumers that read mixed-version
    /// archives.
    #[test]
    fn manifest_v1_loads_v2_with_defaults() {
        let legacy = serde_json::json!({
            "schema_version": "v1",
            "run_id": "019f0000-0000-7000-8000-000000000001",
            "mode": "fast",
            "status": "completed",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:01Z",
            "client_version": "0.3.0",
            "brief_sha256": "",
            "brief_blake3": "",
            "provider": "minimax",
            "model": "MiniMax-M3",
            "phases": [],
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read": 0,
                "cache_creation": 0,
            },
            "manifest_blake3": "",
        });
        let m: Manifest = serde_json::from_value(legacy).unwrap();
        // v2 lifecycle fields default to empty / zero / None so
        // legacy sidecars continue to load.
        assert!(m.config_hash.is_none());
        assert_eq!(m.created_at_iso, "");
        assert!(m.last_resumed_at_iso.is_none());
        assert_eq!(m.resume_count, 0);
        // The schema_version field stays at the legacy value so a
        // round-trip preserves the originating schema marker.
        assert_eq!(m.schema_version, "v1");
    }
}
