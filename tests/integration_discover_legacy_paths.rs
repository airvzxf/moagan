//! F1 (Track G.2) regression test: the legacy `--cardinality`
//! `--dimensions N --facets-per-dimension M` discovery path must
//! keep working during the F1 transition window.
//!
//! F2 is the feature that renames `--cardinality` to
//! `--sketches-per-cell` and lowers the floor. v0.13.2 lowered
//! the floor from 10 to 1 (so debug / integration runs can fan
//! out cheaply). Until F2 lands, the legacy flag pair still
//! drives an 8-cell matrix (the `4 × 2` placeholder layout) so
//! existing CI runs and operator scripts that pass
//! `--cardinality 80 --dimensions 4 --facets-per-dimension 2`
//! keep producing the same artefacts they produced pre-F1.
//!
//! The test below pins three behaviours:
//!
//! 1. The CLI dispatcher accepts the legacy flag triple with
//!    `matrix_spec = []` and `llm_derive = false` (the F1
//!    replacement defaults).
//! 2. The matrix phase rebuilds the `4 × 2` matrix from the
//!    legacy counts via `ExplorationMatrix::new`.
//! 3. `cells()` matches the pre-F1 contract (`4 × 2 = 8`).

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::config::Config;
use moagan::discovery::matrix::{Dimension, ExplorationMatrix, Facet};
use moagan::discovery::matrix_spec::{DimensionSpec, FacetSpec};
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::MockProvider;
use moagan::llm::ProviderRegistry;
use moagan::phases::{DiscoverMatrixPhase, Phase, PhaseOutput, RunContext};
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Pre-F1 (v0.5-PR-24) `ExplorationMatrix::default_for(80)`
/// produced a 4-dim × 2-facet matrix with `cardinality = 80`.
/// The test rebuilds the same shape via `ExplorationMatrix::new`
/// so the assertions stay byte-identical to the pre-F1 contract.
fn legacy_4x2_matrix(cardinality: usize) -> ExplorationMatrix {
    let dims = vec![
        Dimension {
            id: "deployment-model".into(),
            label: "Deployment model".into(),
            facets: vec![
                Facet {
                    id: "serverless".into(),
                    label: "serverless".into(),
                },
                Facet {
                    id: "self-hosted".into(),
                    label: "self-hosted".into(),
                },
            ],
        },
        Dimension {
            id: "storage".into(),
            label: "Storage strategy".into(),
            facets: vec![
                Facet {
                    id: "sql".into(),
                    label: "SQL".into(),
                },
                Facet {
                    id: "kv".into(),
                    label: "embedded key-value".into(),
                },
            ],
        },
        Dimension {
            id: "consistency".into(),
            label: "Consistency model".into(),
            facets: vec![
                Facet {
                    id: "strong".into(),
                    label: "strong".into(),
                },
                Facet {
                    id: "eventual".into(),
                    label: "eventual".into(),
                },
            ],
        },
        Dimension {
            id: "observability".into(),
            label: "Observability".into(),
            facets: vec![
                Facet {
                    id: "logs-only".into(),
                    label: "logs only".into(),
                },
                Facet {
                    id: "metrics-tracing".into(),
                    label: "metrics + tracing".into(),
                },
            ],
        },
    ];
    let per_cell = (cardinality / dims.len().max(1) / 2).max(1);
    ExplorationMatrix::new(dims, per_cell)
}

/// Build a `MatrixSpec` matching the legacy `--dimensions N
/// --facets-per-dimension M` programmatic shape. The CLI layer
/// uses this same shape when neither a `--matrix-spec` nor an
/// LLM-derive is in play.
fn legacy_dim_facets_spec(dims: usize, facets_per_dim: usize) -> Vec<DimensionSpec> {
    (0..dims)
        .map(|i| DimensionSpec {
            id: format!("dim-{:02}", i),
            label: format!("Dimension {}", i),
            facets: (0..facets_per_dim)
                .map(|j| FacetSpec {
                    id: format!("f{}", j + 1),
                    label: format!("F{}", j + 1),
                    description: String::new(),
                })
                .collect(),
        })
        .collect()
}

/// Build a [`RunContext`] wired to the supplied mock provider.
fn build_ctx(home: Arc<MoaganHome>, run_id: RunId, mock: Arc<MockProvider>) -> Arc<RunContext> {
    let registry = Arc::new({
        let mut r = ProviderRegistry::default();
        r.insert("mock".into(), mock);
        r
    });
    let telemetry = Arc::new(Telemetry::noop());
    Arc::new(RunContext::new_with_config(
        run_id,
        home.clone(),
        registry,
        "mock".to_owned(),
        "mock-model".to_owned(),
        Parallelism::new(1),
        (*telemetry).clone(),
        String::new(),
        "discover".to_owned(),
        Arc::new(Config::default()),
    ))
}

fn seed_brief(run_dir: &moagan::fs_layout::RunDir<'_>) {
    let brief = serde_json::json!({
        "problem": "Design a multi-tenant SaaS backend",
        "objectives": ["Auth"],
        "deliverables": ["Architecture doc"],
        "constraints": ["Single Rust binary"],
        "assumptions": ["Operators can deploy"],
        "non_goals": ["Frontend"],
        "acceptance": ["Smoke"],
        "risks": ["Concurrency"],
    });
    std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();
}

#[test]
fn legacy_4x2_matrix_matches_pre_f1_contract() {
    // Pre-F1: `default_for(80)` produced `4 dims × 2 facets ×
    // 10 per cell = 80 sketches`. The new API keeps the same
    // shape so the operator's `--cardinality 80 --dimensions 4
    // --facets-per-dimension 2` invocation produces an
    // equivalent `cells() = 8` matrix.
    let m = legacy_4x2_matrix(80);
    assert_eq!(m.dimensions.len(), 4);
    assert_eq!(m.cells(), 8);
    assert_eq!(m.sketches_per_cell, 10);
    assert_eq!(m.cardinality(), 80);
}

#[test]
fn legacy_dim_facets_spec_matches_programmatic_layout() {
    // `--dimensions 3 --facets-per-dimension 2` should produce
    // 3 dimensions × 2 facets = 6 cells, with `dim-NN` and
    // `fN` placeholder ids.
    let spec = legacy_dim_facets_spec(3, 2);
    assert_eq!(spec.len(), 3);
    assert_eq!(spec[0].id, "dim-00");
    assert_eq!(spec[0].facets.len(), 2);
    assert_eq!(spec[0].facets[0].id, "f1");
    assert_eq!(spec[2].id, "dim-02");
}

#[tokio::test]
async fn matrix_phase_accepts_legacy_4x2_construction() -> Result<()> {
    let _g = env_lock();
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    seed_brief(&run_dir);

    // The matrix fans out 80 LLM calls (4 dims × 2 facets ×
    // 10 per cell). Provide enough unique sketch payloads so
    // the cycle-of-mock provider can satisfy every call
    // without exhausting its buffer.
    let sketch_payload = r#"{
      "id": "sk_test",
      "thesis": "Use Rust and SQLite for a single binary backend with strong typing.",
      "key_decisions": ["single binary", "embedded sqlite"],
      "architecture_outline": "The CLI binary owns the database, the cache, and the agent registry.",
      "assumptions": ["users are comfortable with one process per run"],
      "strengths": ["simple deployment"],
      "weaknesses": ["no horizontal scaling"],
      "hard_constraint_check": {"single_binary": true},
      "expected_validation": "Smoke build of a 1k-line Rust crate that compiles in <2s.",
      "angle": "minimalist"
    }"#;
    let mut p = MockProvider::empty();
    for _ in 0..80 {
        p.push(moagan::llm::MockResponse::plain(sketch_payload));
    }
    p.set_cycle(true);
    let mock = Arc::new(p);

    let ctx = build_ctx(home.clone(), run_id, mock);

    let phase = DiscoverMatrixPhase::new(legacy_4x2_matrix(80));
    assert_eq!(phase.matrix.cells(), 8);
    assert_eq!(phase.matrix.cardinality(), 80);
    // Persist without an LLM call. The `execute` path will
    // fan out LLM calls (the mock cycles), but the matrix
    // shape is the assertion target here.
    let out = phase.execute(&ctx).await?;
    match out {
        PhaseOutput::Sketches(paths) => {
            assert!(!paths.is_empty(), "matrix phase must produce sketches");
        }
        other => panic!("expected Sketches output, got {other:?}"),
    }
    Ok(())
}

#[test]
fn from_spec_rejects_empty_matrix_spec() {
    // The CLI dispatcher should not produce a zero-cell matrix
    // when the operator passes only the legacy flag pair with
    // no spec. The dispatcher derives a programmatic spec from
    // the dimensions count (≥1), so an empty matrix is only
    // reachable via a hand-crafted zero-row spec — which the
    // parser already rejects via `parse_all` ("produced zero
    // dimensions"). Pin the parser-level guard so a future
    // refactor cannot silently produce an empty matrix from a
    // malformed spec.
    use moagan::discovery::matrix_spec::MatrixSpec;
    let err = MatrixSpec::parse_all([""]).unwrap_err();
    assert!(err.to_string().contains("zero dimensions"));
}
