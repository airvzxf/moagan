//! End-to-end test for sub-fase C (validators + sandbox).
//!
//! Spins up the mock provider, builds a deep-mode pipeline that
//! includes the new ValidatePhase, runs it, and verifies:
//!
//! - Every proposal gets a `validation/p_<id>.evidence.json`
//!   sidecar with a structural and a constraints entry.
//! - The evidence sidecars aggregate to a Pass verdict when the
//!   mock-fed proposals are clean.
//! - The Gate phase still writes its own `validation/p_<id>.json`
//!   sidecar (so the existing Repair phase keeps working).
//!
//! The test deliberately does not depend on `cargo`, `python3`,
//! or `tsc` being installed; the language validators only run
//! against artifacts, and the proposal surface does not yet
//! expose them, so every language validator returns a Skipped
//! verdict by design. The structural and constraints checks
//! cover the per-proposal contract.

use std::sync::Arc;

use moagan::config::Config;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::MockProvider;
use moagan::llm::{MockResponse, ProviderRegistry};
use moagan::phases::{
    ClarifyPhase, CritiquePhase, DeliverPhase, GatePhase, IntakePhase, JudgePhase, Pipeline,
    ProposePhase, RankPhase, RepairPhase, RoutePhase, RunContext, SketchPhase, ValidatePhase,
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

fn deep_mock_provider(proposals: usize) -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(intake_json()));
    p.push(MockResponse::plain(clarify_json()));
    p.push(MockResponse::plain(route_json()));
    // deep mode runs 6 sketches before propose.
    for _ in 0..6 {
        p.push(MockResponse::plain(sketch_json()));
    }
    for i in 0..proposals {
        p.push(MockResponse::plain(propose_json(&format!("p_{i:03}"))));
    }
    // deep mode runs 4 critics per proposal.
    for _ in 0..(proposals * 4) {
        p.push(MockResponse::plain(critique_json()));
    }
    for _ in 0..(proposals * 3) {
        p.push(MockResponse::plain(judge_json()));
    }
    p.push(MockResponse::plain(deliver_json()));
    p.set_cycle(false);
    Arc::new(p)
}

fn intake_json() -> &'static str {
    r#"{"problem":"x","objectives":[],"constraints":[],"non_goals":[],"open_questions":[],"raw_prompt":"x"}"#
}

fn clarify_json() -> &'static str {
    r#"{"problem":"x","objectives":[],"deliverables":[],"constraints":[],"assumptions":[],"non_goals":[],"acceptance":[],"risks":[]}"#
}

fn route_json() -> &'static str {
    r#"{"mode":"deep","reason":"x","sketches":6,"proposals":5,"judges":3}"#
}

fn sketch_json() -> &'static str {
    r#"{"thesis":"x","key_decisions":["a"],"architecture_outline":"x","assumptions":[],"strengths":["s"],"weaknesses":["w"],"hard_constraint_check":{},"expected_validation":"x","angle":"pragmatic","id":""}"#
}

fn propose_json(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "summary": "A proposal summary that easily clears the structural length floor.",
  "approach": "Detailed approach for {id} with enough text.",
  "tradeoffs": ["tradeoff one"],
  "evidence": ["evidence one"]
}}"#
    )
}

fn critique_json() -> &'static str {
    r#"{"verdict":"accept","issues":[],"suggestions":[]}"#
}

fn judge_json() -> &'static str {
    r#"{"score":8.0,"criteria":{"correctness":8.0,"completeness":8.0,"fit":8.0,"evidence":8.0,"clarity":8.0},"comments":"x"}"#
}

fn deliver_json() -> &'static str {
    r#"{"title":"x","summary":"x","recommendation":"x","alternatives":[],"next_steps":[]}"#
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
        "x".into(),
        "deep".into(),
    )
}

#[test]
fn validate_phase_writes_evidence_for_every_proposal() -> Result<()> {
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let proposals = 3;
    let run_id = RunId::new();
    let provider = deep_mock_provider(proposals);
    let ctx = build_run_context(home.clone(), provider, run_id);

    let pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(SketchPhase { count: 6 })
        .push(ProposePhase {
            count: proposals as u32,
        })
        .push(ValidatePhase::new())
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: 4,
        })
        .push(RepairPhase::default())
        .push(JudgePhase { judges: 3 })
        .push(RankPhase {
            config: Arc::new(Config::default()),
        })
        .push(DeliverPhase);

    let outputs = pollster::block_on(pipeline.run(&ctx))?;
    ctx.telemetry.flush()?;

    let run_dir = home.run_dir(run_id);
    let validation_dir = run_dir.validation();

    let mut evidence_files = 0;
    let mut gate_files = 0;
    for entry in std::fs::read_dir(&validation_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".evidence.json") {
            evidence_files += 1;
            let sidecar: serde_json::Value =
                serde_json::from_reader(std::fs::File::open(entry.path())?)?;
            let status = sidecar
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let validators = sidecar
                .get("validators")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            assert!(
                validators >= 2,
                "evidence sidecar should list at least structural + constraints"
            );
            assert_eq!(status, "pass", "clean proposals must aggregate to pass");
        } else if name.ends_with(".json") && !name.ends_with(".meta.json") {
            gate_files += 1;
        }
    }
    assert_eq!(
        evidence_files, proposals,
        "every proposal should have a Validate evidence sidecar"
    );
    assert_eq!(
        gate_files, proposals,
        "every proposal should also have a Gate sidecar (Repairs reads it)"
    );

    // The pipeline should have a Validate phase output mixed in
    // alongside the Gate output. Both report through
    // PhaseOutput::Validations, so the exact count is N proposals.
    assert!(
        outputs
            .iter()
            .filter(|o| matches!(o, moagan::phases::PhaseOutput::Validations(_)))
            .count()
            >= 2,
        "Validate and Gate both produce PhaseOutput::Validations"
    );

    Ok(())
}
