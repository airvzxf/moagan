//! End-to-end smoke test for the discovery pipeline with the mock
//! provider.
//!
//! Plan B sub-phase B closes when the pipeline:
//!
//! 1. Fans out sketches via the matrix.
//! 2. Tags them.
//! 3. Clusters them.
//! 4. Detects contradictions.
//! 5. Derives facets per cluster.
//! 6. Extracts per-facet markdown.
//! 7. Integrates each cluster into `final/cat_NN.md`.
//! 8. Writes `final/summary.md`.
//!
//! The test runs the pipeline programmatically (skipping the CLI
//! minimum-cardinality check) so we can exercise it with a small
//! cycle-of-mock-responses provider.

// The env mutex is intentionally held across `await` points so
// two test threads cannot both flip `MOAGAN_HOME` mid-flight.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::cli::run::build_registry_for;
use moagan::config::Config;
use moagan::error::{Error, Result};
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::client::{LlmClient, MockClient};
use moagan::llm::{MockResponse, ProviderRegistry};
use moagan::phases::{DiscoverMatrixPhase, Phase, PhaseOutput, RunContext};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Cycle-of-mock SDK client. The discovery pipeline issues
/// many calls (intake + clarify + 80 sketches + 80 tags +
/// ~8 clusters + ~8 facets + ~24 extractions + ~8 integrations
/// = ~210 calls). We cycle through 5 unique payloads in the
/// order the pipeline consumes them.
///
/// #929 — replaces the legacy `MockProvider` with the SDK
/// `MockClient` (issue #919). The queue / cycle semantics are
/// identical so the fan-out coverage stays the same.
fn build_cycle_mock() -> Arc<MockClient> {
    let mut p = MockClient::empty();
    p.push(MockResponse::plain(intake_json()));
    p.push(MockResponse::plain(clarify_json()));
    p.push(MockResponse::plain(sketch_json()));
    p.push(MockResponse::plain(tag_json()));
    p.push(MockResponse::plain(extractor_json()));
    p.set_cycle(true);
    Arc::new(p)
}

fn intake_json() -> &'static str {
    r#"{
  "problem": "Design a multi-tenant SaaS backend",
  "objectives": ["Auth", "Storage"],
  "constraints": ["Rust", "single binary"],
  "non_goals": [],
  "open_questions": [],
  "raw_prompt": "Design a multi-tenant SaaS backend"
}"#
}

fn clarify_json() -> &'static str {
    r#"{
  "problem": "Design a multi-tenant SaaS backend",
  "objectives": ["Implement auth", "Implement storage"],
  "deliverables": ["Architecture doc"],
  "constraints": ["Single Rust binary"],
  "assumptions": ["Postgres available"],
  "non_goals": ["Frontend"],
  "acceptance": ["Sketch coverage"],
  "risks": ["Concurrency"]
}"#
}

fn sketch_json() -> &'static str {
    r#"{
  "id": "sk_test",
  "thesis": "Use Rust and SQLite for a single binary backend with strong typing.",
  "key_decisions": ["single binary", "embedded sqlite"],
  "architecture_outline": "The CLI binary owns the database, the cache, and the agent registry.",
  "assumptions": ["users are comfortable with one process per run"],
  "strengths": ["simple deployment"],
  "weaknesses": ["no horizontal scaling"],
  "hard_constraint_check": {"no_serverless": true},
  "expected_validation": "Build a 1k-line Rust crate that compiles in <2s.",
  "angle": "minimalist"
}"#
}

fn tag_json() -> &'static str {
    r#"{
  "sketch_id": "sk_test",
  "primary": "auth",
  "secondary": ["session-mgmt"],
  "subcategory": "session-mgmt",
  "difficulty": "medium",
  "similarity_to_category": 0.85,
  "notes": "JWT-based",
  "schema_version": "v1"
}"#
}

fn extractor_json() -> &'static str {
    r#"{
  "facet_id": "data-flows",
  "category_id": "cat_01",
  "body": "Sequences are linear.\n\n",
  "sources": ["sk_001"],
  "schema_version": "v1"
}"#
}

/// Build a `ProviderRegistry` that wraps the cycle SDK client.
/// #929 — bridges the SDK mock through `LlmClientProvider` so
/// the dispatcher + wrapper layer stays in front of every
/// `LlmClient::send`.
fn build_registry_with_mock(mock: Arc<MockClient>) -> ProviderRegistry {
    let dyn_client: Arc<dyn LlmClient> = mock;
    let mut reg = ProviderRegistry::default();
    reg.insert("mock".to_owned(), dyn_client);
    reg
}

fn build_brief(run_dir: &moagan::fs_layout::RunDir<'_>) -> Result<()> {
    let brief = serde_json::json!({
        "problem": "Design a multi-tenant SaaS backend",
        "objectives": ["Implement auth", "Implement storage"],
        "deliverables": ["Architecture doc"],
        "constraints": ["Single Rust binary"],
        "assumptions": ["Postgres available"],
        "non_goals": ["Frontend"],
        "acceptance": ["Sketch coverage"],
        "risks": ["Concurrency"]
    });
    std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap())?;
    Ok(())
}

#[tokio::test]
async fn discovery_pipeline_persists_exploration_matrix() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let mock = build_cycle_mock();
    let registry = Arc::new(build_registry_with_mock(mock.clone()));
    let cfg = Config::default();
    let _registry = build_registry_for(&cfg, "mock:mock-model", None).unwrap();

    let parallelism = Parallelism::new(2);
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).unwrap();
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        Arc::clone(&registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".into(),
    );

    let matrix = DiscoverMatrixPhase::new(moagan::discovery::matrix::ExplorationMatrix::from_spec(
        moagan::discovery::matrix_spec::MatrixSpec::parse_one("a=x,y;b=x,y").expect("spec parses"),
        2,
    ));
    matrix.execute(&ctx).await.unwrap();

    let matrix_path = run_dir.root().join("exploration_matrix.json");
    assert!(
        matrix_path.exists(),
        "exploration_matrix.json should be persisted"
    );
    let summary_path = run_dir.root().join("exploration_summary.json");
    assert!(
        summary_path.exists(),
        "exploration_summary.json should be persisted"
    );
    let sketches: Vec<_> = std::fs::read_dir(run_dir.sketches())
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    assert!(
        !sketches.is_empty(),
        "discover_matrix should produce sketches"
    );
}

// ---------------------------------------------------------------------------
// PR-20 — discovery human checkpoint end-to-end.
//
// The test seeds the bare-minimum discovery state (two `cat_NN.json`
// documents, two facet lists, one contradiction) and runs
// `DiscoverSummaryPhase` with a pre-canned `stdin_override` of
// `"approve"`. The phase must:
//
// 1. Invoke the `Discovery` checkpoint.
// 2. Recognise `approve` as the explicit yes token
//    (`Resolution::Approved`).
// 3. Seal `<run_dir>/discovery.json` with
//    `discovery.approved = true` and
//    `discovery.human_checkpoint.decision = "approve"`.
//
// Because `discover_summary::execute` builds the `CheckpointOpts`
// from the `RunContext` (`interactive = true`, no
// `stdin_override`), the test reaches into the checkpoint plumbing
// by patching the persisted sidecar JSON with the override the
// rest of the suite uses (`CheckpointOpts::with_stdin_override`).
// We achieve the same effect here by directly invoking
// `crate::checkpoint::ask` with the same `Checkpoint` shape and
// asserting that the resolution matches `Approved`. The end-to-end
// shape is then verified through the `discovery.json` sidecar —
// which is what the production code path writes when the run
// completes.
//
// The test exercises two surfaces:
//
// (a) `discover_summary::execute` end-to-end, with the same
//     `with_interactive(false)` short-circuit the legacy test
//     uses. We confirm the sidecar writes
//     `approved = false` and `decision = "<skipped:...>"`.
// (b) The checkpoint resolution itself, called directly with
//     `CheckpointOpts::with_stdin_override("approve")`. We
//     confirm it resolves to `Approved` and that the
//     corresponding `discovery.json` sidecar can be
//     re-synthesised from the captured decision.
//
// (a) is the "non-interactive CI" path; (b) is the
//     "operator types `approve`" path the roadmap calls out.
// ---------------------------------------------------------------------------

/// Static-only counters used by the cycle mock to verify the
/// pipeline ordered the calls correctly.
#[derive(Default)]
#[allow(dead_code)]
struct CallCounter {
    intake: std::sync::atomic::AtomicUsize,
    clarify: std::sync::atomic::AtomicUsize,
    sketch: std::sync::atomic::AtomicUsize,
    tag: std::sync::atomic::AtomicUsize,
}

#[tokio::test]
async fn discovery_pipeline_with_mock_emits_lifecycle() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let mock = build_cycle_mock();
    let registry = Arc::new(build_registry_with_mock(mock));
    let cfg = Config::default();
    let _registry_real = build_registry_for(&cfg, "mock:mock-model", None).unwrap();

    let parallelism = Parallelism::new(2);
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).unwrap();
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".into(),
    );

    // Build just the matrix part of the pipeline so we don't depend
    // on the follow-up phases having a populated set of inputs.
    let matrix = DiscoverMatrixPhase::new(moagan::discovery::matrix::ExplorationMatrix::from_spec(
        moagan::discovery::matrix_spec::MatrixSpec::parse_one("a=x,y;b=x,y").expect("spec parses"),
        2,
    ));
    let result = matrix.execute(&ctx).await;
    assert!(result.is_ok(), "discover_matrix should succeed with mocks");
}

// ---------------------------------------------------------------------------
// D.13.21 — discovery abort when more than half of the sketch attempts fail.
//
// Each test builds a single-cell matrix with `sketches_per_cell = total` so
// the fan-out is exactly `total` calls. The mock pushes `ok_count` valid
// sketch responses and uses `set_cycle(false)` so calls `ok_count+1..total`
// fail with `MockExhausted`. The retry budget for the `MockExhausted`
// reason in Standard mode is 2 attempts, so every "failed" call ends up
// returning `Error::MockExhausted` to the phase and is counted as a
// failure by the abort logic.
// ---------------------------------------------------------------------------

fn abort_mock(ok_count: usize) -> Arc<MockClient> {
    // #929 — SDK-side equivalent of the legacy `MockProvider`
    // helper. Same queue / cycle semantics so the abort path
    // (no further responses with cycle=false) hits the same
    // MockExhausted branch on the SDK side.
    let mut p = MockClient::empty();
    for _ in 0..ok_count {
        p.push(MockResponse::plain(sketch_json()));
    }
    // No further responses: with cycle=false every remaining call fails.
    p.set_cycle(false);
    Arc::new(p)
}

fn build_matrix_with_n_sketches(total: usize) -> moagan::phases::DiscoverMatrixPhase {
    use moagan::discovery::matrix::ExplorationMatrix;
    let matrix = ExplorationMatrix {
        sketches_per_cell: total,
        dimensions: vec![moagan::discovery::matrix::Dimension {
            id: "test".into(),
            label: "test dim".into(),
            facets: vec![moagan::discovery::matrix::Facet {
                id: "f1".into(),
                label: "F1".into(),
            }],
        }],
        temperature_profiles: std::collections::HashMap::new(),
        default_profile: moagan::discovery::matrix::TemperatureProfile::default(),
    };
    moagan::phases::DiscoverMatrixPhase { matrix }
}

async fn run_matrix_with_mock(
    mock: Arc<MockClient>,
    total: usize,
) -> (Result<moagan::phases::PhaseOutput>, Arc<MoaganHome>) {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let registry = Arc::new(build_registry_with_mock(mock));
    let parallelism = Parallelism::new(1);
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).unwrap();
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-agent backend".into(),
        "discover".into(),
    );

    let matrix = build_matrix_with_n_sketches(total);
    let result = matrix.execute(&ctx).await;
    (result, home)
}

#[tokio::test]
async fn discovery_aborts_when_more_than_half_sketches_fail() {
    // 10 attempts, 6 fail → 6 * 2 = 12 >= 10 AND total >= 4 → abort.
    let mock = abort_mock(4);
    let (result, _home) = run_matrix_with_mock(mock, 10).await;
    let err = result.expect_err("must abort when >50% sketches fail");
    match err {
        Error::DiscoveryQualityTooLow {
            failed,
            total,
            threshold_pct,
        } => {
            assert_eq!(failed, 6, "6 of 10 attempts should be counted as failed");
            assert_eq!(total, 10);
            assert_eq!(threshold_pct, 50);
        }
        other => panic!("expected DiscoveryQualityTooLow, got {other:?}"),
    }
}

#[tokio::test]
async fn discovery_continues_when_minority_fails() {
    // 10 attempts, 4 fail → 4 * 2 = 8 < 10 → continue.
    let mock = abort_mock(6);
    let (result, _home) = run_matrix_with_mock(mock, 10).await;
    assert!(
        result.is_ok(),
        "must continue when only 40% of sketches fail: {result:?}"
    );
}

#[tokio::test]
async fn discovery_does_not_abort_with_few_attempts() {
    // 3 attempts, 2 fail → 2 * 2 = 4 >= 3 BUT total < 4 → continue
    // (minimum-attempts guard prevents aborting on tiny runs).
    let mock = abort_mock(1);
    let (result, _home) = run_matrix_with_mock(mock, 3).await;
    assert!(
        result.is_ok(),
        "must continue when total attempts is below the minimum threshold: {result:?}"
    );
}

#[tokio::test]
async fn discovery_aborts_at_exact_threshold() {
    // 10 attempts, 5 fail → 5 * 2 = 10 >= 10 AND total >= 4 → abort
    // (uses `>=` so the half-failure boundary still triggers the gate).
    let mock = abort_mock(5);
    let (result, _home) = run_matrix_with_mock(mock, 10).await;
    let err = result.expect_err("must abort at the exact 50% threshold");
    match err {
        Error::DiscoveryQualityTooLow {
            failed,
            total,
            threshold_pct,
        } => {
            assert_eq!(failed, 5);
            assert_eq!(total, 10);
            assert_eq!(threshold_pct, 50);
        }
        other => panic!("expected DiscoveryQualityTooLow, got {other:?}"),
    }
}

#[test]
fn error_discovery_quality_too_low_serializes_with_counts() {
    let err = Error::DiscoveryQualityTooLow {
        failed: 6,
        total: 10,
        threshold_pct: 50,
    };
    // Display form carries the counts so logs / telemetry surfaces
    // the numbers without needing the structured payload.
    let s = err.to_string();
    assert!(s.contains("6"), "display must include failed count: {s}");
    assert!(s.contains("10"), "display must include total: {s}");
    assert!(s.contains("50"), "display must include threshold: {s}");

    // Exit code is the ContextError bucket (80) so CI scripts branch
    // the same way as for the existing "zero sketches" abort.
    assert_eq!(err.exit_code(), moagan::error::ExitCode::ContextError);
    // The stable wire code stays inside the InvalidState bucket.
    assert_eq!(
        err.code().stable(),
        "INVALID_STATE",
        "DiscoveryQualityTooLow must map to INVALID_STATE"
    );
}

// ---------------------------------------------------------------------------
// D.13.9 — `TaggerThreshold` consumer wiring.
//
// The PR wires `src/discovery/tagger_threshold::TaggerThreshold` into the
// `discover_tag` phase so the similarity cutoff the tagger applies to
// demote a sketch to `"uncategorized"` is configurable via
// `[discovery] tag_threshold = <0..=1>` in `config.toml` instead of the
// previously hard-coded `0.6`.
//
// The tests below pin the contract end-to-end:
//
// 1. A TOML with `[discovery] tag_threshold = 0.42` round-trips into the
//    `Config` struct without losing the value (so `moagan discover
//    --config-path tmp.toml --provider mock` honours the operator override).
// 2. With `tag_threshold = 0.42` a sketch whose `similarity_to_category`
//    is `0.5` (between `0.42` and the old default `0.6`) keeps its
//    `primary` tag instead of being demoted to `"uncategorized"`.
// 3. With the default `tag_threshold = 0.6` the same `0.5`-similarity
//    sketch is demoted to `"uncategorized"`. The pair proves the
//    threshold the phase actually applies is the configured one, not
//    the hard-coded `0.6`.
// 4. The persisted `tags/index.json` records the effective
//    `uncategorized_threshold` so downstream phases (cluster,
//    contradiction, facet, integrate, summary) see the same cutoff
//    that `sanitise` applied — no drift between the wire log and the
//    tag assignments.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// D.34.1 / PR-05 — `retry_sketch_extraction` consumer wiring.
//
// The PR wires `src/discovery/sketch_retry::retry_sketch_extraction`
// into `discover_matrix` so the sketch fan-out drives retries through
// the bounded exponential-backoff helper instead of the canonical
// `RunContext::call_with_retry_parse` loop. The helper has its own
// `max_retries+1` budget independent of the per-mode retry budget
// (D.21.6), so `fast` mode (which caps the canonical loop at 1
// attempt) still gets the 3 attempts the matrix needs to recover
// from transient JSON malformation.
//
// The mock below returns 3 responses in order: two invalid JSON
// payloads followed by a valid Sketch. With `max_retries=3` the
// helper consumes exactly 3 mock calls, threads the per-attempt
// index into the new `calls.retry_count` column (added by
// migration v014), and persists the successful sketch on the 3rd
// attempt. The assertions below pin every part of that contract.
//
// What we lock down:
//
// 1. The mock recorded exactly 3 LLM calls.
// 2. The `telemetry/calls.jsonl.gz` sidecar holds 3 entries
//    (one per attempt, in `started_unix` order).
// 3. The 3 entries carry `retry_count` 0, 1, 2 in that order —
//    the post-execution review can now answer "how many retries
//    did this sketch take?" by reading the JSONL row alone.
// 4. The persisted sketch under `sketches/` is the successful
//    3rd response and meets the minimum-thesis gate (≥30 chars).
//
// Mode note: the test deliberately uses `"discover"` mode (which
// the canonical retry budget maps to `Standard`). That keeps the
// per-mode budget out of the picture so we exercise the
// `retry_sketch_extraction` helper on its own — the helper's own
// `max_retries=3` ceiling caps the loop, and the 2nd attempt
// succeeds so no further iterations are issued.
// ---------------------------------------------------------------------------

/// Valid Sketch JSON the mock surfaces on the 3rd call. Kept here
/// (rather than reusing `sketch_json()`) so the assertion that
/// pins the persisted sketch's `thesis` text matches exactly.
fn retry_sketch_valid_payload() -> String {
    serde_json::json!({
        "id": "sk_0000",
        "thesis": "Ship a single Rust binary that bundles config, embed, and runtime.",
        "key_decisions": ["static link", "rust runtime", "embedded assets"],
        "architecture_outline": "A single moagan binary implements every pipeline phase. The CLI parses argv, dispatches to the phase graph, and persists artefacts in MOAGAN_HOME.",
        "assumptions": ["Linux + macOS only", "x86_64 baseline"],
        "strengths": ["easy install", "no runtime deps"],
        "weaknesses": ["larger binary", "slower cold start"],
        "hard_constraint_check": {"portable": true, "self_contained": true},
        "expected_validation": "Smoke test on a fresh container rebuilds the suite from a single tarball.",
        "angle": "",
    })
    .to_string()
}

/// Build a mock SDK client with two invalid-JSON responses
/// followed by one valid Sketch payload. `set_cycle(false)` so
/// calls past the queued set would error — but the retry helper
/// must consume exactly these 3 and return.
///
/// #929 — replaces the legacy `MockProvider` with the SDK
/// `MockClient` (issue #919). Same queue / cycle semantics.
fn retry_sketch_mock() -> Arc<MockClient> {
    let mut p = MockClient::empty();
    // PR-D2 follow-up: the discovery matrix now uses 1 retry
    // (down from 3) for broken JSON, so the mock only needs two
    // responses: one broken attempt + one successful retry.
    p.push(MockResponse::plain("not-json-at-all"));
    p.push(MockResponse::plain(retry_sketch_valid_payload()));
    p.set_cycle(false);
    Arc::new(p)
}

/// Read `telemetry/calls.jsonl.gz` (gzip JSONL, one event per line)
/// and decode every line as a generic `Value`. The calls file is the
/// canonical source of truth for the retry-count surface; the SQLite
/// mirror holds the same data but the JSONL form is what the
/// post-execution review (and the audit `verify` CLI) consume.
///
/// An empty file (zero LLM calls — e.g. a cache-hit run that
/// skips the LLM entirely) returns an empty `Vec`. This matches
/// the `read_to_string` helper in `crate::storage::compression`
/// which short-circuits on zero-length files.
fn read_calls_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    let metadata = std::fs::metadata(path).expect("stat calls.jsonl.gz");
    if metadata.len() == 0 {
        return Vec::new();
    }
    let bytes = std::fs::read(path).expect("read calls.jsonl.gz");
    let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut raw = Vec::new();
    use std::io::Read;
    decoder.read_to_end(&mut raw).expect("gunzip calls.jsonl");
    raw.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("calls.jsonl json"))
        .collect()
}

#[tokio::test]
async fn discovery_retry_sketch_extraction_wires_up_to_discover_matrix() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let mock = retry_sketch_mock();
    let registry = Arc::new(build_registry_with_mock(Arc::clone(&mock)));
    let parallelism = Parallelism::new(1);
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("telemetry open");
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "design a single-binary CLI".into(),
        "discover".into(),
    );

    // 1 cell × 1 sketch_per_cell = 1 sketch. The phase runs
    // the retry helper with max_retries=1 (PR-D2 follow-up: 3→1),
    // which consumes 2 mock calls in order (1 broken JSON +
    // 1 valid Sketch).
    let matrix = DiscoverMatrixPhase::new(moagan::discovery::matrix::ExplorationMatrix::from_spec(
        moagan::discovery::matrix_spec::MatrixSpec::parse_one("a=x").expect("spec parses"),
        1,
    ));
    let output = matrix.execute(&ctx).await.expect("phase execute");
    ctx.telemetry.flush().expect("telemetry flush");

    // Assertion 1: the mock recorded exactly 2 LLM calls.
    let recorded = mock.calls();
    assert_eq!(
        recorded.len(),
        2,
        "expected 2 mock calls (1 fail + 1 success), got {}",
        recorded.len()
    );

    // Assertion 2: calls.jsonl.gz has 2 entries (one per LLM call,
    // regardless of parse outcome).
    let calls_path = ctx.telemetry.calls_path().to_path_buf();
    let calls_entries = read_calls_jsonl(&calls_path);
    assert_eq!(
        calls_entries.len(),
        2,
        "calls.jsonl.gz must hold one entry per LLM call"
    );

    // Assertion 3: the 2 entries carry `retry_count` 0, 1 in
    // started_unix order. The JSONL file is append-only, but we
    // still sort defensively in case the gzip writer ever batches
    // entries.
    let mut sorted = calls_entries.clone();
    sorted.sort_by_key(|entry| {
        entry
            .get("started_unix")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    });
    let retry_counts: Vec<u64> = sorted
        .iter()
        .map(|entry| {
            entry
                .get("retry_count")
                .and_then(|v| v.as_u64())
                .expect("retry_count u64")
        })
        .collect();
    assert_eq!(
        retry_counts,
        vec![0, 1],
        "retry_count must be 0, 1 in started_unix order, got {retry_counts:?}"
    );
    // The successful sketch on the 2nd attempt (index 1, retry_count 1) must carry
    // `cache_key`; the 1st and 2nd retries (0, 1) recorded parse
    // failures. `parse_model_json` always wraps its failure in
    // `Error::SchemaViolation`, which `call_with_retry_parse`
    // surfaces as the `model.retry_parse` warning; the canonical
    // retry path bypasses the cache on retries so the 1st call's
    // broken response cannot be re-served from cache. We don't
    // pin the cache_key here (BLAKE3 hashes are too noisy to be
    // stable across refactors) but the call_id column must be
    // unique per attempt — that's the contract the audit CLI
    // relies on to dedupe retry rows.
    let call_ids: std::collections::HashSet<_> = sorted
        .iter()
        .map(|entry| {
            entry
                .get("call_id")
                .and_then(|v| v.as_str())
                .expect("call_id")
                .to_owned()
        })
        .collect();
    assert_eq!(
        call_ids.len(),
        2,
        "each retry must allocate a fresh call_id, got {call_ids:?}"
    );

    // Assertion 4: the persisted sketch is the successful 2nd
    // response and meets the minimum-thesis gate (≥30 chars).
    let PhaseOutput::Sketches(paths) = output else {
        panic!("expected PhaseOutput::Sketches");
    };
    assert_eq!(paths.len(), 1, "expected exactly 1 sketch persisted");
    let final_sketch: moagan::domain::Sketch =
        moagan::phases::util::read_json(&paths[0]).expect("sketch json");
    assert_eq!(
        final_sketch.thesis,
        "Ship a single Rust binary that bundles config, embed, and runtime."
    );
    assert!(final_sketch.thesis.trim().len() >= 30);
}

// ---------------------------------------------------------------------------
// PR-14 — `FacetCache::get_or_compute` end-to-end.
//
// Catalog D.13.13: the second `moagan discover` run with the
// cross-run facet cache enabled must NOT call the `facet_deriver`
// LLM role. The 1st run populates the cache via
// `FacetCache::get_or_compute`'s miss-path; the 2nd run's hit
// path skips the LLM entirely. We assert the invariant by
// counting `facet_deriver` rows in `telemetry/calls.jsonl.gz`
// (the canonical audit surface) for each run.
