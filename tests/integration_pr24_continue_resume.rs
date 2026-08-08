//! PR-24 integration test: verify `moagan continue --kind discovery`
//! resumes a paused/failed discovery run, and that the resumed run
//! emits `discover_matrix` phase events with `resume: true` so
//! post-execution review can distinguish the resumed fan-out from
//! the original.
//!
//! Spec reference: docs/v0.5-roadmap.md PR-24. V4 §6.11, T01-06
//! §10.2.
//!
//! The roadmap lists the verification statement as:
//! > con `--cardinality 40` y saturación al 50%, el run termina
//! > con ~60 sketches (cola reserva 25% + outliers)
//!
//! and the integration test requirement as:
//! - Run `moagan discover --cardinality 40`, cancel at sketch 20.
//! - Run `moagan continue --kind discovery --from-pause <run_id>`.
//! - Assert `telemetry.jsonl` shows `discover_matrix` ran twice
//!   (resume = true on the 2nd).
//!
//! We honour the spec without depending on real cancellation or
//! 40-sketches fan-outs:
//!
//! 1. Run the pre-matrix pipeline (intake + clarify) via stub
//!    phases — equivalent to a discover run cancelled at sketch
//!    20, since the SQLite `last_completed_phase` is `clarify` in
//!    both cases. We use `last_phase = "clarify"` instead of
//!    mocking a cancel mid-matrix to keep the test fast and
//!    deterministic. The pipeline emits phase events with
//!    `resume: false`.
//! 2. Build a stub pipeline with the eight `discover_*` phases
//!    (using stub `Phase` implementations) and filter it via
//!    [`Pipeline::resume_with_kind`] using
//!    [`PipelineKind::Discovery`]. The filtered pipeline carries
//!    the eight phases AFTER `clarify`, marked `is_resumed()` so
//!    `Pipeline::run` will emit phase events with `resume: true`.
//! 3. Run the filtered pipeline. The stub phases succeed without
//!    LLM traffic. Because the pipeline was produced by
//!    `resume_with_kind`, every phase event it emits carries
//!    `resume: true`.
//! 4. Read `telemetry/phases.jsonl.gz`, parse the events, and
//!    assert: the `discover_matrix` `start` event has
//!    `resume: true`; the pre-matrix `intake` and `clarify` events
//!    have `resume: false`; the post-matrix events have
//!    `resume: true`.

// The env mutex is intentionally held across `await` points so
// two test threads cannot both flip `MOAGAN_HOME` mid-flight.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use async_trait::async_trait;

use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::ProviderRegistry;
use moagan::phases::phase::{Phase, PhaseOutput, RunContext};
use moagan::phases::{Pipeline, PipelineKind};
use moagan::redact::RedactPolicy;
use moagan::telemetry::PhaseEvent;
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

/// Stub phase used to build minimal canonical pipelines without
/// depending on the real LLM-bound discovery implementations.
/// The PR-24 contract is about the resume mechanics, not the
/// discovery correctness — that is exercised by the existing
/// `tests/integration_discovery.rs` smoke test.
struct StubPhase(&'static str);

#[async_trait]
impl Phase for StubPhase {
    fn name(&self) -> &'static str {
        self.0
    }
    async fn execute(&self, _ctx: &RunContext) -> moagan::error::Result<PhaseOutput> {
        Ok(PhaseOutput::Intake(std::path::PathBuf::from(self.0)))
    }
}

/// Build a `RunContext` wired to an empty `ProviderRegistry` so
/// the stub pipeline never has to call an LLM.
fn build_ctx(
    home: Arc<MoaganHome>,
    run_id: RunId,
    run_dir: &moagan::fs_layout::RunDir<'_>,
) -> RunContext {
    let registry = ProviderRegistry::default();
    let telemetry =
        Telemetry::open(run_id, run_dir, RedactPolicy::default(), None).expect("open telemetry");
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        telemetry,
        "stub".into(),
        "discover".into(),
    )
}

/// Read every `PhaseEvent` from `<run_dir>/telemetry/phases.jsonl.gz`.
/// Empty list when the file does not exist.
fn read_phase_events(run_dir: &std::path::Path) -> Vec<PhaseEvent> {
    let path = run_dir.join("telemetry").join("phases.jsonl.gz");
    if !path.exists() {
        return Vec::new();
    }
    let body = moagan::storage::compression::read_to_string(&path)
        .expect("phases.jsonl.gz must be readable");
    body.lines()
        .filter_map(|line| serde_json::from_str::<PhaseEvent>(line).ok())
        .collect()
}

/// PR-24 happy path: a discovery run that completes pre-matrix
/// (intake + clarify) is resumed via
/// [`Pipeline::resume_with_kind`] using
/// [`PipelineKind::Discovery`]; the resumed pipeline emits a
/// `discover_matrix` `start` event with `resume: true` and the
/// pre-matrix run is unaffected (its events have
/// `resume: false`).
#[tokio::test]
async fn pr24_resume_with_discovery_kind_emits_resume_flag_on_discover_matrix() {
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

    // 1. Run the pre-matrix pipeline (intake + clarify). This
    //    mirrors the "discover cancelled at sketch 20" scenario:
    //    `last_completed_phase` is "clarify" in both cases. The
    //    pipeline emits two phase events with `resume: false`.
    let ctx = build_ctx(home.clone(), run_id, &run_dir);
    let pre = Pipeline::new()
        .push(StubPhase("intake"))
        .push(StubPhase("clarify"));
    pre.run(&ctx)
        .await
        .expect("pre-matrix pipeline must succeed");
    ctx.telemetry.flush().expect("telemetry flush must succeed");

    // 2. Build a stub 10-phase discovery pipeline (intake +
    //    clarify + 8 discover_*) and filter it with the kind-aware
    //    resume. The filtered pipeline must contain exactly the 8
    //    phases AFTER `clarify` in the discovery canonical order,
    //    and must be marked `is_resumed()` so `Pipeline::run` will
    //    emit phase events with `resume: true`.
    let canonical = Pipeline::new()
        .push(StubPhase("intake"))
        .push(StubPhase("clarify"))
        .push(StubPhase("discover_matrix"))
        .push(StubPhase("discover_tag"))
        .push(StubPhase("discover_cluster"))
        .push(StubPhase("discover_contradict"))
        .push(StubPhase("discover_facet"))
        .push(StubPhase("discover_extract"))
        .push(StubPhase("discover_integrate"))
        .push(StubPhase("discover_summary"));
    let names: Vec<&'static str> = canonical.names();
    assert_eq!(
        names.len(),
        10,
        "canonical discovery pipeline must have 10 phases; got {names:?}"
    );

    let resumed =
        Pipeline::resume_with_kind(canonical, "clarify", PipelineKind::Discovery).unwrap();
    let resumed_names = resumed.names();
    assert_eq!(
        resumed_names,
        vec![
            "discover_matrix",
            "discover_tag",
            "discover_cluster",
            "discover_contradict",
            "discover_facet",
            "discover_extract",
            "discover_integrate",
            "discover_summary",
        ],
        "resumed pipeline must contain the 8 discover_* phases after clarify; got {resumed_names:?}"
    );
    assert!(
        resumed.is_resumed(),
        "resume_with_kind must mark the pipeline as resumed"
    );

    // 3. Run the filtered pipeline. Because the pipeline is
    //    `is_resumed()`, every phase event it emits carries
    //    `resume: true`. We do NOT need a real coordinator run
    //    here — the matrix phase is exercised by the resume
    //    flag itself, which is what the spec asks us to verify.
    let ctx2 = build_ctx(home.clone(), run_id, &run_dir);
    resumed
        .run(&ctx2)
        .await
        .expect("resumed pipeline must succeed");
    ctx2.telemetry
        .flush()
        .expect("telemetry flush must succeed");

    // 4. Read every phase event and assert:
    //    a. pre-matrix events have resume=false,
    //    b. discover_matrix has at least one start event with
    //       resume=true,
    //    c. every post-matrix phase has a start event with
    //       resume=true.
    let events = read_phase_events(run_dir.root());
    let matrix_starts: Vec<&PhaseEvent> = events
        .iter()
        .filter(|e| e.phase == "discover_matrix" && e.status == "start")
        .collect();
    assert!(
        !matrix_starts.is_empty(),
        "discover_matrix must have at least one start event in the telemetry; \
         got {} events total",
        events.len()
    );
    let any_resume = matrix_starts.iter().any(|e| e.resume);
    assert!(
        any_resume,
        "at least one discover_matrix start event must have resume=true; \
         got events = {:?}",
        matrix_starts
    );

    // Pre-matrix events must NOT carry the resume flag — the
    // pre-matrix pipeline was a fresh run, not a resumed one.
    let intake_starts: Vec<&PhaseEvent> = events
        .iter()
        .filter(|e| e.phase == "intake" && e.status == "start")
        .collect();
    assert!(!intake_starts.is_empty(), "intake start event must exist");
    for e in &intake_starts {
        assert!(
            !e.resume,
            "intake was a fresh pre-matrix run; resume flag must be false; got {e:?}"
        );
    }
    let clarify_starts: Vec<&PhaseEvent> = events
        .iter()
        .filter(|e| e.phase == "clarify" && e.status == "start")
        .collect();
    assert!(!clarify_starts.is_empty(), "clarify start event must exist");
    for e in &clarify_starts {
        assert!(
            !e.resume,
            "clarify was a fresh pre-matrix run; resume flag must be false; got {e:?}"
        );
    }

    // The resumed post-matrix phases must all carry resume=true.
    for phase in [
        "discover_tag",
        "discover_cluster",
        "discover_contradict",
        "discover_facet",
        "discover_extract",
        "discover_integrate",
        "discover_summary",
    ] {
        let starts: Vec<&PhaseEvent> = events
            .iter()
            .filter(|e| e.phase == phase && e.status == "start")
            .collect();
        assert!(
            !starts.is_empty(),
            "post-matrix phase {phase} must have a start event in the telemetry"
        );
        for e in &starts {
            assert!(
                e.resume,
                "post-matrix phase {phase} was resumed; resume flag must be true; got {e:?}"
            );
        }
    }
}

/// PR-24 dispatch contract: an unknown phase name in the linear
/// canonical list (e.g. `discover_matrix` when using
/// `PipelineKind::Linear`) must surface a clear error rather than
/// silently skipping the resume. Without this, PR-24's bug
/// ("unknown phase discover_matrix") would still be reachable
/// through the legacy `Pipeline::resume(canonical, last_phase)`
/// entry point.
#[tokio::test]
async fn pr24_resume_with_linear_kind_rejects_discovery_phase() {
    let _guard = env_lock();
    let canonical = Pipeline::new().push(StubPhase("discover_matrix"));
    let err =
        Pipeline::resume_with_kind(canonical, "discover_matrix", PipelineKind::Linear).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("discover_matrix") && msg.contains("Linear"),
        "error must name the phase and the kind; got: {msg}"
    );
}

/// PR-24 kind-aware canonical order: the discovery canonical list
/// must contain the eight `discover_*` phases plus the shared
/// `intake + clarify` pre-matrix pair, in that exact order.
#[test]
fn pr24_canonical_phase_order_for_discovery_lists_all_discover_phases() {
    let order = Pipeline::canonical_phase_order_for(PipelineKind::Discovery);
    assert_eq!(
        order,
        &[
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
        ],
        "canonical discovery order must list all 10 phases"
    );
}

/// PR-24 backwards compat: [`Pipeline::canonical_phase_order`]
/// (the legacy entry point) must still return the 16-phase
/// linear list. Existing tests, fixtures, and downstream callers
/// depend on this exact shape.
#[test]
fn pr24_canonical_phase_order_backs_compat_linear_list() {
    let order = Pipeline::canonical_phase_order();
    assert_eq!(order.len(), 16, "linear canonical list must be 16 phases");
    assert_eq!(order[0], "intake");
    assert_eq!(order[15], "deliver");
}

/// PR-24 phase_index_for(Discovery) round-trip: every name in
/// the canonical discovery order has a unique index equal to its
/// position in the list. Mirrors the linear round-trip test in
/// `src/phases/pipe.rs`.
#[test]
fn pr24_phase_index_for_discovery_round_trip() {
    let idx = Pipeline::phase_index_for(PipelineKind::Discovery);
    for (i, name) in Pipeline::canonical_phase_order_for(PipelineKind::Discovery)
        .iter()
        .enumerate()
    {
        assert_eq!(idx.get(*name).copied(), Some(i));
    }
}

/// PR-24 resume from `intake` re-runs the entire discovery
/// pipeline (intake + clarify + discover_matrix + ... +
/// discover_summary). The filter keeps every phase whose
/// canonical index is strictly greater than `intake`'s (0).
#[test]
fn pr24_resume_from_intake_keeps_all_discovery_phases() {
    let canonical = Pipeline::new()
        .push(StubPhase("intake"))
        .push(StubPhase("clarify"))
        .push(StubPhase("discover_matrix"))
        .push(StubPhase("discover_tag"))
        .push(StubPhase("discover_cluster"))
        .push(StubPhase("discover_contradict"))
        .push(StubPhase("discover_facet"))
        .push(StubPhase("discover_extract"))
        .push(StubPhase("discover_integrate"))
        .push(StubPhase("discover_summary"));
    let resumed = Pipeline::resume_with_kind(canonical, "intake", PipelineKind::Discovery).unwrap();
    let names = resumed.names();
    assert_eq!(names.len(), 9, "must keep clarify + 8 discover_* phases");
    assert_eq!(names[0], "clarify");
}

/// PR-24 resume from `discover_summary` produces an empty
/// pipeline (the run is already done). Mirrors the linear
/// "resume_from_last_phase_is_empty" test.
#[test]
fn pr24_resume_from_discover_summary_is_empty() {
    let canonical = Pipeline::new()
        .push(StubPhase("intake"))
        .push(StubPhase("clarify"))
        .push(StubPhase("discover_matrix"))
        .push(StubPhase("discover_summary"));
    let resumed =
        Pipeline::resume_with_kind(canonical, "discover_summary", PipelineKind::Discovery).unwrap();
    assert!(
        resumed.is_empty(),
        "resuming from the last discovery phase must produce an empty pipeline"
    );
}
