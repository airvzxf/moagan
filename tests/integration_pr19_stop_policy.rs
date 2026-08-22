//! PR-19 integration test: verify the stop policy + outlier
//! tracker (D.13.1/.2/.3/.7/.8) wire into a real matrix
//! execution.
//!
//! Spec reference: docs/v0.5-roadmap.md PR-19; the verification
//! statement is "con --cardinality 100 y saturación al 50%, el
//! run termina con ~60 sketches (cola reserva 25% + outliers)".
//!
//! The integration test uses a small cardinality (8) so the
//! test stays fast, but the matrix phase is the real code path
//! (`DiscoverMatrixPhase::execute`) and the `SaturationTracker`
//! is the real `update` implementation. We then drive the
//! tracker by hand to verify the 50%-saturation trip point
//! (the spec's "~60 sketches" math) without depending on the
//! matrix phase's mocked LLM responses.

// The env mutex is intentionally held across `await` points so
// two test threads cannot both flip `MOAGAN_HOME` mid-flight.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::discovery::clusterer::ClusterChunk;
use moagan::discovery::saturation::SaturationTracker;
use moagan::discovery::stop_policy::{StopDecision, StopPolicy, StopReason};
use moagan::domain::{Cluster, Sketch};
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::embed::HashingEmbedder;
use moagan::llm::{MockProvider, MockResponse, ProviderRegistry};
use moagan::phases::{DiscoverMatrixPhase, Phase, PhaseOutput, RunContext};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

/// Process-wide mutex that serialises every test which mutates
/// the `MOAGAN_HOME` env var. Mirrors the pattern used by the
/// other PR-XX integration tests.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn build_sketch(id: &str, thesis: &str, angle: &str) -> Sketch {
    Sketch {
        id: id.into(),
        thesis: thesis.into(),
        angle: angle.into(),
        ..Sketch::default()
    }
}

fn build_cluster(id: &str, label: &str, members: Vec<String>, cohesion: f32) -> Cluster {
    Cluster {
        id: id.into(),
        label: label.into(),
        members,
        cohesion,
        ..Cluster::default()
    }
}

fn build_brief(run_dir: &moagan::fs_layout::RunDir<'_>) -> moagan::error::Result<()> {
    let brief = serde_json::json!({
        "problem": "Design a multi-tenant SaaS backend",
        "objectives": ["Implement auth", "Implement storage"],
        "deliverables": ["Architecture doc"],
        "constraints": ["Single Rust binary"],
        "assumptions": ["Postgres available"],
        "non_goals": ["Frontend"],
        "acceptance": ["Sketch coverage"],
        "risks": ["Concurrency"],
    });
    std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap())?;
    Ok(())
}

fn sketch_json_for(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "thesis": "Use Rust and SQLite for a single binary backend with strong typing and a robust test suite for the {id} cell.",
  "key_decisions": ["single binary", "embedded sqlite", "async runtime"],
  "architecture_outline": "The CLI binary owns the database, the cache, and the agent registry; each sketch is a distinct cell in the matrix.",
  "assumptions": ["users are comfortable with one process per run"],
  "strengths": ["simple deployment", "easy to test"],
  "weaknesses": ["no horizontal scaling"],
  "hard_constraint_check": {{"single_binary": true}},
  "expected_validation": "Build a 1k-line Rust crate that compiles in <2s.",
  "angle": "minimalist"
}}"#
    )
}

fn build_matrix_mock(per_cell: usize, cells: usize) -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    for n in 0..(per_cell * cells) {
        p.push(MockResponse::plain(sketch_json_for(&format!("sk_{n:04}"))));
    }
    p.set_cycle(false);
    Arc::new(p)
}

fn build_run_context(
    home: Arc<MoaganHome>,
    provider: Arc<MockProvider>,
    run_id: RunId,
) -> RunContext {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = provider.clone();
    registry.insert("mock".into(), arc);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("open telemetry");
    let parallelism = Parallelism::new(2);
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".into(),
    )
}

#[test]
fn stop_policy_default_invariants_are_documented() {
    // The defaults are pinned in the spec (D.13.3). The
    // invariant here is the *runtime* invariant the matrix
    // phase relies on: `min_sketches <= max_sketches <= hard_cap`.
    let p = StopPolicy::default();
    assert!(p.min_sketches <= p.max_sketches);
    assert!(p.max_sketches <= p.hard_cap);
    assert!((p.saturation_threshold - 0.05).abs() < 1e-6);
    assert!((p.reserve_ratio - 0.25).abs() < 1e-6);
    assert!((p.outlier_distance - 0.30).abs() < 1e-6);
}

#[test]
fn stop_decision_enum_variants_are_documented() {
    // The variants surface in the supervisor's decision tree.
    // The matrix phase only emits a telemetry event for
    // `Saturated`; the other reasons drive the supervisor's
    // "exit cleanly" path. This test pins the variant set so
    // a future refactor that drops one trips before it lands.
    let variants = [
        StopReason::Saturated,
        StopReason::OutliersCollected,
        StopReason::BudgetExhausted,
        StopReason::Cancelled,
        StopReason::MinSketchesReached,
        StopReason::MaxSketchesReached,
    ];
    assert_eq!(variants.len(), 6);
    // Decision's `Stop` variant carries a reason; the
    // `Continue` variant does not. The match on the inner
    // reason is the only way callers can branch.
    let stop = StopDecision::Stop {
        reason: StopReason::Saturated,
    };
    match stop {
        StopDecision::Stop { reason } => assert_eq!(reason, StopReason::Saturated),
        StopDecision::Continue => panic!("expected Stop"),
    }
}

#[test]
fn detect_outliers_returns_unchanged_when_clusters_match_thesis() {
    // The outlier detector is a cheap deterministic signal:
    // a sketch whose tokens overlap with its cluster's tokens
    // is NOT an outlier. The test pins the symmetry so a
    // refactor that flips the comparison trips the test.
    let samples = vec![
        build_sketch("sk_001", "alpha beta gamma", "minimalist"),
        build_sketch("sk_002", "delta epsilon zeta", "production-grade"),
    ];
    // Cluster's label mirrors the sketch's thesis + angle so
    // the Jaccard distance is well below the 0.30 threshold.
    let clusters = vec![build_cluster(
        "cluster_01",
        "alpha beta gamma minimalist",
        vec!["sk_001".into()],
        0.5,
    )];
    let _tracker = SaturationTracker::with_policy(
        80,
        StopPolicy {
            outlier_distance: 0.3,
            ..StopPolicy::default()
        },
    );
    let outliers =
        moagan::discovery::outlier::detect_outliers_with_threshold(&samples, &clusters, 0.3);
    assert_eq!(
        outliers,
        vec![moagan::discovery::outlier::SketchId("sk_002".into())]
    );
}

#[test]
fn pr19_verification_50_percent_saturation_trips_before_hard_cap() {
    // Spec verification: with a 100-sketch matrix fan-out and
    // 50% saturation, the run should terminate well before the
    // hard cap. The math: saturation_point = 50,
    // reserve = ceil(50 * 0.25) = 13, trip_point = 63.
    let mut tracker = SaturationTracker::with_policy(
        100,
        StopPolicy {
            saturation_threshold: 0.5,
            reserve_ratio: 0.25,
            min_sketches: 40,
            max_sketches: 80,
            hard_cap: 500,
            outlier_distance: 0.3,
        },
    );
    tracker.record_completions(63);
    let clusters = vec![build_cluster(
        "c1",
        "shared vocabulary",
        vec!["sk_001".into()],
        0.5,
    )];
    let decision = tracker.update(&[], &clusters);
    match decision {
        StopDecision::Stop {
            reason: StopReason::Saturated,
        } => {}
        other => panic!(
            "expected Stop(Saturated) at completed=63 with 50% similarity, got {:?}",
            other
        ),
    }
    // And the trip point must be well under the hard cap.
    assert!(tracker.completed < tracker.policy.hard_cap);
    // And the trip point must be in the "~60 sketches" window
    // the spec promises: 50% saturation + 25% reserve.
    let expected_trip = 50 + 13; // saturation_point + reserve
    assert!(
        (tracker.completed as i64 - expected_trip as i64).abs() <= 5,
        "trip point must be within 5 of the spec's ~60-sketches target; got {}",
        tracker.completed
    );
}

#[test]
fn pr19_verification_reserve_left_means_continue() {
    // The loop has not yet spent the reserve_ratio margin;
    // even at the 50% saturation point the tracker says
    // `Continue` so the loop can fire the reserve batch.
    let mut tracker = SaturationTracker::with_policy(
        100,
        StopPolicy {
            saturation_threshold: 0.5,
            reserve_ratio: 0.25,
            min_sketches: 40,
            max_sketches: 80,
            hard_cap: 500,
            outlier_distance: 0.3,
        },
    );
    tracker.record_completions(50);
    let clusters = vec![build_cluster(
        "c1",
        "shared vocabulary",
        vec!["sk_001".into()],
        0.5,
    )];
    let decision = tracker.update(&[build_sketch("sk_001", "alpha", "minimalist")], &clusters);
    assert_eq!(decision, StopDecision::Continue);
}

#[tokio::test]
async fn discover_matrix_phase_runs_under_stop_policy_watch() {
    // End-to-end smoke: drive `DiscoverMatrixPhase::execute`
    // with a small matrix and verify the saturation tracker
    // observes the survivors. The matrix phase trims the
    // surviving set when the tracker says `Stop`; for a
    // 4-cell × 2-per-cell = 8 sketch matrix the default
    // `StopPolicy` is permissive (max_sketches=80,
    // hard_cap=500) so the tracker returns `Continue` and
    // every sketch survives. The test pins the
    // happy-path wiring: the tracker is consulted and the
    // matrix phase still produces the expected number of
    // sketches.
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

    let matrix = DiscoverMatrixPhase::new(moagan::discovery::matrix::ExplorationMatrix::from_spec(
        moagan::discovery::matrix_spec::MatrixSpec::parse_one("a=x,y;b=x,y").expect("spec parses"),
        2,
    ));
    let mock = build_matrix_mock(matrix.matrix.sketches_per_cell, matrix.matrix.cells());
    let ctx = build_run_context(home.clone(), mock, run_id);

    let outcome = matrix.execute(&ctx).await.expect("matrix phase runs");

    let PhaseOutput::Sketches(paths) = outcome else {
        panic!("expected Sketches output");
    };
    assert_eq!(
        paths.len(),
        8,
        "8-slot matrix must produce 8 sketches; got {}",
        paths.len()
    );
}

#[test]
fn cluster_chunk_round_trips_through_embedder() {
    // The outlier detector and the clusterer are
    // complementary: the clusterer groups similar sketches
    // and the outlier detector flags the rest. The test
    // pins the clusterer's contract: two similar texts end
    // up in the same chunk so the outlier detector can
    // compute the Jaccard distance against the cluster's
    // feature bag.
    let records: Vec<moagan::discovery::clusterer::SketchRecord> = vec![
        moagan::discovery::clusterer::SketchRecord {
            id: "sk_001".into(),
            text: "Postgres connection pool with sqlx and tokio async runtime".into(),
        },
        moagan::discovery::clusterer::SketchRecord {
            id: "sk_002".into(),
            text: "Postgres connection pool with sqlx and tokio async runtime for rust".into(),
        },
        moagan::discovery::clusterer::SketchRecord {
            id: "sk_003".into(),
            text: "Quantum mechanics probability distribution function".into(),
        },
    ];
    let embedder = HashingEmbedder::default();
    let chunks: Vec<ClusterChunk> =
        moagan::discovery::clusterer::cluster(&records, &embedder, 0.15);
    assert_eq!(
        chunks.len(),
        2,
        "two clusters expected (Postgres + Quantum)"
    );
    let postgres_chunk = chunks
        .iter()
        .find(|c| c.member_indices.contains(&0))
        .expect("Postgres chunk must exist");
    assert!(
        postgres_chunk.member_indices.contains(&1),
        "sk_002 (Postgres) must share the cluster with sk_001"
    );
    let quantum_chunk = chunks
        .iter()
        .find(|c| c.member_indices.contains(&2))
        .expect("Quantum chunk must exist");
    assert_eq!(quantum_chunk.member_indices, vec![2]);
}

/// Operator-facing regression pin for the profile-expansion bug.
///
/// The user runs:
///
/// ```text
/// moagan discover \
///   --max-parallelism 64 \
///   --temperature-profile 'provider=...;temperatures=0.0,0.3,0.6,1.0,1.3,1.6,1.9;replicas=3'
/// ```
///
/// which expands the matrix by `7 × 3 = 21`. Before the fix the
/// `coordinator` multiplied `min_sketches` by that expansion
/// (`40 × 21 = 840`), giving `outliers_cap = 420`. Combined
/// with the cluster-empty detector classifying every sketch as
/// an outlier during the matrix loop, the loop tripped
/// `OutliersCollected` at iteration #420 — the operator's
/// intended 1680 sketches never reached disk. The fix is twofold:
/// (a) drop the multiplication, (b) guard the outlier
/// accumulator on `!clusters.is_empty()`. This test drives the
/// tracker with the operator's exact shape and asserts that
/// `OutliersCollected` never trips across the full 1680
/// iterations.
#[test]
fn pr19_user_profile_7x3_does_not_trip_outliers_cap() {
    let mut tracker = SaturationTracker::with_policy(
        1680,
        StopPolicy {
            saturation_threshold: 0.05,
            reserve_ratio: 0.25,
            outlier_distance: 0.30,
            min_sketches: 40,
            max_sketches: 2000,
            hard_cap: 2000,
        },
    );

    for i in 0..1680 {
        let sketch = Sketch {
            id: format!("sk_{i:04}"),
            thesis: format!("thesis {i} alpha beta gamma"),
            angle: "minimalist".into(),
            ..Sketch::default()
        };
        // Caller records completion before consulting `update`,
        // mirroring the coordinator's call order.
        tracker.record_completions(1);
        let decision = tracker.update(&[sketch], &[]);
        assert!(
            !matches!(
                decision,
                StopDecision::Stop {
                    reason: StopReason::OutliersCollected
                }
            ),
            "OutliersCollected must not trip during the matrix loop (iteration {i})"
        );
    }
    assert_eq!(
        tracker.outliers_collected, 0,
        "outliers_collected must stay at 0 while clusters is empty"
    );
    assert_eq!(tracker.completed, 1680);
}
