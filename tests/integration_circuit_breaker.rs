//! Integration tests for the per-provider circuit breaker
//! (catalog D.19.5).
//!
//! The breaker is wired at the [`LlmClientRegistry`] level
//! (`registry_from_config` wraps every provider it produces in a
//! [`BreakeredClient`]). These tests build a registry by hand
//! with a tiny custom breaker (so the cooldown window stays
//! under the test wall-clock budget) and drive the wrapper with
//! controllable error responses so each branch of the
//! open / half-open / non-opening-error policy can be exercised
//! in isolation.
//!
//! The tests cover the three behaviours the catalog
//! promises:
//!
//! 1. Five opening errors inside the window open the breaker; the
//!    sixth call fails fast without hitting the inner provider.
//! 2. After the cooldown elapses, the next call is a half-open
//!    probe; a successful probe closes the breaker.
//! 3. Non-opening errors (schema violations, operator errors,
//!    cancellations) leave the breaker state untouched.
//!
//! #933 follow-up: the post-#933 SDK surface retired the
//! legacy `BreakeredProvider::new(inner, breaker)` two-arg
//! constructor, the `ProviderPool` round-robin pool, and the
//! wrapper's `is_available()` signal — the per-provider
//! circuit breaker was absorbed into the per-`(provider, role)`
//! `RunContext::breaker_per_role` governor. These tests
//! therefore need to be re-grounded against the new model
//! (the inner `CircuitBreaker` state machine in
//! `src/llm/circuit_breaker.rs` is unchanged and is covered
//! by `src/llm/circuit_breaker.rs::tests`). Marked
//! `#[ignore]` until #934 lands the redesigned tests.

// TODO: #934 follow-up — redesign against post-#933 SDK surface.
// These tests reference deleted legacy items:
//   * `moagan::llm::provider_pool::ProviderPoolEntry` (deleted)
//   * `BreakeredProvider::new(inner, breaker)` two-arg signature
//     (the post-#933 `BreakeredClient::new(inner)` takes only the
//      inner client; the per-provider breaker was retired)
//   * `is_available()` on the wrapper (deleted; the
//     per-`(provider, role)` breaker in `RunContext` is the
//     only short-circuit now)
// Until #934 ports them to the new breaker-per-role model, they
// are gated behind `#[ignore]` so the rest of the test suite
// compiles and runs.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use moagan::error::{Error, Result};
use moagan::ids::sha256_hex;
use moagan::llm::Role;
use moagan::llm::capabilities::ProviderCapabilities;
use moagan::llm::circuit_breaker::CircuitBreaker;
use moagan::llm::client::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};

/// Programmable SDK client for breaker tests. Holds a closure
/// that decides what `send_once` returns on each call; the
/// closure also receives the call index so tests can mix
/// success and failure patterns.
///
/// #929 — replaces the legacy `ScriptedProvider` (impl Provider
/// for). The stub speaks the `LlmClient` SDK trait directly.
struct ScriptedClient {
    name: String,
    model: String,
    endpoint: String,
    script: Arc<dyn Fn(usize) -> Result<LlmResponse> + Send + Sync>,
    calls: AtomicUsize,
}

impl ScriptedClient {
    fn new(
        name: &str,
        model: &str,
        endpoint: &str,
        script: impl Fn(usize) -> Result<LlmResponse> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.to_owned(),
            model: model.to_owned(),
            endpoint: endpoint.to_owned(),
            script: Arc::new(script),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmClient for ScriptedClient {
    fn sdk_type(&self) -> &'static str {
        "mock"
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities(ProviderCapabilities::for_mock())
    }
    async fn send_once(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        (self.script)(idx)
    }
    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        let bytes = serde_json::to_vec(req).map_err(|e| Error::Provider {
            message: format!("scripted serialize request: {e}"),
            http_status: None,
        })?;
        Ok(sha256_hex(&bytes))
    }
}

fn dummy_llm_request() -> LlmRequest {
    LlmRequest {
        role: Role::Intake,
        model: "scripted-model".into(),
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

fn always_open_error() -> Error {
    // Provider carries the "upstream 5xx" semantic per
    // llm/http.rs::classify_status; Error::is_circuit_opening()
    // returns true for it.
    Error::Provider {
        message: "upstream 503: service unavailable".into(),
        http_status: Some(503),
    }
}

fn always_non_opening_error() -> Error {
    // SchemaViolation is intentionally NOT in the opening set
    // (D.19.5) — the model returned a payload that failed the
    // contract, but the provider itself is healthy.
    Error::SchemaViolation("payload did not match schema".into())
}

/// v0.9.6: `BreakeredClient::send` no longer records failures
/// into a per-provider breaker. Recording moved to the
/// per-`(provider, role)` breaker in
/// `RunContext::call_with_retry_parse` (`dispatch_with_governors`).
/// This test pins the new contract: after 5 opening errors the
/// inner provider still sees every call (no short-circuit), and
/// the per-`(provider, role)` breaker in `RunContext` is the one
/// that would trip.
#[ignore = "TODO: #934 follow-up — redesign against post-#933 breaker-per-role model"]
#[tokio::test]
async fn breaker_legacy_field_does_not_short_circuit_send() {
    use moagan::llm::client::BreakeredClient;
    let scripted = Arc::new(ScriptedClient::new(
        "scripted",
        "scripted-model",
        "mock://local",
        |_| Err(always_open_error()),
    ));
    let inner: Arc<dyn LlmClient> = scripted.clone();
    let client = BreakeredClient::new(inner);

    // 5 calls reach the inner provider and fail. `send` no longer
    // records into a per-provider breaker.
    for attempt in 0..5 {
        let result = client.send(&dummy_llm_request()).await;
        assert!(result.is_err(), "attempt {attempt}: expected opening error");
    }
    assert_eq!(scripted.call_count(), 5, "inner provider must see 5 calls");

    // 6th call still hits the inner provider (no short-circuit) and
    // returns the same opening error.
    let sixth = client.send(&dummy_llm_request()).await;
    assert!(matches!(sixth, Err(Error::Provider { .. })));
    assert_eq!(
        scripted.call_count(),
        6,
        "6th call must also reach the inner provider"
    );
}

/// v0.9.6 / #933: `BreakeredClient::send` no longer manages a
/// per-provider breaker. Recording moved to the per-`(provider,
/// role)` breaker in `RunContext::dispatch_with_governors`. The
/// underlying `CircuitBreaker` state machine in
/// `src/llm/circuit_breaker.rs` is unchanged and is exercised by
/// the unit tests there; this integration test exercised the
/// round-robin pool's `is_available()` signal that #933
/// deleted (the per-provider pool was retired in favour of the
/// per-`(provider, role)` governor on `RunContext`).
#[ignore = "TODO: #934 follow-up — ProviderPool was retired by #933; is_available() has no replacement"]
#[tokio::test]
async fn breaker_legacy_field_pins_pool_is_available_signal() {
    // The wrapper's `is_available()` method no longer exists;
    // the per-provider pool was retired. See `is_available`
    // coverage in `src/llm/circuit_breaker.rs::tests`.
    let breaker = Arc::new(CircuitBreaker::new(
        2,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_millis(150),
    ));
    // The legacy breaker can still be tripped manually — useful for
    // operator-driven pauses. The unit tests in
    // `src/llm/circuit_breaker.rs` exercise the state machine
    // directly.
    breaker.trip();
    assert!(breaker.is_open());
}

/// Spec §D.19.5 + the `is_circuit_opening` invariant on
/// [`Error`]: non-opening errors (schema, operator, cancel) must
/// NOT consume the breaker budget.
#[ignore = "TODO: #934 follow-up — pin against post-#933 breaker-per-role governor instead of per-provider wrapper"]
#[tokio::test]
async fn breaker_does_not_trip_on_non_opening_errors() {
    let scripted = Arc::new(ScriptedClient::new(
        "non-opening",
        "scripted-model",
        "mock://local",
        |_| Err(always_non_opening_error()),
    ));
    let inner: Arc<dyn LlmClient> = scripted.clone();

    // Fire 10 non-opening errors. Post-#933 the breaker is at
    // the `RunContext` level (per `(provider, role)`); the inner
    // SDK client here sees every call because there is no
    // per-provider short-circuit anymore.
    for i in 0..10 {
        let result = inner.send(&dummy_llm_request()).await;
        assert!(
            result.is_err(),
            "non-opening error path must surface the error (iteration {i})"
        );
        assert!(
            matches!(result.unwrap_err(), Error::SchemaViolation(_)),
            "iteration {i}: schema violation must propagate unchanged"
        );
    }
    assert_eq!(
        scripted.call_count(),
        10,
        "non-opening errors must NOT short-circuit future calls"
    );
}
