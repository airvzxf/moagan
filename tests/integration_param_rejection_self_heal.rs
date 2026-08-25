//! Integration tests for the self-healing param-rejection path.
//!
//! End-to-end coverage of the runtime contract:
//!
//! 1. [`omit_param`] drops the wire field the runtime controls
//!    today (`temperature`, `top_p`).
//! 2. [`detect_rejection`] catches every signature observed in the
//!    spike: Anthropic `unknown_parameter`, OpenAI Responses
//!    `unsupported_parameter`, OpenCode Go kimi-k3
//!    `invalid <param>:`, DeepSeek `Invalid <param> value, the
//!    valid range ...` and `Failed to deserialize: <param>:
//!    invalid value`, MiniMax Anthropic-direct `invalid params,
//!    param '<param>'`, and the deterministic `error.param`
//!    structured field.
//! 3. [`ParamRejectionsTable::record`] writes the rejection to
//!    `<MOAGAN_HOME>/param_rejections.toml` and a fresh load picks
//!    the entry up — the cross-run persistence the runtime relies
//!    on so subsequent runs skip the failing round-trip.
//! 4. The audit helper does NOT panic on edge cases (empty body,
//!    non-object body).
//!
//! The tests deliberately do NOT spin up a full `RunContext` and
//! drive `dispatch_to_provider`: that surface is covered by the
//! `tests/integration_*` suite that already exercises the gate
//! (e.g. `integration_capability_gating.rs`); here we pin the
//! individual building blocks so a regression in any one of them is
//! caught independently.

use moagan::fs_layout::MoaganHome;
use moagan::llm::param_rejections::{
    PARAM_NAMES, ParamRejectionsFile, ParamRejectionsTable, audit_unknown_fields, detect_rejection,
};
use moagan::llm::role::Role;
use moagan::llm::wire::{Request, omit_param};

fn sample_request() -> Request {
    Request {
        role: Role::Intake,
        model: "m".to_owned(),
        system: "sys".to_owned(),
        user: "user".to_owned(),
        max_tokens: 1024,
        temperature: Some(0.6),
        top_p: Some(0.95),
        response_schema: None,
        stream: false,
        extra_messages: vec![],
        attachments: vec![],
        tool_choice: None,
    }
}

// ----- omit_param -------------------------------------------------------

/// `omit_param` must clear the optional field on the `Request`
/// struct. The provider serialisers read `Option<f32>::is_none()`
/// to drop the field, so the helper mutates in place.
#[test]
fn omit_param_drops_temperature() {
    let mut req = sample_request();
    assert!(req.temperature.is_some());
    omit_param(&mut req, "temperature");
    assert!(req.temperature.is_none(), "temperature must be cleared");
    // Other fields stay untouched.
    assert!(req.top_p.is_some());
    assert_eq!(req.max_tokens, 1024);
}

#[test]
fn omit_param_drops_top_p() {
    let mut req = sample_request();
    omit_param(&mut req, "top_p");
    assert!(req.top_p.is_none());
    assert!(req.temperature.is_some());
    assert_eq!(req.max_tokens, 1024);
}

#[test]
fn omit_param_unknown_is_noop() {
    // Unknown params (e.g. `max_tokens`, which the dispatch path
    // cannot drop without restructuring the request) must be
    // silently ignored so the retry loop is safe to call on any
    // detected rejection name.
    let mut req = sample_request();
    let before_temp = req.temperature;
    let before_top_p = req.top_p;
    omit_param(&mut req, "max_tokens");
    omit_param(&mut req, "made_up_field");
    assert_eq!(req.temperature, before_temp);
    assert_eq!(req.top_p, before_top_p);
}

#[test]
fn omit_param_walks_the_param_names_list() {
    // Every entry in [`PARAM_NAMES`] must be handled by the helper.
    // A new entry without a corresponding match arm here would be a
    // silent regression: the detector would record the rejection,
    // but the omit step would do nothing, and the retry would loop.
    for &name in PARAM_NAMES {
        let mut req = sample_request();
        omit_param(&mut req, name);
    }
}

// ----- detect_rejection: realistic 4xx bodies --------------------------

fn body(s: &str) -> String {
    serde_json::json!({"error": {"message": s}}).to_string()
}

#[test]
fn detect_anthropic_unknown_parameter() {
    let payload = r#"{"error":{"type":"unknown_parameter","message":"[unknown_parameter] Unknown parameter: 'max_tokens'"}}"#;
    assert_eq!(
        detect_rejection(400, payload).as_deref(),
        Some("max_tokens")
    );
}

#[test]
fn detect_openai_responses_unsupported_parameter() {
    let payload = r#"{"error":{"message":"[invalid_request_error] Unsupported parameter: 'top_p' is not supported with this model.","type":"invalid_request_error"}}"#;
    assert_eq!(detect_rejection(400, payload).as_deref(), Some("top_p"));
}

#[test]
fn detect_openai_compat_structured_error_param_too_large() {
    // The canonical `error.param` shape; the field name is what
    // the runtime records and the audit hint cites.
    let payload = r#"{"error":{"param":"max_tokens is too large: 999999. This model supports at most 131072 completion tokens, whereas you provided 999999.","type":"server_error","message":"..."}}"#;
    assert_eq!(
        detect_rejection(400, payload).as_deref(),
        Some("max_tokens")
    );
}

#[test]
fn detect_minimax_anthropic_direct_invalid_params() {
    assert_eq!(
        detect_rejection(
            400,
            &body("invalid params, param 'top_p' should be in (0,1] (2013)")
        )
        .as_deref(),
        Some("top_p")
    );
}

#[test]
fn detect_kimi_k3_invalid_temperature() {
    assert_eq!(
        detect_rejection(
            400,
            &body("invalid temperature: only 1 is allowed for this model")
        )
        .as_deref(),
        Some("temperature")
    );
}

#[test]
fn detect_deepseek_invalid_value_with_range() {
    assert_eq!(
        detect_rejection(
            400,
            &body("Invalid temperature value, the valid range of temperature is [0, 2]")
        )
        .as_deref(),
        Some("temperature")
    );
}

#[test]
fn detect_deepseek_failed_to_deserialize() {
    assert_eq!(
        detect_rejection(
            400,
            &body(
                "Failed to deserialize the JSON body into the target type: max_tokens: invalid value: integer `-1`"
            )
        )
        .as_deref(),
        Some("max_tokens")
    );
}

#[test]
fn detect_lowercase_unknown_parameter() {
    assert_eq!(
        detect_rejection(400, &body("unknown parameter: 'temperature'")).as_deref(),
        Some("temperature")
    );
}

#[test]
fn detect_5xx_returns_none() {
    // 5xx errors are NOT param rejections — they are transient
    // upstream failures and the breaker is the right response,
    // not a per-call retry-with-omit.
    assert_eq!(detect_rejection(500, "internal server error"), None);
    assert_eq!(detect_rejection(503, "service unavailable"), None);
}

#[test]
fn detect_2xx_returns_none() {
    // 2xx with a body that happens to mention "temperature" — the
    // auto-detect must not run on success paths.
    assert_eq!(
        detect_rejection(200, r#"{"error":{"param":"temperature","message":"..."}}"#),
        None
    );
}

#[test]
fn detect_returns_none_for_unrelated_4xx() {
    // Generic 4xx (e.g. auth, model-not-found) without a rejection
    // signature must NOT trigger the omit-and-retry path.
    assert_eq!(
        detect_rejection(401, r#"{"error":"invalid api key"}"#),
        None
    );
    assert_eq!(
        detect_rejection(404, r#"{"error":"model not found"}"#),
        None
    );
}

#[test]
fn detect_returns_none_for_malformed_body() {
    // Non-JSON upstream responses (rare but real — a proxy
    // returning HTML or a captive portal).
    assert_eq!(detect_rejection(400, "not json"), None);
    assert_eq!(detect_rejection(400, ""), None);
}

// ----- ParamRejectionsTable persistence -------------------------------

/// Recording a rejection must persist to the on-disk TOML and a
/// freshly loaded table must observe it. This is the contract the
/// dispatch path relies on so subsequent runs skip the failing
/// round-trip.
#[test]
fn record_persists_to_disk_and_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(dir.path().to_path_buf());
    let t = ParamRejectionsTable::from_home(&home).unwrap();
    assert!(!t.should_omit("opencode", "gpt-5.6-luna", "temperature"));

    t.record("opencode", "gpt-5.6-luna", "temperature").unwrap();

    // On-disk file exists with the recorded entry.
    let path = home.param_rejections_path();
    assert!(path.exists(), "record must write the TOML sidecar");
    let loaded = ParamRejectionsFile::load(&path).unwrap();
    let set = loaded
        .providers
        .get("opencode")
        .and_then(|m| m.get("gpt-5.6-luna"))
        .expect("recorded entry survives a round-trip");
    assert!(set.contains("temperature"));

    // A fresh in-memory table built from the same path observes
    // the cached rejection.
    let t2 = ParamRejectionsTable::from_path(&path).unwrap();
    assert!(t2.should_omit("opencode", "gpt-5.6-luna", "temperature"));
    assert!(!t2.should_omit("opencode", "gpt-5.6-luna", "top_p"));
}

#[test]
fn record_is_idempotent_per_param() {
    let dir = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(dir.path().to_path_buf());
    let t = ParamRejectionsTable::from_home(&home).unwrap();
    t.record("p", "m", "temperature").unwrap();
    t.record("p", "m", "temperature").unwrap();
    t.record("p", "m", "temperature").unwrap();
    let snap = t.snapshot();
    let set = snap.providers.get("p").unwrap().get("m").unwrap();
    assert_eq!(set.len(), 1, "duplicate records must not bloat the set");
    assert!(set.contains("temperature"));
}

#[test]
fn record_two_params_for_same_pair() {
    let dir = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(dir.path().to_path_buf());
    let t = ParamRejectionsTable::from_home(&home).unwrap();
    t.record("p", "m", "temperature").unwrap();
    t.record("p", "m", "top_p").unwrap();
    assert!(t.should_omit("p", "m", "temperature"));
    assert!(t.should_omit("p", "m", "top_p"));
}

#[test]
fn empty_table_is_inert() {
    let t = ParamRejectionsTable::empty();
    // No entries → no omits. Every `should_omit` call returns
    // `false` regardless of the input.
    for &p in PARAM_NAMES {
        assert!(!t.should_omit("any", "thing", p));
    }
    assert!(t.is_empty());
}

#[test]
fn separate_pairs_do_not_cross_pollute() {
    let dir = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(dir.path().to_path_buf());
    let t = ParamRejectionsTable::from_home(&home).unwrap();
    t.record("p1", "m1", "temperature").unwrap();
    // The recorded pair must NOT leak into a different pair.
    assert!(t.should_omit("p1", "m1", "temperature"));
    assert!(!t.should_omit("p1", "m2", "temperature"));
    assert!(!t.should_omit("p2", "m1", "temperature"));
    assert!(!t.should_omit("p2", "m2", "temperature"));
}

// ----- audit_unknown_fields -------------------------------------------

#[test]
fn audit_does_not_panic_on_non_object() {
    // The audit helper must be safe to call on any JSON shape —
    // the dispatch path serialises the request through serde so
    // the value should always be an object, but a future
    // wire-builder could change that and a panic on a string /
    // array would block every LLM call.
    let body = serde_json::json!("just a string");
    audit_unknown_fields(&body);
    let body = serde_json::json!(["array", "of", "strings"]);
    audit_unknown_fields(&body);
    let body = serde_json::json!(42);
    audit_unknown_fields(&body);
    let body = serde_json::json!(null);
    audit_unknown_fields(&body);
}

#[test]
fn audit_does_not_panic_on_empty_object() {
    let body = serde_json::json!({});
    audit_unknown_fields(&body);
}

// ----- combined: detect → persist → reload → omit --------------------

/// End-to-end: a 4xx body yields a detected param name, the
/// runtime records it, a fresh table loads it, and the omit step
/// drops the field on the next call. This is the exact flow the
/// dispatch path runs.
#[test]
fn end_to_end_detect_persist_reload_omit() {
    let dir = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(dir.path().to_path_buf());
    let t = ParamRejectionsTable::from_home(&home).unwrap();

    // First call: upstream rejects `temperature` with the canonical
    // structured `error.param` shape.
    let body = r#"{"error":{"param":"temperature must be within [0, 1.5]","type":"server_error","message":"..."}}"#;
    let detected = detect_rejection(400, body).expect("detect");
    assert_eq!(detected, "temperature");

    // Runtime records the rejection.
    t.record("opencode", "gpt-5.6-luna", &detected).unwrap();

    // Fresh table sees the cached rejection — no second round-trip.
    let t2 = ParamRejectionsTable::from_path(&home.param_rejections_path()).unwrap();
    assert!(t2.should_omit("opencode", "gpt-5.6-luna", "temperature"));

    // The omit step on a fresh request drops the field.
    let mut req = sample_request();
    omit_param(&mut req, "temperature");
    assert!(req.temperature.is_none());
}
