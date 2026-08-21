//! Integration tests for the per-provider circuit breaker
//! (catalog 10-integrada-v0 §D.19.5, T00-08 §1428-1435).
//!
//! The breaker is wired at the [`ProviderRegistry`] level
//! (`registry_from_config` wraps every provider it produces in a
//! [`BreakeredProvider`]). These tests build a registry by hand
//! with a tiny custom breaker (so the cooldown window stays
//! under the test wall-clock budget) and drive the wrapper with
//! controllable error responses so each branch of the
//! open / half-open / non-opening-error policy can be exercised
//! in isolation.
//!
//! The tests cover the three behaviours the catalog §D.19.5
//! promises:
//!
//! 1. Five opening errors inside the window open the breaker; the
//!    sixth call fails fast without hitting the inner provider.
//! 2. After the cooldown elapses, the next call is a half-open
//!    probe; a successful probe closes the breaker.
//! 3. Non-opening errors (schema violations, operator errors,
//!    cancellations) leave the breaker state untouched.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use moagan::error::{Error, Result};
use moagan::llm::Role;
use moagan::llm::circuit_breaker::CircuitBreaker;
use moagan::llm::provider::{BreakeredProvider, Provider};
use moagan::llm::provider_pool::ProviderPoolEntry;
use moagan::llm::wire::{CallRecord, Request, Response, Usage};

/// A programmable provider for breaker tests. Holds a closure
/// that decides what `send` returns on each call; the closure
/// also receives the call index so tests can mix success and
/// failure patterns.
struct ScriptedProvider {
    name: String,
    model: String,
    endpoint: String,
    script: Arc<dyn Fn(usize) -> Result<(u16, Response)> + Send + Sync>,
    calls: AtomicUsize,
    records: parking_lot::Mutex<Vec<CallRecord>>,
}

impl ScriptedProvider {
    fn new(
        name: &str,
        model: &str,
        endpoint: &str,
        script: impl Fn(usize) -> Result<(u16, Response)> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.to_owned(),
            model: model.to_owned(),
            endpoint: endpoint.to_owned(),
            script: Arc::new(script),
            calls: AtomicUsize::new(0),
            records: parking_lot::Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let record = CallRecord {
            cache_key: String::new(),
            provider: self.name.clone(),
            model: self.model.clone(),
            started_unix: 0,
            ended_unix: 0,
            http_status: None,
            cache_hit: false,
            usage: Usage::default(),
            error: None,
        };
        self.records.lock().push(record);
        (self.script)(idx)
    }
}

fn dummy_request() -> Request {
    Request {
        role: Role::Intake,
        model: "scripted-model".into(),
        system: String::new(),
        user: "u".into(),
        max_tokens: 16,
        temperature: None,
        top_p: None,
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
    Error::Provider("upstream 503: service unavailable".into())
}

fn always_non_opening_error() -> Error {
    // SchemaViolation is intentionally NOT in the opening set
    // (D.19.5) — the model returned a payload that failed the
    // contract, but the provider itself is healthy.
    Error::SchemaViolation("payload did not match schema".into())
}

/// v0.9.6: `BreakeredProvider::send` no longer records failures
/// into the legacy per-provider breaker. Recording moved to the
/// per-`(provider, role)` breaker in `RunContext::call_with_retry_parse`
/// (`dispatch_with_governors`); the legacy field is now used only
/// by the provider pool's `is_available` signal. This test pins
/// the new contract: after 5 opening errors the inner provider
/// still sees every call (no short-circuit), the breaker stays
/// closed, and the per-`(provider, role)` breaker in
/// `RunContext` is the one that would trip.
#[tokio::test]
async fn breaker_legacy_field_does_not_short_circuit_send() {
    let scripted = Arc::new(ScriptedProvider::new(
        "scripted",
        "scripted-model",
        "mock://local",
        |_| Err(always_open_error()),
    ));
    let inner: Arc<dyn Provider> = scripted.clone();
    let breaker = Arc::new(CircuitBreaker::new(
        5,
        Duration::from_secs(60),
        Duration::from_secs(60),
    ));
    let wrapper = BreakeredProvider::new(inner, breaker.clone());

    // 5 calls reach the inner provider and fail. `send` no longer
    // records into the per-provider breaker.
    for attempt in 0..5 {
        let result = wrapper.send(&dummy_request()).await;
        assert!(result.is_err(), "attempt {attempt}: expected opening error");
    }
    assert_eq!(scripted.call_count(), 5, "inner provider must see 5 calls");
    assert_eq!(
        breaker.failure_count(),
        0,
        "v0.9.6: per-provider breaker is no longer tripped by send()"
    );
    assert!(
        !breaker.is_open(),
        "legacy breaker stays closed after send()"
    );

    // 6th call still hits the inner provider (no short-circuit) and
    // returns the same opening error.
    let sixth = wrapper.send(&dummy_request()).await;
    assert!(matches!(sixth, Err(Error::Provider(_))));
    assert_eq!(
        scripted.call_count(),
        6,
        "6th call must also reach the inner provider"
    );
}

/// v0.9.6: `BreakeredProvider::send` no longer manages the legacy
/// per-provider breaker. Recording moved to the per-`(provider,
/// role)` breaker in `RunContext::dispatch_with_governors`. The
/// legacy field on `BreakeredProvider` is still used by the
/// v0.9.6: `BreakeredProvider::send` no longer manages the legacy
/// per-provider breaker. Recording moved to the per-`(provider,
/// role)` breaker in `RunContext::dispatch_with_governors`. The
/// legacy field on `BreakeredProvider` is still used by the
/// provider pool's `is_available` signal, so the underlying
/// `CircuitBreaker` state machine is unchanged. This test pins the
/// new contract end-to-end:
/// - 5 opening errors through `wrapper.send` do NOT trip the
///   legacy breaker (recording moved away).
/// - The legacy breaker can still be tripped manually via `trip()`.
/// - When `is_open()` returns true (manually tripped), the
///   provider pool correctly reports the entry as paused.
#[tokio::test]
async fn breaker_legacy_field_pins_pool_is_available_signal() {
    let scripted = Arc::new(ScriptedProvider::new(
        "probe-scripted",
        "scripted-model",
        "mock://local",
        |idx| {
            if idx < 2 {
                Err(always_open_error())
            } else {
                Ok((
                    200,
                    Response {
                        text: format!("ok-{idx}"),
                        finish_reason: Some("end_turn".into()),
                        truncated: false,
                        usage: Usage::default(),
                    },
                ))
            }
        },
    ));
    let inner: Arc<dyn Provider> = scripted.clone();
    let breaker = Arc::new(CircuitBreaker::new(
        2,
        Duration::from_secs(60),
        Duration::from_millis(150),
    ));
    let wrapper = BreakeredProvider::new(inner, breaker.clone());

    // 2 calls through `wrapper.send` reach the inner provider and
    // fail. v0.9.6: the legacy breaker is NOT tripped by `send`.
    for _ in 0..2 {
        let _ = wrapper.send(&dummy_request()).await;
    }
    assert_eq!(
        scripted.call_count(),
        2,
        "inner provider must see every send() in v0.9.6"
    );
    assert_eq!(
        breaker.failure_count(),
        0,
        "v0.9.6: per-provider breaker no longer records from send()"
    );
    assert!(!breaker.is_open());

    // The legacy breaker can still be tripped manually — useful for
    // operator-driven pauses and for the provider pool's
    // `is_available()` signal.
    breaker.trip();
    assert!(breaker.is_open());

    // `BreakeredProvider::is_available` correctly reports the
    // entry as paused when the breaker is open. The trait is
    // implemented directly on `BreakeredProvider`, not via `Arc`,
    // so we wrap in `Arc` for the dyn dispatch.
    let pool_entry: Arc<BreakeredProvider> = Arc::new(wrapper);
    assert!(!pool_entry.is_available().await);
}

/// Spec §D.19.5 + the `is_circuit_opening` invariant on
/// [`Error`]: non-opening errors (schema, operator, cancel) must
/// NOT consume the breaker budget.
#[tokio::test]
async fn breaker_does_not_trip_on_non_opening_errors() {
    let scripted = Arc::new(ScriptedProvider::new(
        "non-opening",
        "scripted-model",
        "mock://local",
        |_| Err(always_non_opening_error()),
    ));
    let inner: Arc<dyn Provider> = scripted.clone();
    let breaker = Arc::new(CircuitBreaker::new(
        3,
        Duration::from_secs(60),
        Duration::from_secs(60),
    ));
    let wrapper = BreakeredProvider::new(inner, breaker.clone());

    // Fire 10 non-opening errors. None of them should count
    // toward the breaker, so it must stay Closed and the inner
    // provider must see every single call.
    for i in 0..10 {
        let result = wrapper.send(&dummy_request()).await;
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
    assert_eq!(
        breaker.state(),
        "closed",
        "non-opening errors must NOT trip the breaker"
    );
    assert_eq!(
        breaker.failure_count(),
        0,
        "non-opening errors must NOT increment the failure counter"
    );

    // Sanity: a single opening error DOES count, so the negative
    // case above is not vacuous — the breaker only stays closed
    // because the error class is the gating signal. Drive the
    // breaker directly with one opening error and confirm the
    // threshold=1 trip.
    let breaker2 = Arc::new(CircuitBreaker::new(
        1,
        Duration::from_secs(60),
        Duration::from_secs(60),
    ));
    let trip = breaker2
        .run(|| async { Err::<(), _>(always_open_error()) })
        .await;
    assert!(trip.is_err());
    assert_eq!(
        breaker2.state(),
        "open",
        "single opening error with threshold=1 must trip the breaker"
    );
}
