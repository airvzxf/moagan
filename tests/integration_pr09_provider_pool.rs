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
//!
//! #933 follow-up: the per-provider `ProviderPool` was retired
//! in favour of the per-`(provider, role)` governor on
//! `RunContext::breaker_per_role` + `RunContext::throttle`. The
//! pool accessors (`with_pool`, `has_pool`, `pick`) and the
//! `BreakeredProvider::new(inner, breaker)` two-arg constructor
//! the pool built with no longer exist. Marked `#[ignore]` until
//! #934 ports the assertions to the new breaker-per-role model.

// TODO: #934 follow-up — port to post-#933 breaker-per-role governor.
// The tests reference deleted items:
//   * `ProviderRegistry::with_pool(vec![(name, provider, breaker)])`
//   * `ProviderRegistry::has_pool` / `pick(allow_paused)`
//   * `BreakeredProvider::new(provider, breaker)` (two-arg signature)
// The new model exposes the breaker / governor on
// `RunContext::breaker_per_role` + `RunContext::throttle`; the
// pool's "round-robin across N replicas" semantics live at the
// operator's reverse-proxy layer (post-#933). Until #934 ports
// them, all three tests are gated behind `#[ignore]`.

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::config::Config;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::client::{LlmClient, MockClient};
use moagan::llm::mock::MockResponse;
use moagan::llm::provider::ProviderRegistry;
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

/// Pre-load an SDK mock with enough valid responses for the first three
/// phases (intake / clarify / route) plus three proposals. The
/// pipeline makes exactly six LLM calls when `proposals = 3`:
/// one each for intake / clarify / route, then three propose calls.
///
/// #929 — the SDK mock (`MockClient` from issue #919) replaces
/// the legacy `MockProvider`. Both have identical queue
/// semantics (`push`, `set_cycle`, `endpoint`) so the pool
/// wiring stays byte-identical to the pre-#919 path.
fn build_pool_mock(label: &str, endpoint: &str) -> Arc<MockClient> {
    let mut mock = MockClient::empty();
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
/// instances the registry hands to `RunContext::provider()` —
/// the wrapper fronts `MockClient::send` (via the
/// `LlmClientProvider` bridge) with the breaker / rate-limit /
/// semaphore checks.
///
/// #929 — the SDK mocks are bridged into the legacy registry
/// through `LlmClientProvider` so the dispatcher + wrapper layer
/// stays in front of every `LlmClient::send`.
///
/// #933 — the per-provider pool was retired. The helper now
/// returns an empty registry; the `with_pool` wiring the tests
/// assert against no longer exists. Marked `#[ignore]` until
/// #934 ports the assertions to the new model.
fn build_pool_registry(_mock_a: Arc<MockClient>, _mock_b: Arc<MockClient>) -> ProviderRegistry {
    let _ = (
        moagan::llm::circuit_breaker::CircuitBreaker::default(),
        moagan::llm::circuit_breaker::CircuitBreaker::default(),
    );
    ProviderRegistry::default()
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
#[ignore = "TODO: #934 follow-up — ProviderPool was retired by #933; pool round-robin accessors removed"]
#[test]
fn pool_registry_alternates_two_mock_endpoints() {
    let _mock_a = build_pool_mock("a", "mock://pool-a");
    let _mock_b = build_pool_mock("b", "mock://pool-b");
    let registry = build_pool_registry(_mock_a.clone(), _mock_b.clone());
    // #933: `has_pool` / `pick` / `with_pool` were retired; the
    // registry is now a single-entry map. The pool-alternation
    // contract is #934's problem to re-pin against the new model.
    let _ = registry;
}

/// D.19.19 end-to-end: a small pipeline that issues six LLM calls
/// (intake, clarify, route, three proposals) must split the calls
/// 3-3 across the two pool entries via round-robin selection. The
/// `MockProvider::calls` counters are the ground truth — every
/// `send` appends a `CallRecord`, so the per-instance counts must
/// differ by at most one after six calls.
#[ignore = "TODO: #934 follow-up — ProviderPool was retired by #933; pool round-robin accessors removed"]
#[test]
fn pipeline_with_pool_alternates_calls_between_two_mocks() -> Result<()> {
    let _env = env_lock();
    let (_tmp, home) = fresh_home();
    let mock_a = build_pool_mock("a", "mock://pool-a");
    let mock_b = build_pool_mock("b", "mock://pool-b");
    let registry = Arc::new(build_pool_registry(mock_a.clone(), mock_b.clone()));
    // #933: `has_pool` was retired.

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
#[ignore = "TODO: #934 follow-up — ProviderPool was retired by #933; pool round-robin accessors removed"]
#[test]
fn pool_pick_skip_paused_and_allow_paused_gates() {
    let _mock_a = build_pool_mock("a", "mock://pool-a");
    let _mock_b = build_pool_mock("b", "mock://pool-b");
    // #933: the pool accessors (`with_pool`, `has_pool`, `pick`)
    // were retired. The two-arg `BreakeredProvider::new(provider,
    // breaker)` constructor that the pool built was also
    // retired; per-`(provider, role)` breakers now live on
    // `RunContext::breaker_per_role`. The pool-pause / pool-drain
    // contract is #934's problem to re-pin against the new model.
    let _registry = ProviderRegistry::default();
}
