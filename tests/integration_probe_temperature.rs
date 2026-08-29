//! Wiremock integration tests for the temperature auto-probe.
//!
//! These tests stand up a real HTTP `MockServer`, build an
//! `Arc<MinimaxProvider>` against it, and run the temperature
//! probe end-to-end through `detect_supported_temperatures`. The
//! goal is to catch regressions where the algorithm drifts away
//! from the contract documented in
//! `src/llm/temperature_probe.rs`: 21 candidates
//! (`[0.0, 0.1, ..., 2.0]`) probed in batches, classified as
//! `Accepted` (HTTP 2xx/3xx + non-empty body, no rejection
//! signature), `Rejected` (4xx with the rejection signature, or
//! any 2xx/3xx that silently drops the parameter), or
//! `Indeterminate` (everything else).
//!
//! Mirrors `tests/integration_max_tokens_auto.rs` (the max-tokens
//! probe has the same shape: wiremock server, real provider,
//! end-to-end algorithm). The temperature probe differs only in
//! the request body field under inspection (`temperature`, not
//! `max_tokens`) and in the rejection signature ("temperature
//! must be between 0 and 2", not "max_tokens > cap").

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
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a `MinimaxProvider` pointed at the mock server URI.
/// `with_max_retries(1)` keeps each rejected probe to a single
/// HTTP round-trip so the integration tests finish in seconds
/// rather than minutes — the probe deliberately does not retry
/// because a rejection IS the signal, not a transient blip.
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

/// Wrap a provider in a `ProviderTemperatureProbeTransport`
/// typed as `Arc<dyn TemperatureProbeTransport>` so the algorithm
/// does not care that the underlying provider is a
/// `MinimaxProvider`.
fn wrap_transport(provider: Arc<MinimaxProvider>) -> Arc<dyn TemperatureProbeTransport> {
    Arc::new(
        ProviderTemperatureProbeTransport::new(provider)
            .expect("ProviderTemperatureProbeTransport::new should accept the provider"),
    )
}

/// Mount a wiremock that inspects the wire body and accepts
/// `temperature <= ceiling` while rejecting anything strictly
/// above with a body carrying the documented temperature
/// rejection signature. The 200 body follows the canonical
/// Anthropic-compat envelope shape; the 400 body is the
/// `{"error": {"message": "temperature must be between 0 and 2"}}`
/// shape real upstreams emit (Anthropic-compat relays that cap
/// at `1.0`, OpenCode routes that cap at `1.0`, etc.).
async fn mount_temperature_ceiling(server: &MockServer, ceiling: f32) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let temperature = body
                .get("temperature")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32;
            if temperature <= ceiling {
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
                    "error": {"message": "temperature must be between 0 and 2"}
                }))
            }
        })
        .mount(server)
        .await;
}

/// Mount a wiremock that accepts every request regardless of
/// `temperature`. The 200 body is the same canonical envelope
/// the `mount_temperature_ceiling` helper emits; every probe
/// lands as `Accepted` because the body is non-empty and carries
/// no rejection signature.
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

/// Mount a wiremock that rejects every request with a body that
/// carries the temperature rejection signature. Every probe
/// lands as `Rejected` (4xx + signature), so the discovered set
/// is empty without any error bubbling up — the algorithm treats
/// a uniform rejection as "the upstream does not accept any
/// temperature we tried" and returns `Vec::new()`.
async fn mount_reject_all(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "temperature out of range"}
        })))
        .mount(server)
        .await;
}

/// Mount a wiremock that simulates the upstream shape the MiniMax
/// relay emits when the model thinks too hard and exhausts its
/// output budget mid-emit: HTTP 200 with `content: null`,
/// `stop_reason: "max_tokens"`, and `usage.output_tokens > 0`.
/// Before PR #594 + iter 2 made the wire decoder tolerate
/// `content: null`, this shape would have surfaced as a
/// transport error; after the decoder change, the [`Response`]
/// carries an empty `text` field and the temperature-probe
/// classifier is responsible for distinguishing "upstream
/// accepted but the model had no budget to emit" from "upstream
/// silently dropped the parameter". The [`TemperatureTable`]
/// must therefore discover the full set — every probe lands
/// as `Accepted` because the truncation signature
/// (`stop_reason = "max_tokens"` AND `output_tokens > 0`) is
/// exactly the case the classifier reads as acceptance.
async fn mount_truncate_all(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": null,
            "stop_reason": "max_tokens",
            "usage": {
                "input_tokens": 49,
                "output_tokens": 16,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            }
        })))
        .mount(server)
        .await;
}

/// Wire boundary at `1.0`. The wiremock accepts
/// `temperature <= 1.0` and rejects `> 1.0` with the canonical
/// `"temperature must be between 0 and 2"` signature. The
/// discovered set must therefore be exactly
/// `[0.0, 0.1, ..., 1.0]` (11 values, step 0.1). This is the
/// production-shaped test: it pins the boundary contract that a
/// relay capping the temperature at `1.0` (Anthropic-compat
/// endpoints, OpenCode routes for `gpt-5.6-luna`) surfaces,
/// so the runtime never tries `T=1.1` against such a relay after
/// the auto-probe runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_set_above_1_0_rejected() {
    let server = MockServer::start().await;
    mount_temperature_ceiling(&server, 1.0).await;
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

    // Compare against the canonical probe slice directly —
    // `TEMPERATURE_PROBE_VALUES[0..=10]` is exactly
    // `[0.0, 0.1, ..., 1.0]` (11 entries) and the algorithm emits
    // the same f32 representations the constant carries
    // (0.9 stays `0.90000004` in f32; both sides match).
    let expected: &[f32] = &TEMPERATURE_PROBE_VALUES[..=10];
    assert_eq!(
        discovered.len(),
        expected.len(),
        "expected exactly 11 accepted temperatures (0.0..=1.0)"
    );
    assert_eq!(
        discovered, expected,
        "wiremock accepted T<=1.0; discovered set must be exactly [0.0..=1.0]"
    );
    // Persistence disabled via `TemperatureTable::empty()`, so
    // the in-memory entry is the source of truth; no on-disk
    // sidecar is written. Verify the entry round-trips through
    // the public `get` accessor so the gate in
    // `RunContext::dispatch_to_provider` can consult it on the
    // first LLM call after the probe finishes.
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("probe_and_store must record the discovered entry");
    assert_eq!(entry.temperatures.as_slice(), expected);
}

/// Wiremock accepts every `temperature` from `0.0` through `2.0`.
/// The discovered set must therefore be exactly the 21 canonical
/// [`TEMPERATURE_PROBE_VALUES`]. This pins the algorithm's
/// upper-bound behaviour: a permissive relay that accepts the
/// full OpenAI-compat band is correctly identified as such.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_full_set_when_accepts_everything() {
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

    assert_eq!(
        discovered.len(),
        TEMPERATURE_PROBE_VALUES.len(),
        "accept-all wiremock must surface all 21 candidates"
    );
    assert_eq!(
        discovered, TEMPERATURE_PROBE_VALUES,
        "discovered set must match the canonical TEMPERATURE_PROBE_VALUES exactly"
    );
}

/// Wiremock rejects every probe with the temperature rejection
/// signature. The algorithm must return `Vec::new()` (NOT an
/// error) so the runtime gate at dispatch treats the
/// `(provider, model)` as "no probe data, fall through to the
/// operator's requested temperature" instead of blowing up the
/// run. This pins the "operator did not get any signal" branch
/// documented in the plan: a probe that uniformly rejects is
/// indistinguishable from a probe that did not run, so the
/// caller falls back to the legacy "send whatever was
/// requested" behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_returns_empty_when_rejects_everything() {
    let server = MockServer::start().await;
    mount_reject_all(&server).await;
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
        .expect("uniform rejection must not surface as an error");

    assert!(
        discovered.is_empty(),
        "reject-all wiremock must surface an empty discovered set"
    );
    // The entry is still recorded (with an empty `temperatures`)
    // so subsequent calls know the probe ran and decided nothing
    // is supported. `supported_for` returns `Vec::new()` for that
    // entry, which is the gate's "no clamp" signal.
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("probe_and_store records the entry even on empty result");
    assert!(entry.temperatures.is_empty());
}

/// Regression pin for the operator-reported bug: when the
/// upstream returns HTTP 200 with `content: null` and
/// `stop_reason: "max_tokens"` for every probe (the
/// MiniMax-on-think-too-hard shape), the temperature probe
/// must discover the full set, not collapse it to `Vec::new()`.
///
/// Before the PR #594 follow-up, the `classify_probe_response`
/// helper treated every 2xx-with-empty-body as `Rejected` —
/// the assumption being that an empty body could only come
/// from a silent parameter drop. After PR #594 made the wire
/// decoder tolerate `content: null`, that assumption broke:
/// the MiniMax upstream returns this shape legitimately when
/// the model exhausts its output budget mid-emit, and the
/// classifier has to recognise the truncation signal
/// (`finish_reason = "max_tokens"` AND `output_tokens > 0`) as
/// acceptance. The previous classifier collapsed every probe
/// to `Rejected`, so `detect_supported_temperatures` returned
/// an empty vec and the operator saw
/// `temperatures = []` in `temperatures_auto.toml`.
///
/// This test is the end-to-end pin: a wiremock that emits the
/// exact body shape the operator's run produced must surface
/// all 21 candidates as `Accepted` through the real
/// `MinimaxProvider` / `ProviderTemperatureProbeTransport` /
/// `TemperatureTable::probe_and_store` chain. If this test
/// ever regresses, the bug returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_finds_full_set_when_upstream_truncates() {
    let server = MockServer::start().await;
    mount_truncate_all(&server).await;
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
        .expect("truncation-only upstream must not surface as an error");

    // Every probe hits the truncated branch (the wiremock is
    // uniform) and the classifier must commit each to
    // `Accepted`. The discovered set is therefore the full
    // canonical slice in canonical order.
    assert_eq!(
        discovered.len(),
        TEMPERATURE_PROBE_VALUES.len(),
        "truncated wiremock must surface all 21 candidates as Accepted"
    );
    assert_eq!(
        discovered, TEMPERATURE_PROBE_VALUES,
        "discovered set must match the canonical TEMPERATURE_PROBE_VALUES exactly"
    );
    // The entry recorded by `probe_and_store` reflects the
    // non-empty discovery, so the runtime's dispatch gate can
    // consult it on the first LLM call after the probe finishes.
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("probe_and_store records the discovered entry");
    assert_eq!(entry.temperatures.as_slice(), TEMPERATURE_PROBE_VALUES);
    assert!(entry.auto, "auto-detected entries must carry auto=true");
}

/// Pin for the silent-drop / decoder-absorbed-error branch:
/// the wiremock emits HTTP 200 with `content: null` and
/// `stop_reason: "end_turn"` (or no finish_reason at all) for
/// every probe. The classifier must NOT classify this as
/// `Accepted` (no truncation signal) and must NOT classify it
/// as `Rejected` (the `body_carries_temperature_rejection`
/// heuristic does not fire on an empty body). The shape
/// therefore lands as `Indeterminate`, the algorithm's
/// `retry_once_on_indeterminate` fires a second probe, and the
/// second probe produces the same shape — so the batch
/// boundary treats every candidate as terminal `Indeterminate`
/// and the discovered set is empty.
///
/// This pins the "be honest about ambiguous upstream shapes"
/// contract: a probe that cannot commit is NOT a probe that
/// rejects. The runtime's dispatch gate falls back to the
/// operator's requested temperature instead of locking the
/// discovered set to a wrong value, which is the safer of the
/// two failure modes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_returns_empty_when_upstream_returns_empty_body_no_truncation() {
    let server = MockServer::start().await;
    // Wiremock that returns 200 + content:null + stop_reason:
    // "end_turn" — the upstream succeeded but emitted nothing.
    // This is the ambiguous shape that the classifier commits
    // to `Indeterminate`, so the algorithm's retry path runs
    // and the final outcome stays non-`Accepted`.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": null,
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 49,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            }
        })))
        .mount(&server)
        .await;

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
        .expect("uniform Indeterminate must not surface as an error");

    // Every probe is `Indeterminate` → the algorithm retries
    // once → the retry hits the same shape → the candidate is
    // terminal `Indeterminate` at the batch boundary →
    // `detect_supported_temperatures` treats it as not-
    // accepted → the discovered set is empty.
    assert!(
        discovered.is_empty(),
        "non-truncated empty body must surface as empty discovered set; got {discovered:?}"
    );
    // The on-table entry still records the run (so the next
    // startup knows the probe ran and committed nothing) but
    // with an empty `temperatures` vector.
    let entry = table
        .get("minimax", "MiniMax-M3")
        .expect("probe_and_store records the entry even on empty result");
    assert!(entry.temperatures.is_empty());
}
