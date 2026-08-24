//! Wiremock integration test: every probe HTTP request the
//! temperature auto-probe fires against `MinimaxProvider`
//! must carry `max_tokens >= 1024` in the wire body.
//!
//! ## Why this test exists
//!
//! The MiniMax M-series and OpenCode Go / MiMo fleet spend the
//! first ~115 tokens of an output budget on a thinking pass
//! and only emit text afterwards. With
//! `max_tokens = 16` (the historical probe floor) the model
//! exhausts the budget on thinking and never emits text, so the
//! upstream returns HTTP 200 with `content: null` and the
//! discovered temperature set collapses to empty. With
//! `max_tokens = 1024` (the value this test pins) every probe
//! has enough budget for the thinking pass plus the trailing
//! `1` and the algorithm discovers the full
//! `[0.0, 0.1, ..., 2.0]` set.
//!
//! ## What it pins
//!
//! - Every wire body the temperature probe sends to the upstream
//!   carries `max_tokens >= 1024`.
//! - Exactly 21 HTTP round-trips (one per candidate in
//!   [`moagan::llm::temperature_probe::TEMPERATURE_PROBE_VALUES`]),
//!   no retries. The pre-fix `MinimaxProvider::send_probe`
//!   re-issued each probe up to `max_retries = 3` times, so a
//!   failing upstream could blow this to 63 round-trips per
//!   run.
//!
//! Mirrors `tests/integration_probe_temperature.rs`. The
//! difference is the focus: that test pins the algorithm's
//! discovered-set semantics (boundary, accept-all, reject-all,
//! truncated-body, empty-body-no-truncation); this one pins the
//! transport-level wire body the algorithm actually emits.

use std::sync::Arc;

use moagan::config::ProviderConfig;
use moagan::llm::minimax::MinimaxProvider;
use moagan::llm::temperature_probe::{
    ProviderTemperatureProbeTransport, TEMPERATURE_PROBE_BATCH_SIZE, TEMPERATURE_PROBE_VALUES,
    TemperatureProbeTransport, TemperatureTable,
};
use moagan::secret::SecretString;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a `MinimaxProvider` pointed at the mock server URI.
/// `with_max_retries(3)` mirrors the production default so the
/// no-retry assertion below catches a regression where the
/// probe path accidentally re-introduces the retry loop.
fn build_minimax_provider(server_uri: String) -> Arc<MinimaxProvider> {
    let cfg = ProviderConfig {
        models: Vec::new(),
        endpoint: Some(server_uri),
        temperature: None,
        top_p: None,
        omit_max_tokens: false,
        plan: None,
        max_token_auto: None,
        max_token_auto_enabled: None,
        max_token_auto_save: true,
    };
    Arc::new(
        MinimaxProvider::new(&cfg, SecretString::new("sk-test".to_owned()))
            .expect("MinimaxProvider::new should accept the test config")
            .with_max_retries(3),
    )
}

fn wrap_transport(provider: Arc<MinimaxProvider>) -> Arc<dyn TemperatureProbeTransport> {
    Arc::new(
        ProviderTemperatureProbeTransport::new(provider)
            .expect("ProviderTemperatureProbeTransport::new should accept the provider"),
    )
}

/// Wiremock that accepts every request and emits a 200 with
/// `content: ["1"]`. The shape follows the canonical
/// Anthropic-compat envelope so the classifier lands every
/// probe on the `Accepted` branch — the only setup where the
/// algorithm converges to the full 21-element set without
/// firing `retry_once_on_indeterminate`. We want the simple
/// "21 requests in, 21 out" case so the wire-body assertion
/// below is not contaminated by retries from
/// `Indeterminate` re-probes.
async fn mount_accept_all(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "1"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            }
        })))
        .mount(server)
        .await;
}

/// Every probe request the temperature auto-probe fires
/// against `MinimaxProvider` must carry
/// `max_tokens >= 1024` in the wire body. Pins the
/// `PROBE_MIN_OUTPUT_TOKENS` constant — a regression to a
/// small value (e.g. 16) would let the upstream emit
/// `content: null` after the thinking pass and collapse the
/// discovered set to empty. Also pins the no-retry contract:
/// exactly 21 HTTP round-trips regardless of the configured
/// `max_retries = 3`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_requests_carry_min_output_tokens_and_do_not_retry() {
    let server = MockServer::start().await;
    mount_accept_all(&server).await;
    let transport = wrap_transport(build_minimax_provider(server.uri()));

    let table = TemperatureTable::empty();
    let discovered = table
        .probe_and_store(
            "minimax",
            "MiniMax-M3",
            transport,
            TEMPERATURE_PROBE_BATCH_SIZE,
        )
        .await
        .expect("probe converges");

    // Sanity check: the algorithm discovered the full set.
    // This is what the upstream would surface when
    // `max_tokens >= 1024` gives the model enough budget to
    // emit `1` after the thinking pass.
    assert_eq!(
        discovered.len(),
        TEMPERATURE_PROBE_VALUES.len(),
        "accept-all wiremock must surface all 21 candidates when the probe budget is large enough"
    );

    // Wire-body assertion: every request the temperature probe
    // fired must carry `max_tokens >= 1024`. We pull the bodies
    // from the wiremock recorder rather than a custom closure
    // because wiremock's `received_requests()` is the only
    // surface that survives an arbitrary number of probes.
    let received = server
        .received_requests()
        .await
        .expect("wiremock must record received requests");

    assert_eq!(
        received.len(),
        TEMPERATURE_PROBE_VALUES.len(),
        "the temperature probe must issue exactly one HTTP request per \
         candidate ({}), no retries; got {} requests",
        TEMPERATURE_PROBE_VALUES.len(),
        received.len(),
    );

    for (i, req) in received.iter().enumerate() {
        let body: serde_json::Value = serde_json::from_slice(&req.body)
            .unwrap_or_else(|e| panic!("probe {i} body must be valid JSON: {e}"));
        let max_tokens = body
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| panic!("probe {i} body must carry max_tokens: {body}"));
        assert!(
            max_tokens >= 1024,
            "probe {i} must carry max_tokens >= 1024 so the upstream has \
             enough budget for the thinking pass plus the trailing `1`; got \
             max_tokens={max_tokens} in body: {body}",
        );
        // The probe also pins `temperature` to the candidate
        // the algorithm is testing. The wiremock ignores it,
        // but the body assertion doubles as a sanity check that
        // the loop iterated correctly.
        assert!(
            body.get("temperature").is_some(),
            "probe {i} must carry the candidate temperature; got: {body}",
        );
    }
}

/// `MinimaxProvider::send_probe` must NOT honour
/// `self.max_retries`. The probe path bypasses the retry
/// loop because (a) a 4xx IS the algorithm's signal and
/// (b) the algorithm's own `retry_once_on_indeterminate`
/// already covers transient blips. This test mounts a
/// wiremock that returns HTTP 503 (a retryable error) and
/// verifies the probe makes exactly one HTTP request
/// regardless of `self.max_retries = 3`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_does_not_retry_on_transient_5xx() {
    use moagan::llm::provider::Provider;
    use moagan::llm::role::Role;
    use moagan::llm::wire::Request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_responder = calls.clone();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &wiremock::Request| {
            calls_for_responder.fetch_add(1, Ordering::SeqCst);
            // 503 is retryable per the
            // `send_http_with_retries` classifier.
            ResponseTemplate::new(503).set_body_string("upstream unavailable")
        })
        .mount(&server)
        .await;

    let provider = build_minimax_provider(server.uri());
    let req = Request {
        model: "MiniMax-M3".into(),
        role: Role::Intake,
        system: String::new(),
        user: "Reply with the single character: 1".into(),
        max_tokens: 1024,
        temperature: Some(0.5),
        top_p: None,
        response_schema: None,
        stream: false,
        extra_messages: vec![],
        attachments: vec![],
        tool_choice: None,
    };

    let result = provider.send_probe(&req).await;
    assert!(
        result.is_err(),
        "send_probe must surface the 503 error to the caller, got {result:?}",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "send_probe must issue exactly 1 HTTP request even on retryable 5xx; \
         the configured max_retries=3 must NOT trigger on the probe path. \
         Got {} requests",
        calls.load(Ordering::SeqCst),
    );
}
