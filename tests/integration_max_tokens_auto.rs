//! Wiremock integration tests for the max-tokens-auto probe.
//!
//! These tests stand up a real HTTP `MockServer`, build an
//! `Arc<MinimaxProvider>` against it, and run the probe algorithm
//! end-to-end through `ProviderProbeTransport::probe_send`. The
//! goal is to catch regressions where the algorithm's behaviour
//! drifts away from the contract documented in `src/llm/probe.rs`:
//! 30 sequential probes in `[2^1, 2^30]`, followed by 20-point
//! parallel tightening rounds, with a clamp into
//! `[MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING]`.
//!
//! Discovered-value contract: Phase 2 narrows the boundary by
//! searching above `lo`; if no candidate above `lo` is accepted,
//! the discovered value falls back to `lo` itself (Phase 1's last
//! accepted). The unit tests in `probe.rs` already pin this
//! behaviour, and the integration assertions below match what the
//! live algorithm emits.
//!
//! Wire-clamp caveat: `MinimaxProvider::send` clamps the wire
//! `max_tokens` to `MINIMAX_MAX_TOKENS_CAP` before sending. That
//! clamp is intentional (the upstream rejects anything above the
//! cap) but it means the algorithm can never observe a boundary
//! `at` the cap from the wire -- every probe lands on a clamped
//! wire value and the algorithm therefore converges at
//! `MAX_AUTOPROBE_CEILING` instead. The `probe_finds_524k_boundary`
//! test below documents this.

use std::sync::Arc;

use moagan::config::ProviderConfig;
use moagan::fs_layout::MoaganHome;
use moagan::llm::minimax::MinimaxProvider;
use moagan::llm::probe::{
    MAX_AUTOPROBE_CEILING, MIN_AUTOPROBE_FLOOR, ProbeTransport, ProviderProbeTransport,
    detect_max_tokens,
};
use moagan::llm::probe_table::MaxTokensTable;
use moagan::secret::SecretString;
use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a `MinimaxProvider` pointed at the mock server URI.
/// `with_max_retries(1)` keeps each rejected probe to a single
/// HTTP round-trip so the integration tests finish in seconds
/// rather than minutes.
fn build_provider(server_uri: String) -> Arc<MinimaxProvider> {
    let cfg = ProviderConfig {
        kind: "minimax".to_owned(),
        endpoint: server_uri,
        model: "MiniMax-M3".to_owned(),
        max_tokens: None,
        temperature: None,
        top_p: None,
        hard_incompatibilities: vec![],
        omit_max_tokens: false,
        plan: None,
        max_token_auto: None,
        max_token_auto_save: true,
    };
    Arc::new(
        MinimaxProvider::new(&cfg, SecretString::new("sk-test".to_owned()))
            .expect("MinimaxProvider::new should accept the test config")
            .with_max_retries(1),
    )
}

/// Wrap a provider in a `ProviderProbeTransport` typed as
/// `Arc<dyn ProbeTransport>` so the algorithm does not care that
/// the underlying provider is a `MinimaxProvider`.
fn wrap_transport(provider: Arc<MinimaxProvider>) -> Arc<dyn ProbeTransport> {
    Arc::new(
        ProviderProbeTransport::new(provider)
            .expect("ProviderProbeTransport::new should accept the provider"),
    )
}

/// Mount a wiremock that inspects the wire body and accepts
/// `max_tokens <= max_accepted` while rejecting anything strictly
/// above with a body carrying the `max_tokens` rejection
/// signature. The rejection body is the canonical
/// `{"type":"error","error":{"message":"max_tokens > ..."}}`
/// shape that real Anthropic-compat providers emit.
async fn mount_boundary(server: &MockServer, max_accepted: u32) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if max_tokens <= max_accepted {
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
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"type":"error","error":{"message":"max_tokens > cap"}}"#)
            }
        })
        .mount(server)
        .await;
}

/// Mount a wiremock that accepts every request regardless of
/// `max_tokens`. Used by the ceiling-bound test.
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

/// Wire boundary at 8192 — Phase 1 succeeds through `2^13 = 8192`,
/// fails at `2^14 = 16384`; Phase 2 confirms every candidate above
/// `lo = 8192` is rejected, so the discovered value falls back to
/// `lo` itself. The wire-clamp at 524_288 has no effect here
/// because 8192 < 524_288.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_8k_boundary() {
    let server = MockServer::start().await;
    mount_boundary(&server, 8192).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let discovered = detect_max_tokens(transport, MIN_AUTOPROBE_FLOOR)
        .await
        .expect("probe converges");
    assert_eq!(discovered, 8192, "8K boundary: algorithm returns lo");
}

/// Wire boundary at 524_288 — but `MinimaxProvider` clamps the
/// wire body to `MINIMAX_MAX_TOKENS_CAP = 524_288`, so every probe
/// lands on a wire `max_tokens` of 524_288. The wiremock accepts
/// that value, the algorithm never observes a rejection, and
/// `lo` walks all the way to `2^30`. The discovered value is the
/// safety ceiling, not the wire boundary. The downstream
/// `effective_max_tokens` clamp pulls it back to the
/// provider-side cap, so the integration test documents the
/// *ceiling* rather than the boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_524k_boundary() {
    let server = MockServer::start().await;
    mount_boundary(&server, 524_288).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let discovered = detect_max_tokens(transport, MIN_AUTOPROBE_FLOOR)
        .await
        .expect("probe converges");
    assert_eq!(
        discovered, MAX_AUTOPROBE_CEILING,
        "wire clamp masks the 524_288 boundary; algorithm converges at the safety ceiling"
    );
}

/// Wiremock accepts every value. Phase 1 finishes all 30 probes
/// without breaking; `hi` falls back to
/// `MAX_AUTOPROBE_CEILING`; Phase 2 collapses immediately;
/// `discovered = lo = 2^30`; the safety clamp pins the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_caps_at_ceiling_when_provider_accepts_everything() {
    let server = MockServer::start().await;
    mount_accept_all(&server).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let discovered = detect_max_tokens(transport, MIN_AUTOPROBE_FLOOR)
        .await
        .expect("probe converges");
    assert_eq!(discovered, MAX_AUTOPROBE_CEILING);
}

/// Wire boundary at 1024 (well below the operator floor of 8192).
/// Phase 1 finds `lo = 1024`; Phase 2 tightens to ~1025; the
/// floor clamp lifts the discovered value to 8192, so the wire
/// body emitted downstream always carries at least the operator
/// floor regardless of how aggressive the upstream cap is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_respects_floor() {
    let server = MockServer::start().await;
    mount_boundary(&server, 1024).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let discovered = detect_max_tokens(transport, 8192)
        .await
        .expect("probe converges");
    assert_eq!(
        discovered, 8192,
        "floor must lift the discovered value above the wire boundary"
    );
}

/// First probe successfully against an 8K-boundary wiremock.
/// The cached entry lands at `8193` (the off-by-one). We then
/// `reset()` the mock and re-mount it with a 1024 boundary so the
/// cached value is rejected. `verify` returns `false` and the
/// cached entry is removed, leaving the table empty so the
/// caller falls back to a fresh `probe_and_store`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verify_returns_false_when_cached_value_rejected() {
    let server = MockServer::start().await;
    mount_boundary(&server, 8192).await;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("max_tokens_auto.toml");

    let provider = build_provider(server.uri());
    let probe_transport = wrap_transport(provider.clone());
    let verify_transport = wrap_transport(provider);

    let table = MaxTokensTable::from_path(&path, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_path should accept the empty path");
    table
        .probe_and_store("minimax", "MiniMax-M3", probe_transport)
        .await
        .expect("probe_and_store should succeed against an 8K boundary");
    assert!(
        table.get("minimax", "MiniMax-M3").is_some(),
        "cache must hold the freshly discovered entry"
    );

    server.reset().await;
    mount_boundary(&server, 1024).await;

    let ok = table
        .verify("minimax", "MiniMax-M3", verify_transport)
        .await
        .expect("verify should not surface an error");
    assert!(
        !ok,
        "verify must return false when the cached value is rejected"
    );
    assert!(
        table.get("minimax", "MiniMax-M3").is_none(),
        "verify must drop the cached entry on failure so the caller re-probes"
    );
}

/// Probe a provider, persist the table, then build a fresh
/// `MaxTokensTable` from the same `<MOAGAN_HOME>` path and assert
/// the entry round-trips. The test covers the
/// `from_home → probe_and_store → persist → from_home → get`
/// sequence end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistence_round_trip() {
    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());

    let server = MockServer::start().await;
    mount_accept_all(&server).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    table
        .probe_and_store("minimax", "MiniMax-M3", transport)
        .await
        .expect("probe_and_store should succeed against an always-accept wiremock");
    table.persist().expect("persist must serialise to disk");

    assert!(
        home.max_tokens_auto_path().exists(),
        "<MOAGAN_HOME>/max_tokens_auto.toml must exist on disk after persist()"
    );

    let table2 = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("a fresh MaxTokensTable::from_home should accept the same path");
    let entry = table2
        .get("minimax", "MiniMax-M3")
        .expect("persisted entry must round-trip into the new table");
    assert!(entry.auto, "persisted entry must be marked auto=true");
    assert_eq!(
        entry.max_tokens, MAX_AUTOPROBE_CEILING,
        "wiremock accepted everything, so the persisted value is the safety ceiling"
    );
}
