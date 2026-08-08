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

use moagan::cli::discover::build_discovery_pipeline;
use moagan::cli::run::build_registry_for;
use moagan::config::Config;
use moagan::discovery::facet_cache::{DEFAULT_TTL_SECS, FacetCache};
use moagan::discovery::integrator::{
    COVERAGE_RATIO_MIN, PRESERVED_CITATIONS_MIN, meets_safeguards,
};
use moagan::error::{Error, Result};
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::MockProvider;
use moagan::llm::{MockResponse, ProviderRegistry};
use moagan::phases::{
    DiscoverClusterPhase, DiscoverContradictPhase, DiscoverExtractPhase, DiscoverFacetPhase,
    DiscoverIntegratePhase, DiscoverMatrixPhase, DiscoverSummaryPhase, DiscoverTagPhase, Phase,
    RunContext,
};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Cycle-of-mock provider. The discovery pipeline issues many
/// calls (intake + clarify + 80 sketches + 80 tags + ~8 clusters +
/// ~8 facets + ~24 extractions + ~8 integrations = ~210 calls).
/// We cycle through 5 unique payloads in the order the pipeline
/// consumes them.
fn build_cycle_mock() -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
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

/// Build a `ProviderRegistry` that wraps the cycle mock.
fn build_registry_with_mock(mock: Arc<MockProvider>) -> ProviderRegistry {
    let mut reg = ProviderRegistry::default();
    reg.insert("mock".to_owned(), mock);
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
async fn discovery_pipeline_composes_all_seven_phases() {
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

    // Build the pipeline programmatically with a small cardinality
    // so the test stays fast.
    let opts = moagan::cli::discover::DiscoverOptions {
        provider: "mock".into(),
        prompt: "Design a multi-tenant SaaS backend".into(),
        home: None,
        mock_dir: None,
        cardinality: 8,
        max_parallelism: Some(2),
        dimensions: 2,
        facets_per_dimension: 2,
        cluster_threshold: 0.7,
        out_dir: None,
        non_interactive: true,
    };
    let pipeline = build_discovery_pipeline(&opts);
    let names = pipeline.names();
    let expected = vec![
        "intake",
        "clarify",
        "discover_matrix",
        "discover_tag",
        "discover_cluster",
        "discover_contradict",
        "discover_facet",
        "discover_extract",
        "discover_integrate",
        "discover_summary",
    ];
    assert_eq!(names, expected, "pipeline order: {names:?}");
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
    let _registry = build_registry_for(&cfg, "mock", None).unwrap();

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

    let matrix = DiscoverMatrixPhase::from_dimensions(2, 2, 8);
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

#[tokio::test]
async fn discovery_cluster_phase_handles_no_sketches() {
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

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverClusterPhase::default();
    let result = phase.execute(&ctx).await;
    assert!(result.is_err(), "empty sketches must error");
}

#[tokio::test]
async fn discovery_contradict_phase_handles_one_cluster() {
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

    // Write a single cluster JSON; the phase should short-circuit.
    let cluster = moagan::domain::Cluster {
        id: "cluster_01".into(),
        label: "auth".into(),
        summary: "JWT".into(),
        category_id: String::new(),
        members: vec!["sk_001".into()],
        centroid_simhash: String::new(),
        cohesion: 0.5,
        schema_version: "v1".into(),
    };
    std::fs::write(
        run_dir.clusters().join("cluster_01.json"),
        serde_json::to_vec_pretty(&cluster).unwrap(),
    )
    .unwrap();

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverContradictPhase::default();
    let result = phase.execute(&ctx).await;
    assert!(result.is_ok(), "single cluster should be a no-op");
    let contra_path = run_dir.contradictions().join("contradictions.json");
    assert!(contra_path.exists());
}

#[tokio::test]
async fn discovery_tag_phase_requires_sketches() {
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

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverTagPhase;
    let result = phase.execute(&ctx).await;
    assert!(result.is_err(), "discover_tag without sketches must error");
}

#[tokio::test]
async fn discovery_facet_phase_requires_clusters() {
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

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverFacetPhase;
    let result = phase.execute(&ctx).await;
    assert!(
        result.is_err(),
        "discover_facet without clusters must error"
    );
}

#[tokio::test]
async fn discovery_extract_phase_requires_facet_lists() {
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

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverExtractPhase;
    let result = phase.execute(&ctx).await;
    assert!(
        result.is_err(),
        "discover_extract without facet lists must error"
    );
}

#[tokio::test]
async fn discovery_integrate_phase_requires_facet_lists() {
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

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverIntegratePhase;
    let result = phase.execute(&ctx).await;
    assert!(
        result.is_err(),
        "discover_integrate without facet lists must error"
    );
}

#[tokio::test]
async fn discovery_summary_phase_writes_summary_md() {
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

    let parallel = Parallelism::new(1);
    let telemetry = Telemetry::noop();
    let registry = Arc::new(ProviderRegistry::default());
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        registry,
        "mock".into(),
        "mock-model".into(),
        parallel,
        telemetry,
        "p".into(),
        "discover".into(),
    );

    let phase = DiscoverSummaryPhase;
    let result = phase.execute(&ctx).await;
    if let Err(e) = &result {
        eprintln!("summary error: {e:?}");
        eprintln!("sketches dir: {:?}", ctx.run_dir().sketches());
        eprintln!("final dir: {:?}", ctx.run_dir().final_dir());
    }
    assert!(
        result.is_ok(),
        "summary phase should succeed on an empty run"
    );
    let summary = run_dir.final_dir().join("summary.md");
    assert!(summary.exists());
    let text = std::fs::read_to_string(&summary).unwrap();
    assert!(text.contains("Discovery summary"));
    assert!(text.contains("Total sketches: **0**"));
    assert!(text.contains("Categories: **0**"));
}

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
    let _registry_real = build_registry_for(&cfg, "mock", None).unwrap();

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
    let matrix = DiscoverMatrixPhase::from_dimensions(2, 2, 8);
    let result = matrix.execute(&ctx).await;
    assert!(result.is_ok(), "discover_matrix should succeed with mocks");
}

#[test]
fn safeguard_thresholds_are_documented() {
    // Pin the catalog decision 42 + V4 §6.10 numbers.
    assert!((COVERAGE_RATIO_MIN - 0.85).abs() < 1e-6);
    assert!((PRESERVED_CITATIONS_MIN - 0.9).abs() < 1e-6);
}

#[test]
fn safeguard_passes_when_refined_preserves_everything() {
    let a = "body cites sk_001 and sk_002 throughout. ".repeat(20);
    let b = format!("{a}extra suffix");
    assert!(meets_safeguards(&a, &b).is_ok());
}

#[test]
fn safeguard_fails_when_refined_is_much_shorter() {
    let a = "x".repeat(1000);
    let b = "tiny";
    let err = meets_safeguards(&a, b).unwrap_err();
    assert!(err.contains("coverage_ratio"));
}

#[test]
fn safeguard_fails_when_citations_dropped() {
    let a = "sk_001 sk_002 sk_003 sk_004 sk_005 sk_006 sk_007 sk_008 sk_009 sk_010";
    let b = "sk_001 only one";
    let err = meets_safeguards(a, b).unwrap_err();
    assert!(err.contains("preserved_citations"));
}

#[test]
fn facet_cache_default_ttl_is_one_week() {
    // Pins catalog decision D.6.3 default (7 days).
    assert_eq!(DEFAULT_TTL_SECS, 7 * 24 * 60 * 60);
}

#[test]
fn facet_cache_round_trip_persists_to_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = FacetCache::new(tmp.path(), Some(60));
    let list = moagan::domain::FacetList::from_triples(
        "cat_01",
        "cluster_01",
        "brief",
        1_700_000_000,
        vec![("Data Flows".into(), "flows".into(), true)],
    );
    let path = cache.store(&list).unwrap();
    assert!(path.exists(), "store must write to disk");

    // Re-open with the same root — the entry must be there.
    let reopened = FacetCache::new(tmp.path(), Some(60));
    let hit = reopened.lookup(&list.cache_key).unwrap();
    assert!(hit.is_some());
    let hit = hit.unwrap();
    assert_eq!(hit.facets.len(), 1);
    assert_eq!(hit.facets[0].id, "data-flows");
}

#[test]
fn facet_cache_moagan_home_dir_path_helper() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = MoaganHome::resolve().unwrap();
    let path = home.cross_run_facet_cache_dir();
    assert!(path.ends_with("cache/facets"));
}

#[test]
fn facet_cache_invalidate_clears_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = FacetCache::new(tmp.path(), Some(60));
    let list = moagan::domain::FacetList::from_triples(
        "cat_01",
        "cluster_01",
        "brief",
        1_700_000_000,
        vec![("X".into(), "x".into(), true)],
    );
    cache.store(&list).unwrap();
    assert!(cache.lookup(&list.cache_key).unwrap().is_some());
    cache.invalidate(&list.cache_key).unwrap();
    assert!(cache.lookup(&list.cache_key).unwrap().is_none());
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

fn abort_mock(ok_count: usize) -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
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
    };
    moagan::phases::DiscoverMatrixPhase { matrix }
}

async fn run_matrix_with_mock(
    mock: Arc<MockProvider>,
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

/// Tag JSON whose `similarity_to_category` sits in the `[0.42, 0.6)`
/// band so the threshold knob has a visible effect: kept at `0.42`,
/// demoted at the documented `0.6` default.
fn tag_json_with_similarity(similarity: f32) -> String {
    format!(
        r#"{{
  "sketch_id": "sk_test",
  "primary": "auth",
  "secondary": ["session-mgmt"],
  "subcategory": "session-mgmt",
  "difficulty": "medium",
  "similarity_to_category": {similarity},
  "notes": "JWT-based",
  "schema_version": "v1"
}}"#
    )
}

/// Build a minimal `Sketch` JSON so the tag phase has at least one
/// sketch to fan out over. The fan-out is 1 sketch → 1 LLM tag call,
/// which keeps the test fast and deterministic.
fn sketch_only_one_json() -> &'static str {
    r#"{
  "id": "sk_test",
  "thesis": "Use Rust and SQLite for a single binary backend.",
  "key_decisions": ["single binary"],
  "architecture_outline": "Single binary.",
  "assumptions": ["users are comfortable with one process per run"],
  "strengths": ["simple deployment"],
  "weaknesses": ["no horizontal scaling"],
  "hard_constraint_check": {"no_serverless": true},
  "expected_validation": "Compiles",
  "angle": "minimalist"
}"#
}

#[test]
fn discovery_config_toml_parses_tag_threshold() {
    // Pin the canonical TOML surface so an operator typing
    // `[discovery] tag_threshold = 0.42` into `~/.config/moagan/config.toml`
    // gets exactly that value reaching `Config::discovery.tag_threshold`.
    let raw = r#"
        [discovery]
        tag_threshold = 0.42
    "#;
    let cfg: Config = toml::from_str(raw).expect("discovery tag_threshold parses");
    assert!(
        (cfg.discovery.tag_threshold - 0.42).abs() < 1e-6,
        "tag_threshold must surface exactly as written, got {}",
        cfg.discovery.tag_threshold
    );
    // Sanity: the rest of the discovery wiring defaults are preserved
    // when only one field is provided.
    assert!(!cfg.discovery.persona_enabled);
    assert!(!cfg.discovery.angle_enabled);
    assert_eq!(cfg.discovery.angle_clusters_min, 2);
}

#[test]
fn discovery_tag_threshold_default_matches_documented_default() {
    // The default of `tag_threshold` must match the documented
    // `DEFAULT_TAGGER_THRESHOLD` so existing runs are bit-identical
    // (no `Config` in the file → tagger uses 0.6 as before).
    let cfg = Config::default();
    assert!(
        (cfg.discovery.tag_threshold - 0.6).abs() < 1e-6,
        "default tag_threshold must be 0.6, got {}",
        cfg.discovery.tag_threshold
    );
}

/// Shared harness: run the tag phase against a single sketch with a
/// mock that emits `tag_json_with_similarity(similarity)` and the
/// given `tag_threshold` in the discovery config. Returns the temp
/// home (the caller must keep it alive while reading the persisted
/// files), the run dir, the tag path, and the index path.
async fn run_tag_phase_with_threshold(
    similarity: f32,
    tag_threshold: f32,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
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

    let sketch_path = run_dir.sketches().join("sk_test.json");
    std::fs::create_dir_all(run_dir.sketches()).unwrap();
    std::fs::write(&sketch_path, sketch_only_one_json().as_bytes()).unwrap();

    let mut mock = MockProvider::empty();
    mock.push(MockResponse::plain(tag_json_with_similarity(similarity)));
    let registry = Arc::new(build_registry_with_mock(Arc::new(mock)));

    let mut cfg = Config::default();
    cfg.discovery.tag_threshold = tag_threshold;

    let parallelism = Parallelism::new(1);
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).unwrap();
    let ctx = RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::clone(&registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".into(),
        Arc::new(cfg),
    );

    let phase = DiscoverTagPhase;
    let result = phase.execute(&ctx).await;
    assert!(
        result.is_ok(),
        "discover_tag must succeed with one sketch: {result:?}"
    );

    let tag_path = run_dir.tags().join("sk_test_tags.json");
    let index_path = run_dir.tags().join("index.json");
    assert!(tag_path.exists(), "tag file must be persisted");
    assert!(index_path.exists(), "index file must be persisted");
    (tmp, run_dir.root().to_path_buf(), tag_path, index_path)
}

#[tokio::test]
async fn discovery_tag_threshold_042_keeps_midrange_similarity() {
    // similarity = 0.5 is BELOW the documented default 0.6 but ABOVE
    // the configured 0.42 — so the operator's TOML override must win
    // and the tag stays as "auth" instead of being demoted.
    let (_tmp, _run_root, tag_path, index_path) = run_tag_phase_with_threshold(0.5, 0.42).await;

    let tag_text = std::fs::read_to_string(&tag_path).unwrap();
    let tag: serde_json::Value = serde_json::from_str(&tag_text).unwrap();
    assert_eq!(
        tag["primary"].as_str().unwrap(),
        "auth",
        "0.5 >= 0.42 must keep the tag, got tag JSON: {tag_text}"
    );

    // The persisted index mirrors the effective threshold so downstream
    // phases (cluster / contradict / facet / integrate / summary) see
    // the same cutoff that `sanitise` actually applied.
    let index_text = std::fs::read_to_string(&index_path).unwrap();
    let index: serde_json::Value = serde_json::from_str(&index_text).unwrap();
    assert!(
        (index["uncategorized_threshold"].as_f64().unwrap() - 0.42).abs() < 1e-6,
        "index must record the configured threshold, got: {index_text}"
    );
}

#[tokio::test]
async fn discovery_tag_threshold_default_demotes_midrange_similarity() {
    // Same sketch, same similarity = 0.5 — but with the documented
    // default (0.6) the tag must be demoted to "uncategorized". This
    // proves the wire-up is not silently pinning the old const: with
    // no override the existing behaviour (demote below 0.6) is
    // preserved.
    let (_tmp, _run_root, tag_path, index_path) = run_tag_phase_with_threshold(0.5, 0.6).await;

    let tag_text = std::fs::read_to_string(&tag_path).unwrap();
    let tag: serde_json::Value = serde_json::from_str(&tag_text).unwrap();
    assert_eq!(
        tag["primary"].as_str().unwrap(),
        "uncategorized",
        "0.5 < 0.6 must demote the tag, got tag JSON: {tag_text}"
    );

    let index_text = std::fs::read_to_string(&index_path).unwrap();
    let index: serde_json::Value = serde_json::from_str(&index_text).unwrap();
    assert!(
        (index["uncategorized_threshold"].as_f64().unwrap() - 0.6).abs() < 1e-6,
        "index must record the default threshold when not overridden, got: {index_text}"
    );
}

#[tokio::test]
async fn discovery_tag_threshold_out_of_range_falls_back_to_default() {
    // Out-of-range operator input must NOT corrupt the phase: the
    // `TaggerThreshold::from_config_value` validator clamps back to
    // the documented default so a stale / malformed TOML still
    // produces the expected demotion behaviour.
    let (_tmp, _run_root, tag_path, _index_path) = run_tag_phase_with_threshold(0.5, 1.5).await;

    let tag_text = std::fs::read_to_string(&tag_path).unwrap();
    let tag: serde_json::Value = serde_json::from_str(&tag_text).unwrap();
    assert_eq!(
        tag["primary"].as_str().unwrap(),
        "uncategorized",
        "1.5 must be rejected by the validator and fall back to 0.6, demoting the 0.5 tag; got: {tag_text}"
    );
}
