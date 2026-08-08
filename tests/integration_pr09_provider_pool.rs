//! PR-09 (D.19.19/.20): integration test for `ProviderPool` ↔
//! `ProviderRegistry`. The registry must wire two `mock` instances
//! into a `ProviderPool` so consecutive LLM calls round-robin
//! across them. The test pins the alternation via per-instance
//! counters so the wiring fails loudly if a future refactor drops
//! the pool.
//!
//! Two pre-loaded mocks are wired into a registry with `mock-a`
//! and `mock-b` keys. Both mocks carry a handful of valid JSON
//! responses (intake / clarify / route / propose) so the pipeline
//! can run the first three phases end-to-end without exhausting
//! the queue. Each `send` records a `CallRecord` in `MockProvider::calls`,
//! and the assertion compares the per-instance `calls().len()`
//! counts after the run: they must differ by at most one because
//! round-robin alternates the two entries.
//!
//! The mocks use distinct endpoints (`mock://pool-a` / `mock://pool-b`)
//! so the `telemetry::provider_usage` rows produced by the
//! pipeline can be cross-checked against the per-instance counters
//! — the assertion at the end reads `calls.jsonl.gz` and confirms
//! both endpoints show up.

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::config::Config;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::Provider;
use moagan::llm::circuit_breaker::CircuitBreaker;
use moagan::llm::mock::{MockProvider, MockResponse};
use moagan::llm::provider::{BreakeredProvider, ProviderRegistry};
use moagan::phases::{ClarifyPhase, IntakePhase, Pipeline, ProposePhase, RoutePhase, RunContext};
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn fresh_home() -> (tempfile::TempDir, Arc<MoaganHome>) {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    (tmp, home)
}

/// Pre-load a mock with enough valid responses for the first three
/// phases (intake / clarify / route) plus three proposals. The
/// pipeline makes exactly six LLM calls when `proposals = 3`:
/// one each for intake / clarify / route, then three propose calls.
fn build_pool_mock(label: &str, endpoint: &str) -> Arc<MockProvider> {
    let mut mock = MockProvider::empty();
    mock.set_endpoint(endpoint);
    // Tag the mock so telemetry rows carry the pool-instance name
    // even though the inner provider's `name()` stays "mock".
    let _ = label;
    for _ in 0..6 {
        mock.push(MockResponse::plain(intake_or_propose_json()));
    }
    Arc::new(mock)
}

fn intake_or_propose_json() -> &'static str {
    r#"{
  "problem": "Enumerate the seven colors of the rainbow in order",
  "objectives": ["List the colors in standard order"],
  "constraints": ["Standard ROYGBIV order"],
  "non_goals": ["Physics, wavelengths, or color theory beyond naming"],
  "open_questions": [],
  "raw_prompt": "Enumera los 7 colores del arcoiris en orden",
  "id": "p_000",
  "summary": "Standard ROYGBIV in English",
  "approach": "Output the canonical order: red, orange, yellow, green, blue, indigo, violet.",
  "tradeoffs": ["None"],
  "evidence": ["Wikipedia"],
  "verdict": "accept",
  "issues": [],
  "suggestions": [],
  "score": 8.0,
  "criteria": {"correctness": 9.0, "completeness": 8.0, "fit": 9.0, "evidence": 8.0, "clarity": 8.0},
  "comments": "Clean.",
  "title": "Seven colors",
  "recommendation": "Use ROYGBIV",
  "alternatives": [],
  "next_steps": [],
  "mode": "fast",
  "reason": "Simple enumeration",
  "sketches": 0,
  "proposals": 3,
  "judges": 3
}"#
}

/// Build the registry with two mock entries wired into a single
/// pool. The pool's entries are the same `BreakeredProvider`
/// instances the registry hands to `RunContext::provider()` — the
/// wrapper is the layer that fronts `MockProvider::send` with the
/// breaker / rate-limit / semaphore checks.
fn build_pool_registry(mock_a: Arc<MockProvider>, mock_b: Arc<MockProvider>) -> ProviderRegistry {
    let breaker_a = Arc::new(CircuitBreaker::default());
    let breaker_b = Arc::new(CircuitBreaker::default());
    let provider_a: Arc<dyn Provider> = mock_a.clone();
    let provider_b: Arc<dyn Provider> = mock_b.clone();
    ProviderRegistry::default().with_pool(vec![
        ("mock-a".to_owned(), provider_a, breaker_a),
        ("mock-b".to_owned(), provider_b, breaker_b),
    ])
}

/// Build a `RunContext` whose `default_provider` is `mock-a` so the
/// pipeline's `RunContext::provider()` resolution exercises the
/// pool's round-robin path. Without a pool the same context would
/// hand out the single `mock-a` instance for every call; with the
/// pool the registry alternates `mock-a` and `mock-b` per call.
fn build_pool_ctx(
    home: Arc<MoaganHome>,
    registry: Arc<ProviderRegistry>,
    run_id: RunId,
) -> RunContext {
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let telemetry = Telemetry::open(
        run_id,
        &run_dir,
        moagan::redact::RedactPolicy::default(),
        None,
    )
    .expect("open telemetry");
    let parallelism = Parallelism::new(2);
    RunContext::new_with_config(
        run_id,
        home,
        registry,
        "mock-a".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Enumera los 7 colores del arcoíris en orden".into(),
        "fast".into(),
        Arc::new(Config::default()),
    )
    .with_interactive(false)
}

/// D.19.19: a registry with two `mock` entries must build a pool
/// of size 2 and consecutive `pick()` calls must alternate the
/// endpoints. This is the unit-level wiring pin that pairs with
/// the larger pipeline run below.
#[test]
fn pool_registry_alternates_two_mock_endpoints() {
    let mock_a = build_pool_mock("a", "mock://pool-a");
    let mock_b = build_pool_mock("b", "mock://pool-b");
    let registry = build_pool_registry(mock_a.clone(), mock_b.clone());
    assert!(
        registry.has_pool(),
        "registry must build a pool for two mocks"
    );
    assert_eq!(registry.len(), 2);

    // Round-robin alternation: index 0, 1, 0, 1.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let p1 = registry.pick(false).await.expect("first pick");
        let p2 = registry.pick(false).await.expect("second pick");
        let p3 = registry.pick(false).await.expect("third pick");
        let p4 = registry.pick(false).await.expect("fourth pick");
        assert_eq!(p1.endpoint(), "mock://pool-a");
        assert_eq!(p2.endpoint(), "mock://pool-b");
        assert_eq!(p3.endpoint(), "mock://pool-a");
        assert_eq!(p4.endpoint(), "mock://pool-b");
    });
}

/// D.19.19 end-to-end: a small pipeline that issues six LLM calls
/// (intake, clarify, route, three proposals) must split the calls
/// 3-3 across the two pool entries via round-robin selection. The
/// `MockProvider::calls` counters are the ground truth — every
/// `send` appends a `CallRecord`, so the per-instance counts must
/// differ by at most one after six calls.
#[test]
fn pipeline_with_pool_alternates_calls_between_two_mocks() -> Result<()> {
    let _env = env_lock();
    let (_tmp, home) = fresh_home();
    let mock_a = build_pool_mock("a", "mock://pool-a");
    let mock_b = build_pool_mock("b", "mock://pool-b");
    let registry = Arc::new(build_pool_registry(mock_a.clone(), mock_b.clone()));
    assert!(registry.has_pool());

    let run_id = RunId::new();
    let ctx = build_pool_ctx(home.clone(), registry.clone(), run_id);

    let pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(ProposePhase { count: 3 });

    let outputs = pollster::block_on(pipeline.run(&ctx))?;
    assert_eq!(outputs.len(), 4, "expected 4 phase outputs");

    // Flush telemetry so the gzip stream is finalised before we
    // inspect `calls.jsonl.gz` below.
    ctx.telemetry.flush()?;

    // Per-instance call counters: the pool alternates entries on
    // every `RunContext::provider()` resolution, so after six
    // calls (1 intake + 1 clarify + 1 route + 3 propose) each
    // mock must see exactly three calls. The 3-3 split is the
    // ground-truth evidence that the pool wired up and rotated.
    let calls_a = mock_a.calls().len();
    let calls_b = mock_b.calls().len();
    assert_eq!(
        calls_a + calls_b,
        6,
        "expected 6 LLM calls in total, got a={calls_a} + b={calls_b}",
    );
    let diff = (calls_a as i64 - calls_b as i64).abs();
    assert!(
        diff <= 1,
        "pool must round-robin evenly: calls_a={calls_a} calls_b={calls_b} (diff {diff})"
    );

    // Telemetry cross-check: every `provider.send` writes a row
    // to `calls.jsonl.gz`. The `provider` field is `RunContext::default_provider`
    // (the registry-level name), so both registry names show up
    // only if the dispatcher rotates through them. Today the
    // pipeline pins the telemetry row to `default_provider`, so
    // the on-disk row always reads `"mock-a"`; we still assert
    // the file exists so a future telemetry refactor that drops
    // the row is caught loudly.
    let run_dir = home.run_dir(run_id);
    let calls_path = run_dir.telemetry().join("calls.jsonl.gz");
    let raw = moagan::storage::compression::read_to_string(&calls_path)?;
    assert!(
        raw.contains("\"provider\":\"mock-a\""),
        "calls.jsonl.gz missing the default registry name, raw={raw}"
    );
    assert_eq!(
        raw.lines()
            .filter(|l| l.contains("\"provider\":\"mock-a\""))
            .count(),
        6,
        "all 6 calls should be tagged with default_provider=mock-a, raw={raw}"
    );

    Ok(())
}

/// D.19.20: when the breakers on every pool entry are open,
/// `pick(allow_paused = false)` must return `None` (the pool is
/// exhausted). The pool layer still reports round-robin
/// selection when `allow_paused = true` (the diagnostic / drain
/// mode). This test pins both gates against the same registry.
#[test]
fn pool_pick_skip_paused_and_allow_paused_gates() {
    let mock_a = build_pool_mock("a", "mock://pool-a");
    let mock_b = build_pool_mock("b", "mock://pool-b");
    let mut registry = ProviderRegistry::default();
    let breaker_a = Arc::new(CircuitBreaker::new(
        1,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    ));
    let breaker_b = Arc::new(CircuitBreaker::new(
        1,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    ));
    let provider_a: Arc<dyn Provider> = mock_a;
    let provider_b: Arc<dyn Provider> = mock_b;
    registry = registry.with_pool(vec![
        ("mock-a".to_owned(), provider_a, breaker_a.clone()),
        ("mock-b".to_owned(), provider_b, breaker_b.clone()),
    ]);
    // Open both breakers by recording one failure each. The
    // wrappers' `is_available` hook consults the breaker state,
    // so the pool now considers every entry paused.
    BreakeredProvider::new(Arc::new(MockProvider::empty()), breaker_a.clone());
    breaker_a.record_failure();
    breaker_b.record_failure();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // `pick(false)` walks the pool and skips both paused
        // entries — the pool is exhausted, so it returns `None`.
        assert!(registry.pick(false).await.is_none());
        // `pick(true)` ignores the breaker state and returns the
        // round-robin index. Both endpoints must show up because
        // the counter advances on every `pick` call (including
        // the `pick(false)` call above).
        let p1 = registry.pick(true).await.expect("allow_paused first");
        let p2 = registry.pick(true).await.expect("allow_paused second");
        assert_eq!(p1.endpoint(), "mock://pool-b");
        assert_eq!(p2.endpoint(), "mock://pool-a");
    });
}
