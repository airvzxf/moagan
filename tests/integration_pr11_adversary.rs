//! PR-11 integration test: verify the `AdversaryPhase` wires
//! correctly into a full pipeline run. Mirrors the MVP smoke
//! (`tests/integration_mvp.rs`) but inserts the new phase between
//! `JudgePhase` and `RankPhase`, then asserts the
//! `rankings/adversary_report.json` sidecar exists with seven
//! sections (one per canonical [`AdversaryPattern`]).
//!
//! We use `Mode::Fast` instead of `Mode::Deep` to keep the mock
//! fixture compact (fast mode runs 3 proposals × 3 judges = 9
//! judge calls + 6 critique calls, vs. deep mode's 17 × 5 = 85
//! judge calls). The pipeline builder wires the phase for both
//! modes identically — fast is just a cheaper stand-in.
//!
//! Spec reference: docs/v0.5-roadmap.md PR-11 (D.22.1, D.12.5).

use std::sync::Arc;

use moagan::config::Config;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::{MockProvider, MockResponse, ProviderRegistry};
use moagan::phases::{
    AdversaryPhase, ClarifyPhase, CritiquePhase, DeliverPhase, GatePhase, IntakePhase, JudgePhase,
    PATTERN_ADVERSARY_SCHEMA_VERSION, PhaseOutput, Pipeline, ProposePhase, RankPhase, RepairPhase,
    RoutePhase, RunContext,
};
use moagan::ranking::adversary_patterns::AdversaryPattern;
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

/// Process-wide mutex that serialises every test which mutates the
/// `MOAGAN_HOME` env var. See `tests/integration_mvp.rs` for the
/// rationale.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn build_mock_provider() -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(intake_json()));
    p.push(MockResponse::plain(clarify_json()));
    p.push(MockResponse::plain(route_json()));
    for i in 0..3 {
        p.push(MockResponse::plain(propose_json(&format!("p_00{i}"))));
    }
    for _ in 0..6 {
        p.push(MockResponse::plain(critique_json()));
    }
    for _ in 0..9 {
        p.push(MockResponse::plain(judge_json()));
    }
    p.push(MockResponse::plain(deliver_json()));
    p.set_cycle(false);
    Arc::new(p)
}

fn intake_json() -> &'static str {
    r#"{
  "problem": "List the seven colors of the rainbow",
  "objectives": ["List the colors in standard order"],
  "constraints": ["Standard ROYGBIV order"],
  "non_goals": ["Physics, wavelengths, or color theory beyond naming"],
  "open_questions": [],
  "raw_prompt": "Enumera los 7 colores del arcoiris en orden"
}"#
}

fn clarify_json() -> &'static str {
    r#"{
  "problem": "List the seven colors of the rainbow in standard order",
  "objectives": ["Produce ROYGBIV in order"],
  "deliverables": ["A list of seven color names"],
  "constraints": ["Use the canonical English names"],
  "assumptions": ["The user is asking for the standard 7-color model"],
  "non_goals": ["Detailed wavelength information"],
  "acceptance": ["The list is exactly R, O, Y, G, B, I, V in that order"],
  "risks": ["Off-by-one if the user means a different model"]
}"#
}

fn route_json() -> &'static str {
    r#"{
  "mode": "fast",
  "reason": "Simple enumeration, no architecture needed",
  "sketches": 0,
  "proposals": 3,
  "judges": 3
}"#
}

fn propose_json(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "summary": "Standard ROYGBIV in English ({id})",
  "approach": "Output the canonical order: red, orange, yellow, green, blue, indigo, violet.",
  "tradeoffs": ["None — the user asked for the standard order"],
  "evidence": ["Wikipedia: Rainbow", "Standard 7-color model"]
}}"#
    )
}

fn critique_json() -> &'static str {
    r#"{
  "verdict": "accept",
  "issues": [],
  "suggestions": []
}"#
}

fn judge_json() -> &'static str {
    r#"{
  "score": 8.0,
  "criteria": {
    "correctness": 9.0,
    "completeness": 8.0,
    "fit": 9.0,
    "evidence": 8.0,
    "clarity": 8.0
  },
  "comments": "Clean and correct."
}"#
}

fn deliver_json() -> &'static str {
    r#"{
  "title": "The seven colors of the rainbow",
  "summary": "Red, orange, yellow, green, blue, indigo, violet.",
  "recommendation": "Use the canonical ROYGBIV order.",
  "alternatives": ["Use a regional naming convention if the audience is young children."],
  "next_steps": ["Verify with a quick visual check against any standard reference."]
}"#
}

fn build_run_context(
    home: Arc<MoaganHome>,
    provider: Arc<MockProvider>,
    run_id: RunId,
    adversary_enabled: bool,
) -> RunContext {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = provider.clone();
    registry.insert("mock".into(), arc);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("open telemetry");
    let parallelism = Parallelism::new(2);
    // `mode` is a free-form string here — the pipeline builder
    // does not consult it. We use it only as a label in the
    // `RunContext`. The phase's opt-in is gated by the
    // `adversary_enabled` flag passed below.
    let mode_label = if adversary_enabled { "deep" } else { "fast" };
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Enumera los 7 colores del arcoíris en orden".into(),
        mode_label.into(),
    )
}

fn build_pipeline(adversary_enabled: bool) -> Pipeline {
    Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(ProposePhase { count: 3 })
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: 2,
        })
        .push(RepairPhase::default())
        .push(JudgePhase {
            judges: 3,
            ..JudgePhase::default()
        })
        .push(AdversaryPhase {
            enable: adversary_enabled,
        })
        .push(RankPhase {
            config: Arc::new(Config::default()),
            replace_sources_enabled: false,
            stability_enabled: false,
        })
        .push(DeliverPhase)
}

/// End-to-end smoke: with the phase enabled, the pipeline writes
/// `rankings/adversary_report.json` with exactly seven sections,
/// one per canonical pattern. This is the PR-11 acceptance
/// contract.
#[test]
fn adversary_phase_writes_seven_section_report() -> Result<()> {
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let run_id = RunId::new();
    let provider = build_mock_provider();
    let ctx = build_run_context(home.clone(), provider, run_id, true);

    let pipeline = build_pipeline(true);
    let outputs = pollster::block_on(pipeline.run(&ctx))?;
    // 11 phases: intake, clarify, route, propose, gate, critique,
    // repair, judge, adversary, rank, deliver.
    assert_eq!(outputs.len(), 11, "expected 11 phase outputs");

    // Flush telemetry so the gzip stream is finalised.
    ctx.telemetry.flush()?;

    // The `PhaseOutput::PatternAdversary` variant must point at
    // the sidecar we are about to inspect.
    let adv_output = outputs
        .iter()
        .find_map(|o| match o {
            PhaseOutput::PatternAdversary(p) => Some(p.clone()),
            _ => None,
        })
        .expect("AdversaryPhase must emit PhaseOutput::PatternAdversary");
    assert!(adv_output.exists(), "adversary report sidecar must exist");

    // Read the sidecar and assert the 7-section contract.
    let raw = std::fs::read(&adv_output).expect("sidecar readable");
    let report: moagan::phases::PatternAdversaryReport =
        serde_json::from_slice(&raw).expect("sidecar parses");
    assert_eq!(
        report.sections.len(),
        7,
        "expected 7 sections (one per AdversaryPattern), got {}",
        report.sections.len()
    );
    assert_eq!(report.schema_version, PATTERN_ADVERSARY_SCHEMA_VERSION);

    // The section ordering must mirror `AdversaryPattern::all_seven()`
    // so downstream consumers can iterate deterministically.
    let observed: Vec<AdversaryPattern> = report.sections.iter().map(|s| s.pattern).collect();
    assert_eq!(observed, AdversaryPattern::all_seven().to_vec());

    // Each section must carry a per-proposal verdict so the
    // dashboard can render per-pattern / per-proposal drill-downs.
    for section in &report.sections {
        assert!(
            !section.per_proposal.is_empty(),
            "section {:?} must carry per-proposal verdicts",
            section.pattern,
        );
    }

    Ok(())
}

/// When the phase is disabled (`enable = false`) the pipeline
/// still produces a sidecar so downstream consumers can
/// distinguish "ran with zero proposals" from "phase was
/// skipped" (missing file). The disabled sidecar has zero
/// proposals and an empty `sections` vector.
#[test]
fn adversary_phase_disabled_emits_empty_sidecar() -> Result<()> {
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let run_id = RunId::new();
    let provider = build_mock_provider();
    let ctx = build_run_context(home.clone(), provider, run_id, false);

    let pipeline = build_pipeline(false);
    let outputs = pollster::block_on(pipeline.run(&ctx))?;

    let adv_output = outputs
        .iter()
        .find_map(|o| match o {
            PhaseOutput::PatternAdversary(p) => Some(p.clone()),
            _ => None,
        })
        .expect("disabled AdversaryPhase must still emit a sidecar");

    let raw = std::fs::read(&adv_output).expect("sidecar readable");
    let report: moagan::phases::PatternAdversaryReport =
        serde_json::from_slice(&raw).expect("sidecar parses");
    assert_eq!(report.proposal_count, 0);
    assert!(report.sections.is_empty());
    Ok(())
}
