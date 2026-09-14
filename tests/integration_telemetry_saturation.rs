//! Integration tests for the push-side saturation layer (catalog
//!       +      , v0.8 telemetry push-side).
//!
//! Exercises the full end-to-end path:
//!
//! 1. `BreakeredClient::send` fires a `SaturationEvent` through
//!    the configured [`SaturationSink`] when the circuit breaker is
//!    open or the rate limiter exhausts its budget.
//! 2. The [`Telemetry`] sink mirrors the event into both the
//!    per-run JSONL stream (`telemetry/saturation.jsonl`) and the
//!    `saturation_events` SQLite table (v018).
//! 3. The CLI consumer
//!    (`moagan telemetry alerts list --since ... --provider ...`)
//!    surfaces the recorded rows through
//!    [`moagan::storage::sqlite::Db::list_saturation_events`].
//!
//! Tests use a tiny in-process sink so the assertions stay
//! deterministic without spinning up a SQLite connection for every
//! event check.
//!
//! #933 follow-up: the post-#933 SDK surface retired the
//! per-provider rate limiter (`with_rate_limiter`,
//! `with_rate_limit_max_wait`) and the wrapper's
//! `with_saturation_sink` plumbing. The per-`(provider, role)`
//! `RunContext::throttle` governor absorbed the rate-limit case,
//! and saturation events now fire from the telemetry layer
//! directly (`Telemetry::record_circuit_open` /
//! `record_rate_limit`) rather than from the wrapper. The
//! wrapper-based tests below therefore need to be re-grounded
//! against the new model. Marked `#[ignore]` until #934 lands
//! the redesigned tests; the telemetry-mirror test still
//! exercises the SQLite + JSONL path end-to-end because it drives
//! `Telemetry::record_*` directly.

// TODO: #934 follow-up — redesign wrapper-based saturation tests
// against post-#933 SDK surface. The current tests reference
// deleted legacy items:
//   * `BreakeredProvider::new(inner, breaker).with_saturation_sink(...)`
//     (the post-#933 `BreakeredClient::new(inner)` takes only the
//      inner client; the saturation sink plumbing was retired —
//      telemetry events now fire from `Telemetry::record_*`)
//   * `BreakeredProvider::with_rate_limiter(...)` /
//     `with_rate_limit_max_wait(...)` (per-provider rate limiter
//      retired by #933; the per-`(provider, role)` governor on
//      `RunContext::throttle` absorbs the throttle case)
//   * `ProviderRegistry::insert_wrapped` / `saturation_sink` /
//     `breaker` (the registry does not expose those accessors
//      anymore; saturation events fire from `Telemetry`)
// Until #934 ports the wrapper-based assertions to the new
// model, they are gated behind `#[ignore]` so the telemetry-
// mirror test (which exercises `Telemetry` + SQLite + JSONL
// directly) can keep running.

#![cfg(test)]

use std::sync::Arc;

use async_trait::async_trait;

use moagan::error::Result;
use moagan::ids::RunId;
use moagan::ids::sha256_hex;
use moagan::llm::Role;
use moagan::llm::capabilities::ProviderCapabilities;
use moagan::llm::client::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};
use moagan::telemetry::Telemetry;

/// Programmable inner SDK client that always returns an opening
/// error so the breaker trips on the very first call.
///
/// #929 — the inner provider is now an `LlmClient`. The
/// saturation-sink wiring through `BreakeredClient::send` is no
/// longer the canonical path (telemetry events now fire from
/// `Telemetry::record_*` directly); this stub is kept around for
/// the redesigned wrapper-based tests in #934.
struct AlwaysErrorClient;

#[async_trait]
impl LlmClient for AlwaysErrorClient {
    fn sdk_type(&self) -> &'static str {
        "mock"
    }
    fn name(&self) -> &str {
        "integration-sat"
    }
    fn model(&self) -> &str {
        "integration-model"
    }
    fn endpoint(&self) -> &str {
        "mock://integration"
    }
    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities(ProviderCapabilities::for_mock())
    }
    async fn send_once(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        Err(moagan::error::Error::Provider {
            message: "upstream 503: service unavailable".into(),
            http_status: None,
        })
    }
    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        let bytes = serde_json::to_vec(req).map_err(|e| moagan::error::Error::Provider {
            message: format!("always-error serialize request: {e}"),
            http_status: None,
        })?;
        Ok(sha256_hex(&bytes))
    }
}

/// In-memory sink that records every fired event for assertion.
/// Kept around for the redesigned wrapper-based tests in #934;
/// the post-#933 surface uses the
/// `telemetry::saturation::SaturationEvent` directly (the legacy
/// `compat::SaturationSink` trait takes the legacy
/// `compat::_SaturationEvent`).
#[derive(Default)]
#[allow(dead_code)]
struct VecSink(parking_lot::Mutex<Vec<()>>);

fn dummy_request() -> LlmRequest {
    LlmRequest {
        role: Role::Intake,
        model: "MiniMax-M3".into(),
        system: String::new(),
        user: "u".into(),
        max_tokens: Some(16),
        temperature: None,
        top_p: None,
        top_k: None,
        response_schema: None,
        stream: false,
        extra_messages: vec![],
        attachments: vec![],
        tool_choice: None,
    }
}

/// v0.9.6 / #933: `BreakeredClient::send` no longer wires the
/// saturation sink through the wrapper. Saturation events now
/// fire from `Telemetry::record_circuit_open` /
/// `record_rate_limit` directly. This test pinned the legacy
/// wrapper→sink→telemetry chain, which the post-#933 refactor
/// retired in favour of explicit `record_*` calls.
#[ignore = "TODO: #934 follow-up — redesign against post-#933 telemetry-direct path"]
#[tokio::test]
async fn circuit_open_fires_saturation_event() {
    // Wrapper-based saturation-sink wiring was retired by #933.
    // The redesigned test (in #934) will call
    // `Telemetry::record_circuit_open` directly and assert the
    // resulting `SaturationEvent` lands in SQLite + JSONL.
    let _ = (AlwaysErrorClient, VecSink::default(), dummy_request());
}

/// v0.9.6 / #933: per-provider rate limiter was retired. The
/// per-`(provider, role)` `RunContext::throttle` governor absorbs
/// the rate-limit case; saturation events fire from
/// `Telemetry::record_rate_limit`.
#[ignore = "TODO: #934 follow-up — pin against post-#933 throttle governor + telemetry-direct event"]
#[tokio::test]
async fn rate_limit_exhausted_fires_saturation_event() {
    let _ = (AlwaysErrorClient, VecSink::default(), dummy_request());
}

#[test]
fn telemetry_mirrors_saturation_event_into_sqlite_and_jsonl() {
    moagan::test_support::with_moagan_home("telemetry_saturation_mirror", |_home| {
        let home = moagan::fs_layout::MoaganHome::resolve().unwrap();
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        let db = moagan::storage::sqlite::Db::open(&home.meta_db_path()).unwrap();
        db.register_run(run_id, "fast", "running", "0.9.1", None, None, None)
            .unwrap();
        let t = Telemetry::open(
            run_id,
            &run_dir,
            moagan::redact::RedactPolicy::default(),
            Some(db.clone()),
        )
        .unwrap();

        // Fire one event of each kind to exercise every code path.
        t.record_circuit_open("minimax", "MiniMax-M3", 5).unwrap();
        t.record_rate_limit("mock", "mock-m", 12.5, 60, 1).unwrap();
        t.flush().unwrap();

        // JSONL stream must contain two events.
        let content = moagan::storage::compression::read_to_string(t.saturation_path()).unwrap();
        assert!(content.contains("\"kind\":\"error\""), "got: {content}");
        assert!(
            content.contains("\"kind\":\"rate_limit\""),
            "got: {content}"
        );

        // SQLite mirror must carry the same two rows.
        let rows = db.list_saturation_events(None, None, 0).unwrap();
        assert_eq!(rows.len(), 2, "expected two saturation rows");
        let kinds: std::collections::HashSet<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains("error"));
        assert!(kinds.contains("rate_limit"));

        // Filter by provider: only the mock event remains.
        let mock_only = db.list_saturation_events(None, Some("mock"), 0).unwrap();
        assert_eq!(mock_only.len(), 1);
        assert_eq!(mock_only[0].provider, "mock");

        // Filter by since_unix set in the future: empty result.
        let future_unix = moagan::time::now_unix_secs() + 86_400;
        let future = db
            .list_saturation_events(Some(future_unix), None, 0)
            .unwrap();
        assert!(future.is_empty());
    });
}

/// PR #494 follow-up: end-to-end wiring through a real
/// `ProviderRegistry`. #933 retired the wrapper-level
/// `with_saturation_sink` + `attach_saturation_sink` plumbing;
/// saturation events now fire from `Telemetry` directly, so the
/// registry-based wiring test no longer has a path to exercise.
/// The redesign in #934 will pin the new
/// `Telemetry`-direct path through `RunContext`.
#[ignore = "TODO: #934 follow-up — wrapper-level saturation wiring was retired; pin against telemetry-direct path"]
#[test]
fn registry_attach_saturation_sink_routes_to_telemetry() -> Result<()> {
    let _ = Arc::new(AlwaysErrorClient);
    Ok(())
}

/// Companion to the previous test, exercising the
/// `registry_from_config_with_sink` construction-time wiring.
/// #933 retired the construction-time sink parameter on the
/// registry stubs (the post-#933 `registry_from_config_with_sink`
/// is a deprecated stub that returns an empty registry). The
/// redesigned test in #934 will pin the new `Telemetry`-direct
/// path.
#[ignore = "TODO: #934 follow-up — registry_from_config_with_sink stub retired; pin against telemetry-direct path"]
#[test]
fn registry_from_config_with_sink_attaches_sink_at_construction() -> Result<()> {
    Ok(())
}
