//! Wiremock integration tests for Phase 0 + Phase 0.5 cap
//! short-circuit. Exercises the algorithm against a real
//! `MockServer` to confirm:
//!
//! - Phase 0 / 0.5 converges in 1 + 2 HTTP round-trips when the
//!   upstream embeds the cap in its error body
//!   (`"model[X] does not support max tokens > N"` for
//!   Anthropic-compat; `"supports at most N completion tokens"` for
//!   OpenAI-compat).
//! - Phase 0 falls back to Phase 1 + Phase 2 cleanly when the body
//!   is unparseable (generic 4xx, network blip, 200 OK).
//! - The discovered cap is persisted into `Entry::ceiling` so a
//!   second run with a pre-populated TOML skips Phase 0's
//!   `max_tokens = u32::MAX` probe entirely.

use std::sync::Arc;

use moagan::config::ProviderConfig;
use moagan::fs_layout::MoaganHome;
use moagan::llm::minimax::MinimaxProvider;
use moagan::llm::probe::{
    MAX_AUTOPROBE_CEILING, MIN_AUTOPROBE_FLOOR, ProbeTransport, ProviderProbeTransport,
};
use moagan::llm::probe_table::MaxTokensTable;
use moagan::secret::SecretString;
use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a `MinimaxProvider` pointed at the mock server URI.
fn build_provider(server_uri: String) -> Arc<MinimaxProvider> {
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

/// Wrap a provider in a `ProviderProbeTransport` typed as
/// `Arc<dyn ProbeTransport>` so the algorithm does not care that
/// the underlying provider is a `MinimaxProvider`.
fn wrap_transport(provider: Arc<MinimaxProvider>) -> Arc<dyn ProbeTransport> {
    Arc::new(
        ProviderProbeTransport::new(provider)
            .expect("ProviderProbeTransport::new should accept the provider"),
    )
}

/// Mount a wiremock that inspects the wire body and returns:
/// - HTTP 400 with the canonical Anthropic-compat cap body for
///   `max_tokens > u32::MAX / 2` (the Phase 0 first request)
/// - HTTP 400 with the canonical max-tokens rejection body for
///   anything strictly above `accept_max` (Phase 0.5 B and Phase 1
///   probes).
/// - HTTP 200 otherwise.
///
/// `cap` is the boundary value reported in the Phase 0 error body.
/// `accept_max` is the actual wire cap the upstream uses. They can
/// differ to model the "upstream lies" case (`accept_max < cap`) or
/// the "B OK, A OK" case (`accept_max > cap`).
async fn mount_anthropic_compat_with_cap(server: &MockServer, cap: u32, accept_max: u32) {
    let cap_body = format!(
        r#"{{"type":"error","error":{{"message":"model[MiniMax-M2.5] does not support max tokens > {cap} (2013)"}}}}"#
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            // The first probe (Phase 0) sets max_tokens to u32::MAX
            // and gets the parseable cap body regardless of
            // `accept_max`. Subsequent probes follow `accept_max`.
            if max_tokens > 1_000_000_000 {
                ResponseTemplate::new(400).set_body_string(&cap_body)
            } else if max_tokens <= accept_max {
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

/// Same shape as `mount_anthropic_compat_with_cap` but with the
/// OpenAI-compat / Responses-API error body shape.
async fn mount_openai_compat_with_cap(server: &MockServer, cap: u32, accept_max: u32) {
    let cap_body = format!(
        r#"{{"error":{{"param":"max_tokens is too large: 200000. This model supports at most {cap} completion tokens, whereas you provided 200000.","type":"server_error","message":"..."}}}}"#
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if max_tokens > 1_000_000_000 {
                ResponseTemplate::new(400).set_body_string(&cap_body)
            } else if max_tokens <= accept_max {
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
                ResponseTemplate::new(400).set_body_string(
                    r#"{"error":{"param":"max_tokens is too large: 999999. This model supports at most 131072 completion tokens, whereas you provided 999999."}}"#,
                )
            }
        })
        .mount(server)
        .await;
}

/// Mount a wiremock that returns a generic 400 (no parseable cap)
/// for the first probe and then accepts up to `accept_max`. Models
/// an upstream that rejects without embedding the boundary in the
/// body — Phase 0 falls back to Phase 1.
async fn mount_generic_4xx_fallback(server: &MockServer, accept_max: u32) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            if max_tokens > 1_000_000_000 {
                // Generic 400 — body does NOT carry the cap
                // signature. Phase 0 must fall through to Phase 1.
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"message":"internal error"}}"#)
            } else if max_tokens <= accept_max {
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

/// Anthropic-compat Phase 0 short-circuit: one initial probe + two
/// validation probes = 3 HTTP round-trips total. The discovered
/// value lands exactly on the cap (196_608 for MiniMax-M2.5).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anthropic_compat_cap_in_body_short_circuits() {
    let server = MockServer::start().await;
    mount_anthropic_compat_with_cap(&server, 196_608, 196_608).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());
    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M2.5", transport, MAX_AUTOPROBE_CEILING)
        .await
        .expect("probe_and_store should converge via Phase 0");
    assert_eq!(
        discovered, 196_608,
        "Phase 0 must discover the parsed cap on the first validation probe"
    );
    // The cached entry carries the parsed cap.
    let entry = table
        .get("minimax", "MiniMax-M2.5")
        .expect("entry must be cached");
    assert_eq!(
        entry.ceiling,
        Some(196_608),
        "entry.ceiling must hold the upstream-reported cap"
    );
    // The TOML was persisted with the cap field.
    let path = home.max_tokens_auto_path();
    assert!(path.exists(), "max_tokens_auto.toml must be on disk");
    let body = std::fs::read_to_string(&path).expect("read TOML");
    assert!(
        body.contains("ceiling = 196608"),
        "TOML must contain ceiling = 196608, got:\n{body}"
    );
}

/// OpenAI-compat / Responses-API Phase 0 short-circuit. The wire
/// body carries `"supports at most N completion tokens"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_compat_cap_in_body_short_circuits() {
    let server = MockServer::start().await;
    mount_openai_compat_with_cap(&server, 131_072, 131_072).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());
    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    let discovered = table
        .probe_and_store("opencode", "longcat-2.0", transport, MAX_AUTOPROBE_CEILING)
        .await
        .expect("probe_and_store should converge via Phase 0");
    assert_eq!(discovered, 131_072);
    let entry = table
        .get("opencode", "longcat-2.0")
        .expect("entry must be cached");
    assert_eq!(entry.ceiling, Some(131_072));
}

/// When the upstream returns a generic 400 (no parseable cap), the
/// algorithm must fall back to Phase 1 + Phase 2 and run the full
/// binary search. The discovered value lands at `accept_max`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_cap_in_body_falls_back_to_binary_search() {
    let server = MockServer::start().await;
    mount_generic_4xx_fallback(&server, 8_192).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());
    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M3", transport, MAX_AUTOPROBE_CEILING)
        .await
        .expect("probe_and_store should converge via Phase 1");
    assert!((8_000..=8_192).contains(&discovered), "got {discovered}");
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("entry must be cached");
    assert_eq!(
        entry.ceiling, None,
        "Phase 0 did not parse a cap; entry.ceiling must stay None"
    );
}

/// When the upstream reports a cap in the body but actually
/// accepts MORE (`cap` < `accept_max`), Phase 0.5 sees `A OK, B OK`
/// and returns the parsed cap as a known floor. The discovered
/// value lands at the parsed cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_minus_one_falls_back() {
    let server = MockServer::start().await;
    // Body reports 196_608, but the upstream actually accepts up
    // to 524_288. Phase 0.5 sees A=196_608 OK, B=196_609 OK and
    // returns 196_608 as a known floor.
    mount_anthropic_compat_with_cap(&server, 196_608, 524_288).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());
    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M2.5", transport, MAX_AUTOPROBE_CEILING)
        .await
        .expect("probe_and_store should converge via Phase 0.5");
    assert_eq!(
        discovered, 196_608,
        "Phase 0.5 must return the parsed cap as a floor when both A and B accept"
    );
}

/// When the upstream reports a cap but actually rejects it
/// (`cap` > `accept_max`), Phase 0.5 sees `A Rejected` and falls
/// back to Phase 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_rejected_falls_back() {
    let server = MockServer::start().await;
    // Body reports 196_608, but the upstream only accepts up to
    // 4_096. Phase 0.5 sees A=196_608 rejected and falls back to
    // Phase 1, which discovers the 4_096 boundary.
    mount_anthropic_compat_with_cap(&server, 196_608, 4_096).await;
    let transport = wrap_transport(build_provider(server.uri()));

    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());
    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    let discovered = table
        .probe_and_store("minimax", "MiniMax-M2.5", transport, MAX_AUTOPROBE_CEILING)
        .await
        .expect("probe_and_store should converge via Phase 1 fallback");
    assert!(
        (4_000..=4_096).contains(&discovered),
        "Phase 1 must discover the real boundary after Phase 0.5 falls back; got {discovered}"
    );
    let entry = table
        .get("minimax", "MiniMax-M2.5")
        .expect("entry must be cached");
    assert_eq!(
        entry.ceiling, None,
        "Phase 0.5 rejected the candidate; entry.ceiling must stay None"
    );
}

/// End-to-end: first run discovers the cap via Phase 0 + 0.5 and
/// persists it. The second run reads the cached `ceiling` and
/// re-validates it. The contract: the second run's `max_seen` is
/// the larger of the Phase 0 `u32::MAX` probe and the
/// `min(ceiling, candidate)` validation probes. The cached ceiling
/// itself must not get probed UP — Phase 0.5 still uses the cap
/// as the upper bound, so the algorithm walk stays below or at
/// the cached value.
///
/// The test tracks the largest value seen on the second wiremock.
/// The expected behaviour: at least one Phase 0 probe fires
/// `max_tokens = u32::MAX`, and Phase 0.5 then uses the cached
/// cap as the upper bound (so the largest non-Phase-0 probe is
/// at most `cached_ceiling + 1`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cached_ceiling_skips_phase_0_on_second_run() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let dir = tempdir().expect("tempdir");
    let home = MoaganHome::at(dir.path().to_path_buf());

    // First run: Phase 0 discovers 196_608, persists it.
    let server = MockServer::start().await;
    mount_anthropic_compat_with_cap(&server, 196_608, 196_608).await;
    let transport1 = wrap_transport(build_provider(server.uri()));
    let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should accept an empty home");
    let _ = table
        .probe_and_store("minimax", "MiniMax-M2.5", transport1, MAX_AUTOPROBE_CEILING)
        .await
        .expect("first run should converge via Phase 0");
    let entry = table
        .get("minimax", "MiniMax-M2.5")
        .expect("entry must be cached after first run");
    assert_eq!(entry.ceiling, Some(196_608));
    drop(table);

    // Second run: load the table from the same home. Phase 0
    // fires `max_tokens = u32::MAX` (this is the algorithm's
    // contract — Phase 0 always re-validates the upstream cap).
    // Phase 0.5 then uses the cached ceiling as the upper bound
    // for the parallel A/B pair (A = ceiling, B = ceiling + 1).
    let server2 = MockServer::start().await;
    let max_seen = Arc::new(AtomicU32::new(0));
    let max_non_phase0_seen = Arc::new(AtomicU32::new(0));
    let phase0_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let max_seen_clone = Arc::clone(&max_seen);
    let max_non_phase0_clone = Arc::clone(&max_non_phase0_seen);
    let phase0_clone = Arc::clone(&phase0_seen);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let max_tokens = body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            // Track the largest value seen.
            let prev = max_seen_clone.load(Ordering::SeqCst);
            if max_tokens > prev {
                max_seen_clone.store(max_tokens, Ordering::SeqCst);
            }
            // Phase 0 fires with `max_tokens = u32::MAX`. Track it
            // separately from the algorithm walk.
            if max_tokens == u32::MAX {
                phase0_clone.store(true, Ordering::SeqCst);
                ResponseTemplate::new(400).set_body_string(
                    r#"{"type":"error","error":{"message":"model[X] does not support max tokens > 196608 (2013)"}}"#,
                )
            } else {
                // Track non-Phase-0 separately.
                let prev_n = max_non_phase0_clone.load(Ordering::SeqCst);
                if max_tokens > prev_n {
                    max_non_phase0_clone.store(max_tokens, Ordering::SeqCst);
                }
                if max_tokens <= 196_608 {
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
                    ResponseTemplate::new(400).set_body_string(
                        r#"{"type":"error","error":{"message":"max_tokens > cap"}}"#,
                    )
                }
            }
        })
        .mount(&server2)
        .await;

    let transport2 = wrap_transport(build_provider(server2.uri()));
    let table2 = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true)
        .expect("MaxTokensTable::from_home should load the persisted table");
    // The cached entry exists with ceiling = 196_608.
    let cached = table2
        .get("minimax", "MiniMax-M2.5")
        .expect("second run must see the cached entry");
    assert_eq!(cached.ceiling, Some(196_608));

    // The probe still runs end-to-end against a fresh transport:
    // Phase 0 fires `max_tokens = u32::MAX` once to re-validate
    // the upstream's reported cap, and Phase 0.5 walks
    // `(196608, 196609)` to confirm. The Phase 0.5 walk respects
    // the cached ceiling: nothing above 196_609 (cap + 1) is
    // probed on the algorithm path.
    let _ = table2
        .probe_and_store("minimax", "MiniMax-M2.5", transport2, MAX_AUTOPROBE_CEILING)
        .await
        .expect("second run should re-discover 196_608");
    assert!(
        phase0_seen.load(Ordering::SeqCst),
        "Phase 0 must fire on the second run to re-validate the cached ceiling"
    );
    let max_after = max_non_phase0_seen.load(Ordering::SeqCst);
    assert!(
        max_after <= 196_609,
        "Phase 0.5 must stay at cached_ceiling + 1 = 196609, got {max_after}"
    );
}
