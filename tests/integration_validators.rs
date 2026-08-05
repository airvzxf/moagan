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
    ClarifyPhase, CritiquePhase, DeliverPhase, GatePhase, IntakePhase, JudgePhase, Phase, Pipeline,
    ProposePhase, RankPhase, RepairPhase, RoutePhase, RunContext, SketchPhase, ValidatePhase,
};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;
use moagan::test_support::with_moagan_home;

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

use moagan::phases::util::write_json;
use moagan::validators::CodeArtifact;

#[test]
fn validate_phase_dispatches_artifacts_to_language_validators() -> Result<()> {
    // The integration below seeds proposals directly on disk so
    // we can attach real artifacts and watch the language
    // validators actually run. cargo / python3 / tsc availability
    // is honoured by the per-validator tests (they short-circuit
    // to Skipped when the binary is missing); this test asserts
    // only the dispatch wiring, not the per-language outcome.
    //
    // `with_moagan_home` serialises this test against any other
    // test that mutates MOAGAN_HOME in the same process (the
    // phase_d / phase_h / phase_l integration suites do), and
    // gives each call a unique tempdir so the meta.sqlite from
    // one run cannot collide with another.
    with_moagan_home(
        "validate_phase_dispatches_artifacts_to_language_validators",
        |tmp| {
            let home = Arc::new(MoaganHome::resolve()?);
            home.ensure()?;

            let run_id = RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure()?;
            let proposals_dir = run_dir.proposals();
            std::fs::create_dir_all(&proposals_dir)?;

            // Proposal 1: a Rust artifact that obviously is source.
            let rust_artifact = CodeArtifact::new(
                "src/lib.rs",
                "rust",
                "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
            );
            let proposal_with_rust = moagan::domain::Proposal {
                id: "p_000".into(),
                summary: "A proposal with a Rust source attachment.".into(),
                approach: "Uses the rust artifact below.".into(),
                tradeoffs: vec!["none".into()],
                evidence: vec!["self-attached source".into()],
                source_sketch: String::new(),
                artifacts: vec![rust_artifact],
                replaced_by: None,
                source_nodes: Vec::new(),
            };
            write_json(&proposals_dir.join("p_000.json"), &proposal_with_rust)?;

            // Proposal 2: empty artifact list (the common case today).
            let proposal_no_artifacts = moagan::domain::Proposal {
                id: "p_001".into(),
                summary: "A proposal with no executable attachment.".into(),
                approach: "Pure prose, no code.".into(),
                tradeoffs: vec!["none".into()],
                evidence: vec!["none".into()],
                source_sketch: String::new(),
                artifacts: vec![],
                replaced_by: None,
                source_nodes: Vec::new(),
            };
            write_json(&proposals_dir.join("p_001.json"), &proposal_no_artifacts)?;

            // Build a context with a mock provider the Validate phase
            // never touches (it does not issue LLM calls).
            let provider = Arc::new(MockProvider::empty());
            let ctx = build_run_context(home.clone(), provider, run_id);

            let phase = ValidatePhase::new();
            let outputs = {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async {
                    let out = phase.execute(&ctx).await?;
                    // Touch the output to silence dead-code about PhaseOutput.
                    match &out {
                        moagan::phases::PhaseOutput::Validations(_) => {
                            Ok::<_, moagan::error::Error>(out)
                        }
                        _ => panic!("ValidatePhase must emit PhaseOutput::Validations"),
                    }
                })
            }?;
            ctx.telemetry.flush()?;

            let p0_evidence =
                std::fs::read_to_string(run_dir.validation().join("evidence").join("p_000.json"))?;
            assert!(
                p0_evidence.contains("\"validator\":\"rust\""),
                "p_000 with a Rust artifact must produce a rust validator entry"
            );
            let p1_evidence =
                std::fs::read_to_string(run_dir.validation().join("evidence").join("p_001.json"))?;
            assert!(
                !p1_evidence.contains("\"validator\":\"rust\""),
                "p_001 with no artifacts must not invoke any language validator"
            );
            // Both sidecars must still carry the structural + constraints validators.
            assert!(p0_evidence.contains("\"validator\":\"structural\""));
            assert!(p0_evidence.contains("\"validator\":\"constraints\""));
            assert!(p1_evidence.contains("\"validator\":\"structural\""));
            assert!(p1_evidence.contains("\"validator\":\"constraints\""));

            // The Validate phase output itself wraps the two evidence paths.
            assert!(matches!(
                outputs,
                moagan::phases::PhaseOutput::Validations(_)
            ));

            // `tmp` is the unique tempdir `with_moagan_home`
            // chose for this call. Reference it once so the test
            // is observably using the helper rather than the
            // old `tempfile::tempdir` path.
            assert!(tmp.is_dir());

            Ok(())
        },
    )
}

#[test]
fn validate_phase_writes_evidence_for_every_proposal() -> Result<()> {
    with_moagan_home("validate_phase_writes_evidence_for_every_proposal", |_| {
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
            .push(JudgePhase {
                judges: 3,
                ..JudgePhase::default()
            })
            .push(RankPhase {
                config: Arc::new(Config::default()),
                replace_sources_enabled: false,
                stability_enabled: false,
            })
            .push(DeliverPhase);

        let outputs = pollster::block_on(pipeline.run(&ctx))?;
        ctx.telemetry.flush()?;

        let run_dir = home.run_dir(run_id);
        let validation_dir = run_dir.validation();
        let evidence_dir = validation_dir.join("evidence");

        let mut evidence_files = 0;
        let mut gate_files = 0;
        for entry in std::fs::read_dir(&validation_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") && !name.ends_with(".meta.json") {
                gate_files += 1;
            }
        }
        for entry in std::fs::read_dir(&evidence_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") && !name.ends_with(".meta.json") {
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
            }
        }
        assert_eq!(
            evidence_files, proposals,
            "every proposal should have a Validate evidence sidecar under validation/evidence/"
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
    })
}

/// The Validate phase must read `brief.json` and feed the real
/// constraints into the ConstraintsValidator so the "requisitos
/// duros" check actually runs against the proposal text. Before
/// this fix the trait path short-circuited to `no_constraints_in_brief`
/// for every proposal regardless of the brief content.
#[test]
fn validate_phase_propagates_brief_constraints_to_constraints_validator() -> Result<()> {
    with_moagan_home("validate_phase_propagates_brief_constraints", |_| {
        let home = Arc::new(MoaganHome::resolve()?);
        home.ensure()?;

        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure()?;

        // Brief with two hard constraints, one of which the proposal
        // echoes and one it does not.
        let brief = moagan::domain::Brief {
            problem: "x".into(),
            objectives: vec![],
            deliverables: vec![],
            constraints: vec!["tokio runtime".into(), "deploy via systemd".into()],
            assumptions: vec![],
            non_goals: vec![],
            acceptance: vec![],
            risks: vec![],
            context_block: None,
        };
        write_json(&run_dir.brief(), &brief)?;

        // Proposal that echoes the first constraint verbatim and omits
        // the second — so the constraints validator must record one
        // check as run and one as failed (overall verdict: Warn).
        let proposal = moagan::domain::Proposal {
            id: "p_000".into(),
            summary: "summary that's long enough to clear the structural length floor".into(),
            approach: "Build a CLI that uses tokio runtime for async I/O.".into(),
            tradeoffs: vec!["t".into()],
            evidence: vec!["e".into()],
            source_sketch: String::new(),
            artifacts: vec![],
            replaced_by: None,
            source_nodes: Vec::new(),
        };
        let proposals_dir = run_dir.proposals();
        std::fs::create_dir_all(&proposals_dir)?;
        write_json(&proposals_dir.join("p_000.json"), &proposal)?;

        let provider = Arc::new(MockProvider::empty());
        let ctx = build_run_context(home.clone(), provider, run_id);

        let phase = ValidatePhase::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async { phase.execute(&ctx).await })?;
        ctx.telemetry.flush()?;

        let sidecar_path = run_dir.validation().join("evidence").join("p_000.json");
        let raw = std::fs::read_to_string(&sidecar_path)?;
        let sidecar: serde_json::Value = serde_json::from_str(&raw)?;
        let constraints_entry = sidecar
            .get("validators")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|v| v.get("validator").and_then(|s| s.as_str()) == Some("constraints"))
            })
            .expect("evidence must include a constraints validator entry");

        let checks_run: Vec<String> = constraints_entry
            .get("checks_run")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_owned()))
                    .collect()
            })
            .unwrap_or_default();

        assert!(
            checks_run.iter().any(|c| c.contains("tokio runtime")),
            "matched constraint must appear in checks_run, got {checks_run:?}"
        );
        let failed: Vec<String> = constraints_entry
            .get("failed_checks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            failed.iter().any(|c| c.contains("deploy via systemd")),
            "missing constraint must appear in failed_checks, got {failed:?}"
        );
        assert!(
            !checks_run.iter().any(|c| c == "no_constraints_in_brief"),
            "phase must not short-circuit when the brief has constraints, got {checks_run:?}"
        );

        let status = constraints_entry
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        assert_eq!(
            status, "Warn",
            "missing constraint must downgrade the constraints verdict to Warn"
        );

        Ok(())
    })
}

/// The Validate phase must dispatch sql artifacts to SqlValidator
/// and record the verdict. Without sqlite3 on the host the
/// evidence is "parse only" Pass; with sqlite3 the validator
/// additionally executes the statement in-memory.
#[test]
fn validate_phase_dispatches_sql_artifact() -> Result<()> {
    with_moagan_home("validate_phase_dispatches_sql_artifact", |_| {
        let home = Arc::new(MoaganHome::resolve()?);
        home.ensure()?;

        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure()?;

        let sql_artifact = CodeArtifact::new(
            "dialect:sqlite",
            "sql-sqlite",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); SELECT * FROM t;",
        );
        let proposal = moagan::domain::Proposal {
            id: "p_000".into(),
            summary: "A proposal that ships a SQL schema and a SELECT.".into(),
            approach: "Build the table with sqlite, query it back.".into(),
            tradeoffs: vec!["t".into()],
            evidence: vec!["e".into()],
            source_sketch: String::new(),
            artifacts: vec![sql_artifact],
            replaced_by: None,
            source_nodes: Vec::new(),
        };
        let proposals_dir = run_dir.proposals();
        std::fs::create_dir_all(&proposals_dir)?;
        write_json(&proposals_dir.join("p_000.json"), &proposal)?;

        let provider = Arc::new(MockProvider::empty());
        let ctx = build_run_context(home.clone(), provider, run_id);

        let phase = ValidatePhase::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async { phase.execute(&ctx).await })?;
        ctx.telemetry.flush()?;

        let sidecar_path = run_dir.validation().join("evidence").join("p_000.json");
        let raw = std::fs::read_to_string(&sidecar_path)?;
        assert!(
            raw.contains("\"validator\":\"sql\""),
            "evidence must include a sql validator entry; got {raw}"
        );

        Ok(())
    })
}

/// The Validate phase must dispatch a JSON Schema artifact to
/// SchemaValidator and record the verdict. Uses the paired
/// pattern (one `json-schema` artifact + one `json` artifact)
/// since that is the most common case in real proposals.
#[test]
fn validate_phase_dispatches_schema_artifact() -> Result<()> {
    with_moagan_home("validate_phase_dispatches_schema_artifact", |_| {
        let home = Arc::new(MoaganHome::resolve()?);
        home.ensure()?;

        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure()?;

        let schema_artifact = CodeArtifact::new(
            "user.schema.json",
            "json-schema",
            r#"{
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {"id": {"type": "integer"}, "name": {"type": "string"}},
                "required": ["id", "name"]
            }"#,
        );
        let data_artifact = CodeArtifact::new("user.json", "json", r#"{"id": 1, "name": "alice"}"#);
        let proposal = moagan::domain::Proposal {
            id: "p_000".into(),
            summary: "A proposal that ships a JSON schema and matching data.".into(),
            approach: "Validate the user payload against the schema.".into(),
            tradeoffs: vec!["t".into()],
            evidence: vec!["e".into()],
            source_sketch: String::new(),
            artifacts: vec![schema_artifact, data_artifact],
            replaced_by: None,
            source_nodes: Vec::new(),
        };
        let proposals_dir = run_dir.proposals();
        std::fs::create_dir_all(&proposals_dir)?;
        write_json(&proposals_dir.join("p_000.json"), &proposal)?;

        let provider = Arc::new(MockProvider::empty());
        let ctx = build_run_context(home.clone(), provider, run_id);

        let phase = ValidatePhase::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async { phase.execute(&ctx).await })?;
        ctx.telemetry.flush()?;

        let sidecar_path = run_dir.validation().join("evidence").join("p_000.json");
        let raw = std::fs::read_to_string(&sidecar_path)?;
        let sidecar: serde_json::Value = serde_json::from_str(&raw)?;
        let schema_entry = sidecar
            .get("validators")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|v| v.get("validator").and_then(|s| s.as_str()) == Some("schema"))
            })
            .expect("evidence must include a schema validator entry");
        let status = schema_entry
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        assert_eq!(status, "Pass", "schema validator must report Pass");
        let checks_run: Vec<String> = schema_entry
            .get("checks_run")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            checks_run.iter().any(|c| c.contains("validated 1 pair")),
            "schema validator must report one validated pair, got {checks_run:?}"
        );

        Ok(())
    })
}
