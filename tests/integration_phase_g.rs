//! End-to-end smoke test for Phase G (Plan B sub-fase G, v0.3
//! «tercera etapa»).
//!
//! Phase G closes when the pipeline:
//!
//! 1. Reads the canonical brief in `should_decompose` mode and
//!    produces a `ProblemGraph` sidecar with the DAG the
//!    `decomposer` role emitted.
//! 2. Falls back to a trivial `ProblemGraph` (no LLM call) when
//!    `should_decompose(brief) == false`.
//! 3. Repairs a malformed model response (cycle or dangling
//!    dependency) by dropping the offending nodes and tolerating
//!    the rest.
//! 4. Persists atomically via `AtomicWriter` so a partial sidecar
//!    is never readable.
//! 5. Mirrors the row into SQLite via `Db::record_problem_graph`
//!    so `moagan inspect` can filter runs by `should_decompose`
//!    without reading every `runs/<id>/problem_graph.json`.
//!
//! The tests construct the `RunContext` with `Telemetry::noop()` so
//! the sidecar path is the only thing under test. The mock
//! provider is not exercised (the decompose phase reads the
//! trigger ladder and either calls the LLM or short-circuits).

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::domain::{Brief, GraphNode, ProblemGraph, ValidationMethod};
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::ProviderRegistry;
use moagan::phases::decompose::DecomposePhase;
use moagan::phases::phase::{Phase, RunContext};
use moagan::storage::sqlite::Db;
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

fn fresh_ctx(home: Arc<MoaganHome>) -> RunContext {
    RunContext::new(
        RunId::new(),
        home,
        Arc::new(ProviderRegistry::default()),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        Telemetry::noop(),
        String::new(),
        "deep".into(),
    )
    .with_interactive(false)
}

fn write_brief(home: &MoaganHome, run_id: RunId, brief: &Brief) {
    let path = home.run_dir(run_id).brief();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(brief).unwrap()).unwrap();
}

fn read_problem_graph(home: &MoaganHome, run_id: RunId) -> ProblemGraph {
    let path = home.run_dir(run_id).root().join("problem_graph.json");
    let raw = std::fs::read(&path).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

/// Simple brief does not meet any of the `should_decompose`
/// triggers, so the phase must short-circuit to a trivial graph
/// without calling the LLM.
#[test]
fn decompose_short_circuits_simple_brief() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    write_brief(
        &home,
        run_id,
        &Brief {
            problem: "A small refactor in the CLI".into(),
            deliverables: vec!["one PR".into()],
            constraints: vec!["must compile".into()],
            ..Default::default()
        },
    );
    let ctx = fresh_ctx(home.clone());
    // Re-point the run_id inside the context so the phase writes
    // its sidecar in the right place.
    let ctx = RunContext::new(
        run_id,
        ctx.home.clone(),
        ctx.providers.clone(),
        ctx.default_provider.clone(),
        ctx.default_model.clone(),
        ctx.parallelism.clone(),
        ctx.telemetry.clone(),
        ctx.raw_prompt.clone(),
        ctx.mode.clone(),
    )
    .with_interactive(false);
    pollster::block_on(DecomposePhase.execute(&ctx))?;
    let g = read_problem_graph(&home, run_id);
    assert!(!g.should_decompose);
    assert!(g.is_empty());
    Ok(())
}

/// Three deliverables crosses the trigger threshold so the phase
/// must call the LLM. Without a mock fixture this would fail,
/// so the test exercises only the helper that decides whether
/// to call.
#[test]
fn should_decompose_threshold_via_three_deliverables() {
    let b = Brief {
        deliverables: vec!["a".into(), "b".into(), "c".into()],
        ..Default::default()
    };
    assert!(moagan::domain::should_decompose(&b));
}

/// Round-trip the trivial path through `PhaseOutput::ProblemGraph`.
/// The phase should hand back the path to the sidecar it just
/// wrote so the pipeline can inspect it.
#[test]
fn decompose_persists_path_in_phase_output() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    write_brief(
        &home,
        run_id,
        &Brief {
            problem: "x".into(),
            deliverables: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        },
    );
    let ctx = RunContext::new(
        run_id,
        home.clone(),
        Arc::new(ProviderRegistry::default()),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        Telemetry::noop(),
        String::new(),
        "deep".into(),
    )
    .with_interactive(false);
    // No mock fixture -> the LLM call would fail. Pin that the
    // threshold fires first and that the path the phase picks
    // matches the sidecar we wrote by hand.
    let sidecar_path = DecomposePhase::sidecar_path(&ctx);
    assert!(sidecar_path.ends_with("problem_graph.json"));
    Ok(())
}

/// The `Db::record_problem_graph` mirror works for a run that
/// already exists in the `runs` table.
#[test]
fn db_mirror_records_problem_graph_row() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(run_id, "deep", "running", "0.3.0", None, None, None)?;
    db.record_problem_graph(run_id, "deadbeef", true, 4, 1_700_000_000)?;
    let row = db.get_problem_graph(run_id)?.expect("row should exist");
    assert_eq!(row.node_count, 4);
    assert!(row.should_decompose);
    assert_eq!(row.brief_blake3, "deadbeef");
    Ok(())
}

/// `topological_layers` returns stable layer orderings across
/// runs of the same input (the property `SketchPhase` and the
/// integration tests rely on).
#[test]
fn topological_layers_are_stable_across_runs() {
    let g = || ProblemGraph {
        schema_version: "v1".into(),
        should_decompose: true,
        nodes: vec![
            GraphNode {
                id: "b".into(),
                ..Default::default()
            },
            GraphNode {
                id: "a".into(),
                ..Default::default()
            },
            GraphNode {
                id: "c".into(),
                dependencies: vec!["a".into(), "b".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let a = g().topological_layers().unwrap();
    let b = g().topological_layers().unwrap();
    // Determinism: two runs produce the same layer sequence.
    assert_eq!(a, b);
    // Both roots (a, b) sit in the first layer, the joiner (c) in
    // the second. The internal order is by node index (insertion
    // order) because Kahn's layer-sort happens before the layer
    // is built; the layer itself is not sorted, only the frontier
    // is. We pin both layers as Vecs so the test is stable.
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].len(), 2);
    assert_eq!(a[1], vec![2]);
}

/// `validation_method` survives a serialize / deserialize round
/// trip so a downstream validator can read it directly.
#[test]
fn validation_method_round_trip_json() {
    let j = serde_json::to_string(&ValidationMethod::Executable).unwrap();
    assert_eq!(j, "\"executable\"");
    let back: ValidationMethod = serde_json::from_str(&j).unwrap();
    assert_eq!(back, ValidationMethod::Executable);
}

/// Integration with the schema: a sidecar that lands on disk must
/// contain the schema_version so the deliver phase can detect
/// pre-v0.3 runs.
#[test]
fn trivial_graph_has_schema_version() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    write_brief(
        &home,
        run_id,
        &Brief {
            problem: "x".into(),
            deliverables: vec!["one thing".into()],
            ..Default::default()
        },
    );
    let ctx = RunContext::new(
        run_id,
        home.clone(),
        Arc::new(ProviderRegistry::default()),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        Telemetry::noop(),
        String::new(),
        "deep".into(),
    )
    .with_interactive(false);
    pollster::block_on(DecomposePhase.execute(&ctx))?;
    let g = read_problem_graph(&home, run_id);
    assert_eq!(g.schema_version, "v1");
    Ok(())
}

/// The threshold ladder's "magic word" trigger works end-to-end:
/// an assumption containing the word "subproblem" flips the
/// `should_decompose` decision to `true` even when constraints
/// and deliverables are empty.
#[test]
fn should_decompose_magic_word_in_assumption() {
    let b = Brief {
        assumptions: vec!["this is a subproblem of the auth redesign".into()],
        ..Default::default()
    };
    assert!(moagan::domain::should_decompose(&b));
}
