//! Integration tests for the push-side saturation layer (catalog
//! §D.23 + §D.27, v0.8 telemetry push-side).
//!
//! Exercises the full end-to-end path:
//!
//! 1. `BreakeredProvider::send` fires a `SaturationEvent` through
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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use moagan::error::Result;
use moagan::ids::RunId;
use moagan::llm::Role;
use moagan::llm::circuit_breaker::CircuitBreaker;
use moagan::llm::provider::{BreakeredProvider, Provider, SaturationSink};
use moagan::llm::rate_limiter::RateLimiter;
use moagan::llm::wire::{Request, Response};
use moagan::telemetry::Telemetry;
use moagan::telemetry::saturation::{SaturationEvent, SaturationKind};

/// Programmable inner provider that always returns an opening
/// error so the breaker trips on the very first call. Used to
/// drive the circuit-open hook without burning through five
/// failures per test (the threshold=1 breaker does the same
/// thing with one call).
struct AlwaysErrorProvider;

#[async_trait]
impl Provider for AlwaysErrorProvider {
    fn name(&self) -> &str {
        "integration-sat"
    }
    fn model(&self) -> &str {
        "integration-model"
    }
    fn endpoint(&self) -> &str {
        "mock://integration"
    }
    async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
        // Provider-class error that the breaker treats as
        // circuit-opening (T01-06 §15.4 / §D.19.5).
        Err(moagan::error::Error::Provider(
            "upstream 503: service unavailable".into(),
        ))
    }
}

/// In-memory sink that records every fired event for assertion.
#[derive(Default)]
struct VecSink(parking_lot::Mutex<Vec<SaturationEvent>>);

impl VecSink {
    fn events(&self) -> Vec<SaturationEvent> {
        self.0.lock().clone()
    }
}

impl SaturationSink for VecSink {
    fn on_saturation(&self, event: &SaturationEvent) {
        self.0.lock().push(event.clone());
    }
}

fn dummy_request() -> Request {
    Request {
        role: Role::Intake,
        model: "integration-model".into(),
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

#[tokio::test]
async fn circuit_open_fires_saturation_event() {
    let inner: Arc<dyn Provider> = Arc::new(AlwaysErrorProvider);
    // threshold=1 so a single opening error trips the breaker.
    let breaker = Arc::new(CircuitBreaker::new(
        1,
        Duration::from_secs(60),
        Duration::from_secs(60),
    ));
    let sink = Arc::new(VecSink::default());
    let wrapper = BreakeredProvider::new(inner, breaker.clone()).with_saturation_sink(sink.clone());

    // First call: inner provider returns an opening error. The
    // wrapper counts the failure (1/1) and the breaker opens.
    let _ = wrapper.send(&dummy_request()).await;

    // Second call: breaker is open, so the wrapper short-circuits
    // and fires a SaturationKind::Error event through the sink.
    let result = wrapper.send(&dummy_request()).await;
    assert!(result.is_err());

    let events = sink.events();
    assert_eq!(
        events.len(),
        1,
        "exactly one saturation event expected from the circuit-open rejection"
    );
    let ev = &events[0];
    assert_eq!(ev.kind, SaturationKind::Error);
    assert_eq!(ev.provider, "integration-sat");
    assert_eq!(ev.model, "integration-model");
    assert!(
        (ev.threshold_pct - 100.0).abs() < f32::EPSILON,
        "circuit-open events always pin threshold at 100%"
    );
}

#[tokio::test]
async fn rate_limit_exhausted_fires_saturation_event() {
    let inner: Arc<dyn Provider> = Arc::new(AlwaysErrorProvider);
    let breaker = Arc::new(CircuitBreaker::new(
        100,
        Duration::from_secs(60),
        Duration::from_secs(60),
    ));
    // Bucket with capacity=1 + initial=1 so the very first acquire
    // succeeds; the second one with max_wait=1ns fails fast
    // (would-wait-exceeded-max) and the wrapper fires the
    // rate-limit event.
    let rl = Arc::new(RateLimiter::new(moagan::config::RateLimitConfig {
        capacity: 1,
        refill_per_sec: 1,
        initial: Some(1),
    }));
    let sink = Arc::new(VecSink::default());
    let wrapper = BreakeredProvider::with_rate_limiter(inner, breaker.clone(), rl.clone())
        .with_rate_limit_max_wait(Duration::from_nanos(1))
        .with_saturation_sink(sink.clone());

    // First call drains the bucket. The inner provider then
    // returns an opening error; the breaker counts it but stays
    // closed (threshold=100). The event is NOT a saturation
    // event — it's just a call error.
    let _ = wrapper.send(&dummy_request()).await;
    assert!(
        sink.events().is_empty(),
        "first call must not fire a saturation event"
    );

    // Second call: the rate limiter rejects with a budget-exhausted
    // error. The wrapper fires SaturationKind::RateLimit through
    // the sink.
    let result = wrapper.send(&dummy_request()).await;
    assert!(result.is_err(), "second call must be rate-limit rejected");
    let events = sink.events();
    assert_eq!(
        events.len(),
        1,
        "exactly one saturation event expected from the rate-limit rejection"
    );
    let ev = &events[0];
    assert_eq!(ev.kind, SaturationKind::RateLimit);
    assert_eq!(ev.provider, "integration-sat");
    assert_eq!(ev.model, "integration-model");
    let details = ev.details.as_ref().expect("details");
    assert_eq!(
        details.get("capacity").unwrap().as_u64().unwrap(),
        1,
        "capacity must be carried in details"
    );
    assert_eq!(
        details.get("refill_per_sec").unwrap().as_u64().unwrap(),
        1,
        "refill_per_sec must be carried in details"
    );
}

#[test]
fn telemetry_mirrors_saturation_event_into_sqlite_and_jsonl() {
    moagan::test_support::with_moagan_home("telemetry_saturation_mirror", |_home| {
        let home = moagan::fs_layout::MoaganHome::resolve().unwrap();
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        let db = moagan::storage::sqlite::Db::open(&home.meta_db_path()).unwrap();
        db.register_run(run_id, "fast", "running", "0.8.0", None, None, None)
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
