//! Wiremock integration tests for the top_p and top_k auto-probes
//! (closes #930 D7).
//!
//! These tests stand up a real HTTP `MockServer`, build an
//! `Arc<MinimaxProvider>` against it, and run the top_p / top_k
//! probes end-to-end through `detect_supported_top_p_values` /
//! `detect_supported_top_k_values`. The goal is to catch
//! regressions where the algorithm drifts away from the contract
//! documented in `src/llm/top_p_probe.rs` and
//! `src/llm/top_k_probe.rs`:
//!
//! - `top_p`: 20 candidates (`[0.05, 0.10, ..., 1.00]`) probed in
//!   batches of 3.
//! - `top_k`: 11 candidates (`[1, 2, 4, ..., 1024]`) probed in
//!   batches of 3.
//!
//! Mirrors `tests/integration_probe_temperature.rs` — same
//! wiremock / scripted LLM client shape, same `MinimaxProvider`
//! transport, same end-to-end `probe_and_store` chain. The
//! probe under inspection differs in the request body field
//! (`top_p` / `top_k` vs `temperature`) and the rejection
//! signature (`top_p must be between 0 and 1` /
//! `top_k must be a positive integer` vs `temperature must be
//! between 0 and 2`).

use std::sync::Arc;

use moagan::config::ProviderConfig;
use moagan::llm::client::{LlmClient, ProviderLlmClient};
use moagan::llm::minimax::MinimaxProvider;
use moagan::llm::top_k_probe::{
    TOP_K_PROBE_BATCH_SIZE, TOP_K_PROBE_VALUES, TopKProbeOutcome, TopKProbeTransport, TopKTable,
};
use moagan::llm::top_p_probe::{
    LlmClientTopPProbeTransport, TOP_P_PROBE_BATCH_SIZE, TOP_P_PROBE_VALUES, TopPProbeTransport,
    TopPTable,
};
use moagan::secret::SecretString;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a `MinimaxProvider` pointed at the mock server URI.
/// `with_max_retries(1)` keeps each rejected probe to a single
/// HTTP round-trip so the integration tests finish in seconds
/// rather than minutes.
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
        temperature_auto_enabled: None,
    };
    Arc::new(
        MinimaxProvider::new(&cfg, SecretString::new("sk-test".to_owned()))
            .expect("MinimaxProvider::new should accept the test config")
            .with_max_retries(1),
    )
}

/// Wrap a provider in an `LlmClientTopPProbeTransport` typed as
/// `Arc<dyn TopPProbeTransport>` so the algorithm does not care
/// that the underlying transport speaks `LlmClient`. The
/// `ProviderLlmClient` adapter bridges the SDK shape back to the
/// legacy `Provider` so the test exercises the same
/// `MinimaxProvider` transport across both wiring styles.
fn wrap_top_p_transport(provider: Arc<MinimaxProvider>) -> Arc<dyn TopPProbeTransport> {
    let client: Arc<dyn LlmClient> = Arc::new(ProviderLlmClient::new(
        provider as Arc<dyn moagan::llm::provider::Provider>,
    ));
    Arc::new(
        LlmClientTopPProbeTransport::new(client)
            .expect("LlmClientTopPProbeTransport::new should accept the client"),
    )
}

/// Mount a wiremock that inspects the wire body and accepts
/// `top_p <= ceiling` while rejecting anything strictly above
/// with a body carrying the documented top_p rejection
/// signature. The 200 body follows the canonical Anthropic-compat
/// envelope shape; the 400 body is the
/// `{"error": {"message": "top_p must be between 0 and 1"}}`
/// shape real upstreams emit.
async fn mount_top_p_ceiling(server: &MockServer, ceiling: f32) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let top_p = body.get("top_p").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            if top_p <= ceiling {
                ResponseTemplate::new(200).set_body_json(json!({
                    "content": [{"type": "text", "text": "1"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    }
                }))
            } else {
                ResponseTemplate::new(400).set_body_json(json!({
                    "error": {"message": "top_p must be between 0 and 1"}
                }))
            }
        })
        .mount(server)
        .await;
}

/// Mount a wiremock that accepts every request regardless of
/// `top_p`. The 200 body is the canonical envelope.
async fn mount_top_p_accept_all(server: &MockServer) {
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

/// Mount a wiremock that rejects every request with a body that
/// carries the top_p rejection signature.
async fn mount_top_p_reject_all(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "top_p out of range"}
        })))
        .mount(server)
        .await;
}

/// Mount a wiremock that inspects the wire body and accepts
/// `top_k <= ceiling` while rejecting anything strictly above
/// Wire boundary at `top_p = 0.95`. The wiremock accepts
/// `top_p <= 0.95` and rejects `> 0.95` with the canonical
/// `"top_p must be between 0 and 1"` signature. The discovered
/// set must therefore be exactly
/// `[0.05, 0.10, ..., 0.95]` (19 entries — the `1.00` candidate
/// gets rejected because `1.00 > 0.95`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_set_above_0_95_rejected_for_top_p() {
    let server = MockServer::start().await;
    mount_top_p_ceiling(&server, 0.95).await;
    let transport = wrap_top_p_transport(build_minimax_provider(server.uri()));

    let table = TopPTable::empty();
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M3", transport, TOP_P_PROBE_BATCH_SIZE)
        .await
        .expect("probe converges");

    // The first 19 entries of TOP_P_PROBE_VALUES are
    // `[0.05, 0.10, ..., 0.95]`; the last entry is `1.00`,
    // which the wiremock rejects.
    let expected: &[f32] = &TOP_P_PROBE_VALUES[..=18];
    assert_eq!(
        discovered.len(),
        expected.len(),
        "expected 19 accepted top_p values (0.05..=0.95)"
    );
    assert_eq!(
        discovered, expected,
        "wiremock accepted top_p<=0.95; discovered set must be exactly [0.05..=0.95]"
    );
    // The discovered entry records the smallest accepted
    // value (`0.05`).
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("probe_and_store must record the discovered entry");
    assert_eq!(entry.top_p, 0.05);
    assert!(entry.auto);
}

/// Wiremock accepts every `top_p` from `0.05` through `1.00`.
/// The discovered set must therefore be the full canonical
/// [`TOP_P_PROBE_VALUES`].
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_full_set_for_top_p() {
    let server = MockServer::start().await;
    mount_top_p_accept_all(&server).await;
    let transport = wrap_top_p_transport(build_minimax_provider(server.uri()));

    let table = TopPTable::empty();
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M3", transport, TOP_P_PROBE_BATCH_SIZE)
        .await
        .expect("probe converges");

    assert_eq!(
        discovered.len(),
        TOP_P_PROBE_VALUES.len(),
        "accept-all wiremock must surface all 20 candidates"
    );
    assert_eq!(
        discovered, TOP_P_PROBE_VALUES,
        "discovered set must match the canonical TOP_P_PROBE_VALUES exactly"
    );
}

/// Wiremock rejects every probe with the top_p rejection
/// signature. The algorithm must return `Vec::new()` (NOT an
/// error) so the runtime gate at dispatch treats the
/// `(provider, model)` as "no probe data, fall through to the
/// operator's requested top_p".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_returns_empty_when_rejects_everything_for_top_p() {
    let server = MockServer::start().await;
    mount_top_p_reject_all(&server).await;
    let transport = wrap_top_p_transport(build_minimax_provider(server.uri()));

    let table = TopPTable::empty();
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M3", transport, TOP_P_PROBE_BATCH_SIZE)
        .await
        .expect("uniform rejection must not surface as an error");

    assert!(
        discovered.is_empty(),
        "reject-all wiremock must surface an empty discovered set"
    );
}

/// Mock transport that accepts a hard-coded subset of `top_k`
/// values. Mirrors the `SubsetTransport` in
/// `src/llm/top_k_probe.rs::tests` but lives in the integration
/// test file so it can drive a full `TopKTable::probe_and_store`
/// end-to-end without going through the wiremock + SDK impl chain
/// (the Anthropic-compat SDK drops `top_k` before serialisation,
/// so a wiremock cannot inspect the candidate — see the
/// `mount_top_k_ceiling` doc-comment above).
#[derive(Clone)]
struct MockTopKTransport {
    accept: std::sync::Arc<std::collections::BTreeSet<u32>>,
}

impl MockTopKTransport {
    fn accepting(values: &[u32]) -> Self {
        let mut set = std::collections::BTreeSet::new();
        for v in values {
            set.insert(*v);
        }
        Self {
            accept: std::sync::Arc::new(set),
        }
    }
}

#[async_trait::async_trait]
impl TopKProbeTransport for MockTopKTransport {
    async fn probe_send_top_k(&self, k: u32) -> TopKProbeOutcome {
        if self.accept.contains(&k) {
            TopKProbeOutcome::Accepted
        } else {
            TopKProbeOutcome::Rejected
        }
    }
}

fn mock_top_k_transport(t: MockTopKTransport) -> std::sync::Arc<dyn TopKProbeTransport> {
    std::sync::Arc::new(t)
}

/// Mock-based boundary check at `top_k = 40`. The transport
/// accepts `top_k <= 40` and rejects `> 40` with the canonical
/// `Rejected` outcome. The discovered set must therefore be
/// exactly `[1, 2, 4, 8, 16, 32]` (6 entries — `64, 128, 256,
/// 512, 1024` all get rejected).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_set_at_40_rejected_for_top_k() {
    let transport = mock_top_k_transport(MockTopKTransport::accepting(&[1, 2, 4, 8, 16, 32]));

    let table = TopKTable::empty();
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M3", transport, TOP_K_PROBE_BATCH_SIZE)
        .await
        .expect("probe converges");

    let expected: &[u32] = &TOP_K_PROBE_VALUES[..6];
    assert_eq!(
        discovered.len(),
        expected.len(),
        "expected 6 accepted top_k values (1..=32)"
    );
    assert_eq!(
        discovered, expected,
        "mock accepted top_k<=32; discovered set must be exactly [1, 2, 4, 8, 16, 32]"
    );
    // The discovered entry records the smallest accepted
    // value (`1`).
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("probe_and_store must record the discovered entry");
    assert_eq!(entry.top_k, 1);
    assert!(entry.auto);
}

/// Mock transport accepts every `top_k` from `1` through `1024`.
/// The discovered set must therefore be the full canonical
/// [`TOP_K_PROBE_VALUES`].
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_full_set_for_top_k() {
    let mut accept_all = std::collections::BTreeSet::new();
    for v in TOP_K_PROBE_VALUES {
        accept_all.insert(*v);
    }
    let transport = mock_top_k_transport(MockTopKTransport {
        accept: std::sync::Arc::new(accept_all),
    });

    let table = TopKTable::empty();
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M3", transport, TOP_K_PROBE_BATCH_SIZE)
        .await
        .expect("probe converges");

    assert_eq!(
        discovered.len(),
        TOP_K_PROBE_VALUES.len(),
        "accept-all mock must surface all 11 candidates"
    );
    assert_eq!(
        discovered, TOP_K_PROBE_VALUES,
        "discovered set must match the canonical TOP_K_PROBE_VALUES exactly"
    );
}
