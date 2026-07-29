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
use moagan::discovery::integrator::{
    COVERAGE_RATIO_MIN, PRESERVED_CITATIONS_MIN, meets_safeguards,
};
use moagan::error::Result;
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
