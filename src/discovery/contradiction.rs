//! Discovery contradiction detector.
//!
//! A#11: complete LLM-as-judge detector. The lightweight stub
//! (top-pairs ranking, severity ordering, plain record) has been
//! replaced with a typed detector function that dispatches a real
//! LLM call against `Role::ContradictionJudge`, parses the JSON
//! response into [`crate::domain::ContradictionFinding`], and
//! gracefully handles malformed payloads without panicking. The
//! pair helpers and severity helpers are kept here because the
//! phase that wires the call still uses them to order the
//! candidates before the LLM pass.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{
    ContradictionFinding, ContradictionJudgeReport, ContradictionSeverity, Sketch,
};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::RunContext;

/// One contradiction record before it is serialised to
/// `crate::domain::Contradiction`. The transformation is `into()`.
///
/// Kept because the existing `contradictions.json` sidecar still
/// carries the `(cluster_a, cluster_b)` shape (V4 §6.7 + T01-06).
/// The new [`ContradictionFinding`] type is the LLM-as-judge
/// wire form; the phase that converts one to the other lives in
/// `src/phases/discover_contradict.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionRecord {
    /// Cluster id on the "a" side.
    pub cluster_a: String,
    /// Cluster id on the "b" side.
    pub cluster_b: String,
    /// Sketch ids that triggered the contradiction.
    pub representatives: Vec<String>,
    /// Topic.
    pub topic: String,
    /// Description.
    pub description: String,
    /// Severity.
    pub severity: String,
}

/// Pick the cluster pair(s) with the highest disagreement. The
/// heuristic is simple: the centroid-distance ranking from the
/// clustering step is in `distances` (already sorted descending by
/// the cluster phase), and we return the top-`max_n` pairs.
pub fn top_pairs(distances: &[(String, String, f32)], max_n: usize) -> Vec<(String, String, f32)> {
    distances.iter().take(max_n).cloned().collect()
}

/// Severity ordering used to sort contradictions before persistence.
///
/// Kept on top of the new [`ContradictionSeverity::rank`] so the
/// legacy sidecar (which stores free-form `"low"|"medium"|"high"`
/// strings) can keep using the existing numeric ladder without
/// having to round-trip through the new enum.
pub fn severity_rank(s: &str) -> u8 {
    match s {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// Maximum number of candidate sketches the LLM-as-judge prompt
/// embeds for one comparison. The cooldown is `MAX_PAIRS` per
/// cluster pair in `discover_contradict.rs`; this caps the inner
/// sketch pool so the prompt fits the 1M-token ceiling even on
/// large cluster pairs. 32 was picked because
/// `proposal-03 §D.x contradiction` mentions a 30-sketch ceiling
/// for the v1 dataset; the extra two slots cover mild overlap.
const MAX_CANDIDATES_PER_CALL: usize = 32;

/// Build the user payload for the LLM-as-judge call. The model
/// receives the focal sketch and a small list of candidate sketches
/// and is asked to surface every contradiction as a finding in the
/// `ContradictionJudge` schema.
///
/// `focal` is the sketch the caller wants to challenge; `candidates`
/// is the comparison pool the judge can find disagreements against.
/// When `candidates` is empty the helper returns a payload that
/// primes the judge to emit `{"findings": []}` so the wire form is
/// still well-typed.
pub fn user_payload(focal: &Sketch, candidates: &[Sketch]) -> String {
    let focal_block = render_sketch(focal);
    let candidate_blocks: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATES_PER_CALL)
        .map(render_sketch)
        .collect();
    format!(
        "Focal sketch:\n{focal}\n\n\
         Candidate sketches (compare every candidate against the focal):\n{cands}\n\n\
         Return a JSON object with one field:\n\
         - \"findings\": list of {{\"pair\": [\"<focal_id>\", \"<candidate_id>\"], \
         \"severity\": \"minor|major|critical\", \
         \"evidence\": \"<short excerpt>\", \
         \"suggestion\": \"<one-line hint>\"}} objects.\n\n\
         If nothing contradicts the focal sketch, return {{\"findings\": []}}.",
        focal = focal_block,
        cands = candidate_blocks.join("\n"),
    )
}

/// Render one sketch as a compact markdown bullet. Stays short
/// so the prompt fits the per-role ceiling even with the candidate
/// pool at the `MAX_CANDIDATES_PER_CALL` cap.
fn render_sketch(s: &Sketch) -> String {
    let decisions = if s.key_decisions.is_empty() {
        "  - (no explicit decisions)".to_owned()
    } else {
        s.key_decisions
            .iter()
            .map(|d| format!("  - {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "- id: {id}\n  thesis: {thesis}\n  key_decisions:\n{decisions}",
        id = s.id,
        thesis = s.thesis,
        decisions = decisions,
    )
}

/// LLM-as-judge entry point. Given one focal sketch and a list of
/// candidates, asks the model to surface every contradiction and
/// returns the typed [`Vec<ContradictionFinding>`].
///
/// Behaviour:
///
/// * Empty `candidates` short-circuits to `Ok(Vec::new())` — no
///   LLM call is made when there is nothing to compare against.
/// * The LLM call goes through the canonical
///   `RunContext::call_with_retry_parse` so the retry / parse /
///   telemetry pipeline stays consistent with the rest of the
///   catalog invocations.
/// * A model that returns malformed JSON does NOT bubble up an
///   error: the helper falls through to an empty Vec after a
///   `parse_fallback` warning so a single bad LLM response cannot
///   kill the discovery run. Operators can still correlate the
///   behaviour through the warnings stream / SQLite `calls` row.
pub async fn find_contradictions_against(
    ctx: &RunContext,
    focal: &Sketch,
    candidates: &[Sketch],
) -> Result<Vec<ContradictionFinding>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let user = user_payload(focal, candidates);
    let role = Role::ContradictionJudge;
    let system = system_prompt(role).to_owned();
    let schema_hint =
        "ContradictionJudge: {findings[]: {pair[id1,id2], severity, evidence, suggestion}}";
    let raw: Result<Value> = ctx
        .call_with_retry_parse(role, system.clone(), user, schema_hint, 3)
        .await;
    let raw = match raw {
        Ok(v) => v,
        Err(e) => {
            // Graceful fallback: log to telemetry, return empty
            // findings. The LLM-as-judge call layer already raised
            // its own `model.retry_parse` / `model.retry_provider`
            // warnings; we add one more so the discovery phase can
            // correlate a missing findings array with the failed
            // call.
            let _ = ctx.telemetry.warn(
                "discovery.contradiction_judge.parse_fallback",
                "warn",
                "contradiction judge call did not parse; surfacing zero findings",
                serde_json::json!({
                    "focal_id": focal.id,
                    "candidate_count": candidates.len(),
                    "error": e.to_string(),
                }),
                crate::telemetry::WarningContext {
                    phase: Some("discover_contradict".into()),
                    role: Some("contradiction_judge".into()),
                    ..Default::default()
                },
            );
            return Ok(Vec::new());
        }
    };
    Ok(parse_findings(&raw))
}

/// Lenient parse of the wire-form wrapper into the typed
/// findings vector. Drops any item whose `pair` is not a
/// 2-element string array (instead of bubbling an error up) so a
/// single bad row cannot blank the entire phase.
pub fn parse_findings(value: &Value) -> Vec<ContradictionFinding> {
    // Collect raw item values first; the wrapper JSON object or
    // a bare array are both acceptable inputs.
    let items: Vec<Value> =
        if let Ok(report) = serde_json::from_value::<ContradictionJudgeReport>(value.clone()) {
            // Re-serialise each typed finding back into a generic
            // Value so the lenient field-by-field parsing below sees
            // the same shape regardless of whether the model returned
            // the typed wrapper or a bare array.
            report
                .findings
                .iter()
                .map(|f| serde_json::to_value(f).unwrap_or(Value::Null))
                .collect()
        } else if let Some(arr) = value.get("findings").and_then(|v| v.as_array()) {
            arr.clone()
        } else if let Some(arr) = value.as_array() {
            arr.clone()
        } else {
            return Vec::new();
        };
    let mut out: Vec<ContradictionFinding> = Vec::with_capacity(items.len());
    for item in items {
        if let Some(f) = ContradictionFinding::from_json(&item) {
            out.push(f);
        }
    }
    // Stable, severity-descending sort so the integrator picks
    // the urgent fixes first.
    out.sort_by_key(|f| std::cmp::Reverse(f.severity.rank()));
    out
}

/// Map a findings vector back to the legacy free-form
/// `severity` string the [`crate::domain::Contradiction`]
/// sidecar expects. Re-exported for use by the phase wire-up;
/// kept here so the severity-remapping story is co-located with
/// the enum definition.
pub fn severity_to_legacy(s: ContradictionSeverity) -> &'static str {
    s.legacy_label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::execution::Parallelism;
    use crate::fs_layout::MoaganHome;
    use crate::ids::RunId;
    use crate::llm::{ProviderRegistry, Response, Role};
    use crate::telemetry::{Telemetry, WarningContext};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Severity ladder: legacy free-form labels must round-trip
    /// through the new enum in both directions.
    #[test]
    fn severity_legacy_labels_and_from_str_lossy_round_trip() {
        for s in [
            ContradictionSeverity::Minor,
            ContradictionSeverity::Major,
            ContradictionSeverity::Critical,
        ] {
            // Round-trip via legacy_label -> from_str_lossy.
            let legacy = s.legacy_label();
            assert_eq!(ContradictionSeverity::from_str_lossy(legacy), s);
        }
        // The actual catalog vocabulary the prompt uses.
        assert_eq!(
            ContradictionSeverity::from_str_lossy("critical"),
            ContradictionSeverity::Critical
        );
        assert_eq!(
            ContradictionSeverity::from_str_lossy("major"),
            ContradictionSeverity::Major
        );
        assert_eq!(
            ContradictionSeverity::from_str_lossy("minor"),
            ContradictionSeverity::Minor
        );
        // Legacy / compact vocabulary still maps cleanly.
        assert_eq!(
            ContradictionSeverity::from_str_lossy("high"),
            ContradictionSeverity::Critical
        );
        assert_eq!(
            ContradictionSeverity::from_str_lossy("medium"),
            ContradictionSeverity::Major
        );
        assert_eq!(
            ContradictionSeverity::from_str_lossy("low"),
            ContradictionSeverity::Minor
        );
        // Unknown labels fall back to Minor instead of erroring.
        assert_eq!(
            ContradictionSeverity::from_str_lossy("???"),
            ContradictionSeverity::Minor
        );
    }

    /// The helper used by the legacy `severity_rank` must keep its
    /// documented ladder so `domain::Contradiction` consumers
    /// continue to see a stable ordering.
    #[test]
    fn severity_rank_orders_canonical_values() {
        assert!(severity_rank("high") > severity_rank("medium"));
        assert!(severity_rank("medium") > severity_rank("low"));
        assert_eq!(severity_rank("unknown"), 0);
    }

    /// `top_pairs` caps the result and handles the empty case.
    #[test]
    fn top_pairs_caps_at_max_n() {
        let d = vec![
            ("c1".into(), "c2".into(), 0.9),
            ("c1".into(), "c3".into(), 0.7),
            ("c2".into(), "c3".into(), 0.5),
        ];
        let top = top_pairs(&d, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "c1");
    }

    #[test]
    fn top_pairs_handles_empty() {
        assert!(top_pairs(&[], 5).is_empty());
    }

    /// `ContradictionRecord` keeps its serde round-trip so the
    /// legacy `contradictions.json` sidecar stays parseable.
    #[test]
    fn contradiction_record_round_trips() {
        let r = ContradictionRecord {
            cluster_a: "cluster_01".into(),
            cluster_b: "cluster_05".into(),
            representatives: vec!["sk_001".into(), "sk_022".into()],
            topic: "consistency".into(),
            description: "ACID vs eventual".into(),
            severity: "high".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: ContradictionRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back.severity, "high");
    }

    /// `user_payload` must echo the focal id and thesis verbatim
    /// so the judge can quote the right sketch in the
    /// `evidence` field.
    #[test]
    fn user_payload_includes_focal_and_candidates() {
        let focal = Sketch {
            id: "sk_001".into(),
            thesis: "alpha".into(),
            key_decisions: vec!["ACID".into()],
            ..Default::default()
        };
        let candidates = vec![
            Sketch {
                id: "sk_002".into(),
                thesis: "beta".into(),
                key_decisions: vec!["eventual".into()],
                ..Default::default()
            },
            Sketch {
                id: "sk_003".into(),
                thesis: "gamma".into(),
                ..Default::default()
            },
        ];
        let s = user_payload(&focal, &candidates);
        assert!(s.contains("sk_001"));
        assert!(s.contains("alpha"));
        assert!(s.contains("ACID"));
        assert!(s.contains("sk_002"));
        assert!(s.contains("eventual"));
        assert!(s.contains("sk_003"));
    }

    /// `parse_findings` accepts the well-formed wrapper and
    /// extracts the findings in stable, severity-descending order.
    #[test]
    fn parse_findings_accepts_wrapper_and_sorts() {
        let v: Value = serde_json::from_str(
            r#"{
                "findings": [
                    {"pair": ["sk_001", "sk_002"], "severity": "minor",
                     "evidence": "thesis mismatch", "suggestion": "align"},
                    {"pair": ["sk_001", "sk_003"], "severity": "critical",
                     "evidence": "ACID vs eventual", "suggestion": "pick one"},
                    {"pair": ["sk_001", "sk_004"], "severity": "major",
                     "evidence": "consensus", "suggestion": ""}
                ],
                "schema_version": "contradiction_judge.v1"
            }"#,
        )
        .unwrap();
        let fs = parse_findings(&v);
        assert_eq!(fs.len(), 3);
        // critical must surface first.
        assert_eq!(fs[0].severity, ContradictionSeverity::Critical);
        assert_eq!(fs[1].severity, ContradictionSeverity::Major);
        assert_eq!(fs[2].severity, ContradictionSeverity::Minor);
    }

    /// `parse_findings` returns an empty vector for a wrapper
    /// that has zero findings — the "no contradiction" path.
    #[test]
    fn parse_findings_returns_empty_for_no_findings() {
        let v: Value =
            serde_json::from_str(r#"{"findings": [], "schema_version": "contradiction_judge.v1"}"#)
                .unwrap();
        assert!(parse_findings(&v).is_empty());
    }

    /// `parse_findings` drops malformed items instead of
    /// returning an error. A single bad row cannot blank the
    /// run.
    #[test]
    fn parse_findings_drops_malformed_items_gracefully() {
        let v: Value = serde_json::from_str(
            r#"{
                "findings": [
                    {"pair": ["sk_001", "sk_002"], "severity": "major",
                     "evidence": "ok", "suggestion": ""},
                    {"pair": ["only_one_id"], "severity": "minor",
                     "evidence": "bad", "suggestion": ""},
                    {"pair": ["sk_001", 42], "severity": "minor",
                     "evidence": "bad", "suggestion": ""},
                    "not-an-object"
                ],
                "schema_version": "contradiction_judge.v1"
            }"#,
        )
        .unwrap();
        let fs = parse_findings(&v);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].pair[0], "sk_001");
        assert_eq!(fs[0].pair[1], "sk_002");
    }

    /// `parse_findings` falls through to a graceful empty
    /// vector when the wrapper itself is malformed (not just
    /// individual items). This is what the detector hits on
    /// a model that returns an inline array instead of the
    /// wrapper object.
    #[test]
    fn parse_findings_tolerates_bare_array_wrapper() {
        let v: Value = serde_json::from_str(
            r#"[
                {"pair": ["sk_001", "sk_002"], "severity": "minor",
                 "evidence": "ok", "suggestion": ""}
            ]"#,
        )
        .unwrap();
        let fs = parse_findings(&v);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].pair[1], "sk_002");
    }

    /// Mock LLM provider used to exercise the happy-path,
    /// malformed, and empty-payload branches of
    /// `find_contradictions_against` without spinning up a real
    /// provider. Mirrors the harness in `src/discovery/persona_angle.rs`.
    struct MockProvider {
        outcomes: parking_lot::Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: parking_lot::Mutex::new(responses),
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl crate::llm::Provider for MockProvider {
        fn name(&self) -> &str {
            "mock-contradiction-judge"
        }
        fn model(&self) -> &str {
            "mock-model"
        }
        fn endpoint(&self) -> &str {
            "mock://contradiction-judge"
        }
        async fn send(&self, _req: &crate::llm::Request) -> Result<(u16, Response)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self
                .outcomes
                .lock()
                .pop()
                .expect("MockProvider was drained");
            Ok((
                200,
                Response {
                    text,
                    finish_reason: Some("end_turn".into()),
                    truncated: false,
                    usage: Default::default(),
                },
            ))
        }
    }

    fn build_ctx(provider: Arc<MockProvider>) -> (TempDir, RunContext) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
        home.ensure().unwrap();
        let mut registry = ProviderRegistry::default();
        registry.insert("mock".into(), provider.clone());
        let ctx = RunContext::new_with_config(
            RunId::new(),
            home,
            Arc::new(registry),
            "mock".to_owned(),
            "mock-model".to_owned(),
            Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            "standard".to_owned(),
            Arc::new(Config::default()),
        );
        (tmp, ctx)
    }

    /// Happy path: the mock returns one well-formed finding
    /// against the focal sketch; the detector surfaces exactly
    /// one `ContradictionFinding`.
    #[tokio::test]
    async fn find_contradictions_against_happy_path_returns_one_finding() {
        let mock = MockProvider::new(vec![
            r#"{
            "findings": [
                {"pair": ["sk_001", "sk_002"], "severity": "major",
                 "evidence": "thesis mismatch", "suggestion": "align"}
            ],
            "schema_version": "contradiction_judge.v1"
        }"#
            .to_owned(),
        ]);
        let (_tmp, ctx) = build_ctx(mock.clone());
        let focal = Sketch {
            id: "sk_001".into(),
            thesis: "alpha".into(),
            ..Default::default()
        };
        let candidates = vec![Sketch {
            id: "sk_002".into(),
            thesis: "beta".into(),
            ..Default::default()
        }];
        let findings = find_contradictions_against(&ctx, &focal, &candidates)
            .await
            .unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].pair, ["sk_001", "sk_002"]);
        assert_eq!(findings[0].severity, ContradictionSeverity::Major);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    /// Empty-findings path: the mock returns an explicit empty
    /// findings array (the "nothing contradicts this sketch"
    /// response); the detector surfaces zero findings.
    #[tokio::test]
    async fn find_contradictions_against_zero_findings_returns_empty() {
        let mock = MockProvider::new(vec![
            r#"{
            "findings": [],
            "schema_version": "contradiction_judge.v1"
        }"#
            .to_owned(),
        ]);
        let (_tmp, ctx) = build_ctx(mock.clone());
        let focal = Sketch {
            id: "sk_001".into(),
            thesis: "alpha".into(),
            ..Default::default()
        };
        let candidates = vec![Sketch {
            id: "sk_002".into(),
            thesis: "beta".into(),
            ..Default::default()
        }];
        let findings = find_contradictions_against(&ctx, &focal, &candidates)
            .await
            .unwrap();
        assert!(findings.is_empty());
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    /// Empty candidate list short-circuits before any LLM
    /// call so the detector never burns budget on a question
    /// the model cannot answer.
    #[tokio::test]
    async fn find_contradictions_against_empty_candidates_short_circuits() {
        let mock = MockProvider::new(vec![]);
        let (_tmp, ctx) = build_ctx(mock.clone());
        let focal = Sketch {
            id: "sk_001".into(),
            thesis: "alpha".into(),
            ..Default::default()
        };
        let findings = find_contradictions_against(&ctx, &focal, &[])
            .await
            .unwrap();
        assert!(findings.is_empty());
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    }

    /// Malformed JSON path: the mock returns raw text that is
    /// not a JSON object. The detector must NOT panic; it
    /// must return zero findings and continue. Operators
    /// can correlate the drop via the
    /// `discovery.contradiction_judge.parse_fallback`
    /// warning that the helper emits.
    #[tokio::test]
    async fn find_contradictions_against_malformed_json_falls_back_gracefully() {
        let mock = MockProvider::new(vec!["this is not json at all { ]".to_owned()]);
        let (_tmp, ctx) = build_ctx(mock.clone());
        let focal = Sketch {
            id: "sk_001".into(),
            thesis: "alpha".into(),
            ..Default::default()
        };
        let candidates = vec![Sketch {
            id: "sk_002".into(),
            thesis: "beta".into(),
            ..Default::default()
        }];
        let findings = find_contradictions_against(&ctx, &focal, &candidates)
            .await
            .unwrap();
        assert!(findings.is_empty());
        // Mock was called but every parse attempt failed; the
        // detector still returned cleanly.
        assert!(mock.calls.load(Ordering::SeqCst) >= 1);
        // Silence the unused-warning-context import on rustc.
        let _ = WarningContext::default();
    }

    // `Role::ContradictionJudge` smoke test: the role must keep
    // its public-as_str and round-trip contract regardless of
    // what the detector does. Pins the enum-membership of the
    // new role so a future removal is flagged.
    #[test]
    fn role_contradiction_judge_is_well_known() {
        assert_eq!(Role::ContradictionJudge.as_str(), "contradiction_judge");
    }
}
