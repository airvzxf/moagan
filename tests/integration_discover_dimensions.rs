//! F1 (Track G.2 `discover_dimensions`): integration tests for the
//! new LLM-derived matrix dimensions path.
//!
//! The tests below pin three behaviours the F1 contract depends on:
//!
//! 1. `DiscoverDimensionsPhase` calls the LLM with
//!    `Role::DimensionDeriver`, parses the response into
//!    `DerivedDimensions`, and persists the result to
//!    `<run_dir>/discovery_dimensions.json`.
//! 2. The matrix phase picks up the sidecar verbatim when no
//!    `--matrix-spec` is supplied (the LLM-derive path), so the
//!    final `cells()` count matches the dimensions the LLM
//!    produced — not the legacy 4×2 default.
//! 3. When the operator passes `--matrix-spec`, the
//!    `discover_dimensions` phase is skipped entirely; the
//!    matrix uses the spec verbatim and `cells()` matches the
//!    sum of per-dimension facet counts.
//!
//! Tests use the mock provider so they do not require an API
//! key. The LLM response is hand-crafted JSON with asymmetric
//! facet counts (3 dimensions with 1, 2, and 3 facets
//! respectively) so a regression that silently re-applies the
//! legacy 4×2 default would be caught by the `cells() == 6`
//! assertion.

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::config::Config;
use moagan::discovery::matrix::{
    DISCOVERY_DIMENSIONS_FILENAME, DISCOVERY_DIMENSIONS_SCHEMA_VERSION, DiscoveryDimensions,
};
use moagan::discovery::matrix_spec::DerivedDimensions;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::MockProvider;
use moagan::llm::{MockResponse, ProviderRegistry};
use moagan::phases::{
    DiscoverDimensionsPhase, DiscoverMatrixPhase, Phase, PhaseOutput, RunContext,
};
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Hand-crafted LLM response the `Role::DimensionDeriver` call
/// returns. Three dimensions with asymmetric facet counts (1, 2,
/// 3) — a deliberate check that the matrix sums per-dimension
/// facet counts instead of multiplying `dims × facets`.
const DIMENSIONS_JSON: &str = r#"{
  "dimensions": [
    {
      "id": "auth",
      "label": "Auth strategy",
      "facets": [
        { "id": "oauth", "label": "OAuth", "description": "OAuth 2 flow" }
      ]
    },
    {
      "id": "storage",
      "label": "Storage strategy",
      "facets": [
        { "id": "sql", "label": "SQL", "description": "Relational backend" },
        { "id": "kv", "label": "Embedded KV", "description": "Key-value cache" }
      ]
    },
    {
      "id": "scaling",
      "label": "Scaling strategy",
      "facets": [
        { "id": "vertical", "label": "Vertical", "description": "Scale up" },
        { "id": "horizontal", "label": "Horizontal", "description": "Scale out" },
        { "id": "auto", "label": "Auto", "description": "Auto-scale" }
      ]
    }
  ]
}"#;

/// Brief the upstream `intake` + `clarify` phases normally write
/// to `<run_dir>/brief.json`. The `discover_dimensions` phase
/// reads this file to build the LLM user payload.
fn seed_brief(run_dir: &moagan::fs_layout::RunDir<'_>) {
    let brief = serde_json::json!({
        "problem": "Design a multi-tenant SaaS backend",
        "objectives": ["Auth", "Storage", "Scaling"],
        "deliverables": ["Architecture doc"],
        "constraints": ["Single Rust binary"],
        "assumptions": ["Operators can deploy"],
        "non_goals": ["Frontend"],
        "acceptance": ["End-to-end smoke"],
        "risks": ["Concurrency"],
    });
    std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap()).unwrap();
}

/// Build a [`RunContext`] wired to the supplied mock provider.
/// Mirrors the helper in `tests/integration_discovery.rs` but
/// scoped to the F1 dimension-derive path.
fn build_ctx(home: Arc<MoaganHome>, run_id: RunId, mock: Arc<MockProvider>) -> Arc<RunContext> {
    let _run_dir = home.run_dir(run_id);
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

#[tokio::test]
async fn discover_dimensions_phase_writes_sidecar_and_exposes_path() -> Result<()> {
    let _g = env_lock();
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    seed_brief(&run_dir);

    // Mock returns exactly one response: the dimensions JSON.
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(DIMENSIONS_JSON));
    let mock = Arc::new(p);

    let ctx = build_ctx(home.clone(), run_id, mock);
    let phase = DiscoverDimensionsPhase;
    let out = phase.execute(&ctx).await?;

    let path = match out {
        PhaseOutput::DiscoveryDimensions(p) => p,
        other => panic!("expected DiscoveryDimensions output, got {other:?}"),
    };
    assert!(path.exists(), "sidecar must exist after execute");

    // Sidecar is JSON; decode and assert the LLM-derived list
    // matches the mock response verbatim.
    let raw = std::fs::read_to_string(&path).unwrap();
    let sidecar: DiscoveryDimensions = serde_json::from_str(&raw).unwrap();
    assert_eq!(sidecar.schema_version, DISCOVERY_DIMENSIONS_SCHEMA_VERSION);
    assert_eq!(sidecar.brief_hash.len(), 64);
    assert_eq!(sidecar.dimensions.len(), 3);
    assert_eq!(sidecar.dimensions[0].id, "auth");
    assert_eq!(sidecar.dimensions[0].facets.len(), 1);
    assert_eq!(sidecar.dimensions[1].id, "storage");
    assert_eq!(sidecar.dimensions[1].facets.len(), 2);
    assert_eq!(sidecar.dimensions[2].id, "scaling");
    assert_eq!(sidecar.dimensions[2].facets.len(), 3);
    // The descriptions list captures the LLM's per-facet text so
    // the integrator phase can surface it without re-deriving.
    assert_eq!(
        sidecar.description_for("auth", "oauth"),
        Some("OAuth 2 flow")
    );
    Ok(())
}

#[tokio::test]
async fn discover_dimensions_phase_skips_llm_when_sidecar_present() -> Result<()> {
    let _g = env_lock();
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    seed_brief(&run_dir);

    // Pre-populate the sidecar with a known shape. The mock
    // provider is never called because the phase short-circuits
    // on sidecar presence.
    let sidecar = DiscoveryDimensions {
        schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.to_string(),
        brief_hash: "deadbeef".repeat(8),
        dimensions: vec![moagan::discovery::matrix::Dimension {
            id: "auth".into(),
            label: "Auth".into(),
            facets: vec![moagan::discovery::matrix::Facet {
                id: "oauth".into(),
                label: "OAuth".into(),
            }],
        }],
        descriptions: Default::default(),
        created_unix: 0,
    };
    let path = run_dir.root().join(DISCOVERY_DIMENSIONS_FILENAME);
    std::fs::write(&path, serde_json::to_vec(&sidecar).unwrap()).unwrap();

    let mut p = MockProvider::empty();
    // Cycle on the dimensions JSON so any accidental LLM call
    // would emit the wrong shape and surface in the assertions.
    p.push(MockResponse::plain(DIMENSIONS_JSON));
    p.set_cycle(true);
    let mock = Arc::new(p);

    let ctx = build_ctx(home.clone(), run_id, mock);
    let phase = DiscoverDimensionsPhase;
    let out = phase.execute(&ctx).await?;
    let returned = match out {
        PhaseOutput::DiscoveryDimensions(p) => p,
        other => panic!("expected DiscoveryDimensions output, got {other:?}"),
    };
    assert_eq!(returned, path);
    Ok(())
}

#[tokio::test]
async fn matrix_phase_picks_up_llm_derived_dimensions() -> Result<()> {
    let _g = env_lock();
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    seed_brief(&run_dir);

    // Pre-populate the sidecar with the LLM-derived 3-dim layout
    // (asymmetric facet counts: 1 + 2 + 3 = 6 cells).
    let sidecar = DiscoveryDimensions {
        schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.to_string(),
        brief_hash: "deadbeef".repeat(8),
        dimensions: vec![
            moagan::discovery::matrix::Dimension {
                id: "auth".into(),
                label: "Auth".into(),
                facets: vec![moagan::discovery::matrix::Facet {
                    id: "oauth".into(),
                    label: "OAuth".into(),
                }],
            },
            moagan::discovery::matrix::Dimension {
                id: "storage".into(),
                label: "Storage".into(),
                facets: vec![
                    moagan::discovery::matrix::Facet {
                        id: "sql".into(),
                        label: "SQL".into(),
                    },
                    moagan::discovery::matrix::Facet {
                        id: "kv".into(),
                        label: "KV".into(),
                    },
                ],
            },
            moagan::discovery::matrix::Dimension {
                id: "scaling".into(),
                label: "Scaling".into(),
                facets: vec![
                    moagan::discovery::matrix::Facet {
                        id: "vertical".into(),
                        label: "Vertical".into(),
                    },
                    moagan::discovery::matrix::Facet {
                        id: "horizontal".into(),
                        label: "Horizontal".into(),
                    },
                    moagan::discovery::matrix::Facet {
                        id: "auto".into(),
                        label: "Auto".into(),
                    },
                ],
            },
        ],
        descriptions: Default::default(),
        created_unix: 0,
    };
    let path = run_dir.root().join(DISCOVERY_DIMENSIONS_FILENAME);
    std::fs::write(&path, serde_json::to_vec(&sidecar).unwrap()).unwrap();

    let mock = Arc::new(MockProvider::empty());
    let ctx = build_ctx(home.clone(), run_id, mock);

    let matrix_phase =
        DiscoverMatrixPhase::resolved(&ctx, 10).expect("matrix resolves from sidecar");
    // The matrix must reflect the LLM-derived dimensions, not
    // the legacy 4×2 default.
    assert_eq!(matrix_phase.matrix.cells(), 6);
    assert_eq!(matrix_phase.matrix.dimensions.len(), 3);
    assert_eq!(matrix_phase.matrix.dimensions[0].facets.len(), 1);
    assert_eq!(matrix_phase.matrix.dimensions[2].facets.len(), 3);
    assert_eq!(matrix_phase.matrix.sketches_per_cell, 10);
    Ok(())
}

#[test]
fn derived_dimensions_round_trips_through_json_for_sidecar() {
    let derived = DerivedDimensions {
        dimensions: vec![moagan::discovery::matrix_spec::DimensionSpec {
            id: "auth".into(),
            label: "Auth".into(),
            facets: vec![moagan::discovery::matrix_spec::FacetSpec {
                id: "oauth".into(),
                label: "OAuth".into(),
                description: "OAuth 2 flow".into(),
            }],
        }],
    };
    let json = serde_json::to_string(&derived).unwrap();
    let back: DerivedDimensions = serde_json::from_str(&json).unwrap();
    assert_eq!(back, derived);
}
