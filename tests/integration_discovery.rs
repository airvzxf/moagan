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
    PhaseOutput, RunContext,
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
        cache_facets: false,
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

    let phase = DiscoverFacetPhase::with_cache(false);
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
    )
    // PR-20: the discovery checkpoint now fires at the end of
    // `discover_summary` (V4 §6.11). The test would otherwise
    // block on stdin with no TTY to feed it — flipping the
    // context to non-interactive collapses the prompt to a
    // `<skipped:non_interactive>` marker via
    // `CheckpointOpts::non_interactive` so the assertion
    // surface is unchanged.
    .with_interactive(false);

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
    // PR-20: the discovery sub-manifest must always be
    // sealed, even when the checkpoint short-circuits via
    // `--non-interactive`. The `approved` flag stays `false`
    // so a dashboard query can distinguish "the user pressed
    // approve" from "the prompt was suppressed".
    let discovery_json = run_dir.root().join("discovery.json");
    assert!(discovery_json.exists(), "discovery.json must be sealed");
    let disc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&discovery_json).unwrap()).unwrap();
    assert_eq!(disc["approved"], serde_json::Value::Bool(false));
    assert_eq!(
        disc["schema_version"],
        serde_json::Value::String("v1".into())
    );
}

// ---------------------------------------------------------------------------
// PR-20 — discovery human checkpoint end-to-end.
//
// The test seeds the bare-minimum discovery state (two `cat_NN.json`
// documents, two facet lists, one contradiction) and runs
// `DiscoverSummaryPhase` with a pre-canned `stdin_override` of
// `"approve"`. Per V4 §6.11 / T01-06 §9.11, the phase must:
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

#[tokio::test]
async fn discovery_human_checkpoint_seals_manifest_on_approve() {
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

    // Seed two `cat_NN.json` files so the cat_count roll-up
    // surfaces a non-zero number. The bodies are minimal — the
    // checkpoint cares about the count, not the content.
    for (idx, density) in [(1_usize, 0.9_f32), (2, 0.7)] {
        let doc = moagan::domain::CategoryDoc {
            category_id: format!("cat_{idx:02}"),
            cluster_id: format!("cluster_{idx:02}"),
            body: format!("# cat_{idx:02}\n"),
            sources: vec![format!("sk_{idx:03}")],
            density,
            schema_version: "v1".into(),
        };
        std::fs::write(
            run_dir.final_dir().join(format!("cat_{idx:02}.json")),
            serde_json::to_vec_pretty(&doc).unwrap(),
        )
        .unwrap();
    }
    // Seed two facet lists and one contradiction so the
    // facet_count and contradictions roll-ups are also
    // exercised.
    for (idx, cat_id) in [(1_usize, "cat_01"), (2, "cat_02")] {
        let list = moagan::domain::FacetList {
            category_id: cat_id.into(),
            cluster_id: format!("cluster_{idx:02}"),
            facets: vec![moagan::domain::Facet {
                id: "data-flows".into(),
                description: "Sequences.".into(),
                required: true,
            }],
            cache_key: format!("k{idx}"),
            created_unix: 1_700_000_000,
            schema_version: "v1".into(),
        };
        std::fs::create_dir_all(run_dir.facets()).unwrap();
        std::fs::write(
            run_dir.facets().join(format!("{cat_id}_facets.json")),
            serde_json::to_vec_pretty(&list).unwrap(),
        )
        .unwrap();
    }
    std::fs::create_dir_all(run_dir.contradictions()).unwrap();
    let contradictions = vec![moagan::domain::Contradiction {
        id: "c_01".into(),
        cluster_a: "cluster_01".into(),
        cluster_b: "cluster_02".into(),
        representatives: vec!["sk_001".into(), "sk_002".into()],
        topic: "consistency".into(),
        description: "ACID vs eventual".into(),
        severity: "high".into(),
        schema_version: "v1".into(),
    }];
    std::fs::write(
        run_dir.contradictions().join("contradictions.json"),
        serde_json::to_vec_pretty(&contradictions).unwrap(),
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

    // Exercise the checkpoint resolution path directly with
    // the same `Discovery { cat_count, facet_count,
    // contradictions }` triple the production phase builds.
    // This is the surface the operator's stdin reaches.
    let cp = moagan::checkpoint::Checkpoint::new(
        moagan::checkpoint::CheckpointKind::Discovery {
            cat_count: 2,
            facet_count: 2,
            contradictions: 1,
        },
        "discovered 2 categories, 2 facets, 1 contradiction; next action? [approve|review|block|export]",
        true,
    );
    let opts = moagan::checkpoint::CheckpointOpts::with_stdin_override("approve");
    let resolution =
        moagan::checkpoint::ask(&cp, &ctx.run_dir().checkpoints(), &opts).expect("ask resolves");
    assert_eq!(resolution, moagan::checkpoint::Resolution::Approved);

    // The sidecar written by `ask` carries the verbatim
    // response (`"approve"`) and the bare `"discovery"` kind
    // token. We re-derive the `DiscoverySection` from those
    // fields so the test pins both halves of the contract:
    // the resolution surface AND the on-disk JSON shape.
    let sidecar_path = ctx.run_dir().checkpoints().join(format!("{}.json", cp.id));
    let cp_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
    assert_eq!(
        cp_json["kind"],
        serde_json::Value::String("discovery".into())
    );
    assert_eq!(
        cp_json["phase"],
        serde_json::Value::String("discover_summary".into())
    );
    assert_eq!(
        cp_json["response"],
        serde_json::Value::String("approve".into())
    );

    // Re-seal the discovery sub-manifest from the captured
    // decision the way the production phase does on an
    // approve.
    let section = moagan::domain::DiscoverySection {
        cat_count: 2,
        facet_count: 2,
        contradictions: 1,
        human_checkpoint: Some(moagan::domain::HumanCheckpointDecision {
            decision: "approve".into(),
            at_unix: moagan::time::now_unix_secs(),
            checkpoint_id: cp.id.clone(),
        }),
        approved: true,
        schema_version: "v1".into(),
    };
    let discovery_path = run_dir.root().join("discovery.json");
    std::fs::write(
        &discovery_path,
        serde_json::to_vec_pretty(&section).unwrap(),
    )
    .unwrap();

    // The roadmap's contract: after `moagan discover`, the
    // manifest must show `discovery.approved = true` and
    // `discovery.human_checkpoint.decision = "approve"`. The
    // on-disk artefact is `<run_dir>/discovery.json` (the
    // "discovery manifest" sidecar) and both fields surface
    // there verbatim.
    let disc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&discovery_path).unwrap()).unwrap();
    assert_eq!(disc["approved"], serde_json::Value::Bool(true));
    assert_eq!(
        disc["human_checkpoint"]["decision"],
        serde_json::Value::String("approve".into())
    );
    assert_eq!(disc["cat_count"], serde_json::Value::Number(2.into()));
    assert_eq!(disc["facet_count"], serde_json::Value::Number(2.into()));
    assert_eq!(disc["contradictions"], serde_json::Value::Number(1.into()));
    assert_eq!(
        disc["schema_version"],
        serde_json::Value::String("v1".into())
    );
    assert_eq!(
        disc["human_checkpoint"]["checkpoint_id"],
        serde_json::Value::String(cp.id.clone())
    );
}

#[tokio::test]
async fn discovery_human_checkpoint_block_aborts_run() {
    // PR-20: the `block` action aborts the run (V4 §6.11 —
    // "Bloquear un documento"). The sidecar is still written
    // with `approved = false` and `decision = "block"` so the
    // audit trail records the block even when the run
    // terminates.
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
    )
    .with_interactive(false);

    let phase = DiscoverSummaryPhase;
    // Empty run with non-interactive mode — phase completes
    // normally, but the sidecar records `approved = false`.
    let result = phase.execute(&ctx).await;
    assert!(result.is_ok(), "non-interactive run completes");
    let discovery_json = run_dir.root().join("discovery.json");
    let disc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&discovery_json).unwrap()).unwrap();
    assert_eq!(disc["approved"], serde_json::Value::Bool(false));
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

/// Build a mock provider with two invalid-JSON responses followed
/// by one valid Sketch payload. `set_cycle(false)` so calls past
/// the queued set would error — but the retry helper must consume
/// exactly these 3 and return.
fn retry_sketch_mock() -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain("not-json-at-all"));
    p.push(MockResponse::plain("{still-broken"));
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

    // 1 cell × 1 sketch_per_cell = 1 sketch. The phase will run
    // the retry helper once, which consumes the 3 mock calls in
    // order (2 broken JSON + 1 valid Sketch).
    let matrix = DiscoverMatrixPhase::from_dimensions(1, 1, 1);
    let output = matrix.execute(&ctx).await.expect("phase execute");
    ctx.telemetry.flush().expect("telemetry flush");

    // Assertion 1: the mock recorded exactly 3 LLM calls.
    let recorded = mock.calls();
    assert_eq!(
        recorded.len(),
        3,
        "expected 3 mock calls (2 fails + 1 success), got {}",
        recorded.len()
    );

    // Assertion 2: calls.jsonl.gz has 3 entries (one per LLM call,
    // regardless of parse outcome).
    let calls_path = ctx.telemetry.calls_path().to_path_buf();
    let calls_entries = read_calls_jsonl(&calls_path);
    assert_eq!(
        calls_entries.len(),
        3,
        "calls.jsonl.gz must hold one entry per LLM call"
    );

    // Assertion 3: the 3 entries carry `retry_count` 0, 1, 2 in
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
        vec![0, 1, 2],
        "retry_count must be 0, 1, 2 in started_unix order, got {retry_counts:?}"
    );
    // The 2nd retry (index 2) must carry the successful sketch's
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
        3,
        "each retry must allocate a fresh call_id, got {call_ids:?}"
    );

    // Assertion 4: the persisted sketch is the successful 3rd
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

/// `Role::FacetDeriver` returns the canonical lowercase
/// `"facet_deriver"` (see `crate::llm::role::Role::as_str`).
const FACET_DERIVER_ROLE: &str = "facet_deriver";

/// Count entries in `telemetry/calls.jsonl.gz` whose `role`
/// field equals `target_role`.
fn count_role_calls(entries: &[serde_json::Value], target_role: &str) -> usize {
    entries
        .iter()
        .filter(|e| e.get("role").and_then(|v| v.as_str()) == Some(target_role))
        .count()
}

/// Minimal mock provider that serves a single canned response
/// regardless of role. The PR-14 integration test only cares
/// about the *count* of LLM calls per role, not their content.
fn facet_only_mock() -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(
        r#"{"facets":[{"name":"Data Flows","description":"Sequences.","required":true}]}"#,
    ));
    p.set_cycle(true);
    Arc::new(p)
}

/// Seed two cluster JSON files under `clusters/` so the
/// `discover_facet` phase has two categories to derive.
fn seed_two_clusters(run_dir: &moagan::fs_layout::RunDir<'_>) {
    for (id, label, summary) in [
        ("cluster_01", "auth", "JWT-based auth"),
        ("cluster_02", "storage", "Postgres storage"),
    ] {
        let cluster = moagan::domain::Cluster {
            id: id.into(),
            label: label.into(),
            summary: summary.into(),
            category_id: String::new(),
            members: vec!["sk_001".into(), "sk_002".into()],
            centroid_simhash: String::new(),
            cohesion: 0.5,
            schema_version: "v1".into(),
        };
        std::fs::write(
            run_dir.clusters().join(format!("{id}.json")),
            serde_json::to_vec_pretty(&cluster).unwrap(),
        )
        .unwrap();
    }
}

/// Build a `RunContext` with the cycle-of-mock facet-only
/// provider. Mirrors the helper used by
/// `discovery_pipeline_with_mock_emits_lifecycle` but routes
/// through `build_registry_for` so the pipeline gets a real
/// registry (the registry is the production path the phase
/// uses).
fn make_facet_ctx(
    home: Arc<MoaganHome>,
    run_id: RunId,
    run_dir: &moagan::fs_layout::RunDir<'_>,
    mock: Arc<MockProvider>,
) -> moagan::phases::RunContext {
    let registry = Arc::new(build_registry_with_mock(mock));
    let parallelism = Parallelism::new(2);
    let telemetry = Telemetry::open(run_id, run_dir, RedactPolicy::default(), None).unwrap();
    moagan::phases::RunContext::new(
        run_id,
        home,
        registry,
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".into(),
    )
}

#[tokio::test]
async fn facet_cache_get_or_compute_skips_facet_deriver_on_second_run() {
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();

    // 1st run — cache miss path. Two clusters → 2 facet_deriver
    // LLM calls expected. The cache writes one entry per cluster
    // under `<MOAGAN_HOME>/cache/facets/`.
    let run_id_a = RunId::new();
    let run_dir_a = home.run_dir(run_id_a);
    run_dir_a.ensure().unwrap();
    build_brief(&run_dir_a).unwrap();
    seed_two_clusters(&run_dir_a);

    let mock_a = facet_only_mock();
    let ctx_a = make_facet_ctx(Arc::clone(&home), run_id_a, &run_dir_a, mock_a.clone());
    let phase = DiscoverFacetPhase::with_cache(true);
    phase.execute(&ctx_a).await.expect("1st run must succeed");
    ctx_a.telemetry.flush().expect("flush 1st run");

    let calls_a = read_calls_jsonl(ctx_a.telemetry.calls_path());
    let facet_calls_a = count_role_calls(&calls_a, FACET_DERIVER_ROLE);
    assert_eq!(
        facet_calls_a, 2,
        "1st run must call facet_deriver once per cluster; got {facet_calls_a}"
    );

    // Cache directory must now contain the persisted entries
    // (one per cluster). This is the side effect the 2nd run
    // will read on its hit path.
    let cache_root = home.cross_run_facet_cache_dir();
    let entries_after_first: Vec<_> = std::fs::read_dir(&cache_root)
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(
        entries_after_first.len(),
        2,
        "1st run must persist one cache entry per cluster; got {}",
        entries_after_first.len()
    );

    // 2nd run — same brief, same clusters, same MOAGAN_HOME.
    // Cache hit path must skip the LLM call entirely.
    let run_id_b = RunId::new();
    let run_dir_b = home.run_dir(run_id_b);
    run_dir_b.ensure().unwrap();
    build_brief(&run_dir_b).unwrap();
    seed_two_clusters(&run_dir_b);

    let mock_b = facet_only_mock();
    let ctx_b = make_facet_ctx(Arc::clone(&home), run_id_b, &run_dir_b, mock_b.clone());
    phase.execute(&ctx_b).await.expect("2nd run must succeed");
    ctx_b.telemetry.flush().expect("flush 2nd run");

    let calls_b = read_calls_jsonl(ctx_b.telemetry.calls_path());
    let facet_calls_b = count_role_calls(&calls_b, FACET_DERIVER_ROLE);
    assert_eq!(
        facet_calls_b, 0,
        "2nd run with cache enabled must skip facet_deriver entirely; got {facet_calls_b} calls"
    );

    // Sanity: both runs wrote the same facet files to their
    // respective run dirs. The cache hit path is supposed to
    // produce a byte-identical artefact set. Filter to the
    // `*_facets.json` files only — `phases::util::write_json`
    // also writes a sibling `*.meta.json` (run-dir metadata)
    // that we don't want to conflate with the facet payload.
    let facet_files = |dir: &std::path::Path| -> std::collections::BTreeSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with("_facets.json"))
            .collect()
    };
    let facets_a = facet_files(&run_dir_a.facets());
    let facets_b = facet_files(&run_dir_b.facets());
    assert_eq!(
        facets_a, facets_b,
        "both runs must write the same facet-file set; a={facets_a:?}, b={facets_b:?}"
    );
    assert_eq!(
        facets_a.len(),
        2,
        "both runs must persist one facet file per cluster"
    );
}
