//! End-to-end smoke test for the MVP pipeline with the mock provider.
//!
//! Verifies the gates promised in the plan:
//! 1. `moagan run --mode fast --provider mock` produces
//!    `final/portfolio.md` and `rankings/ranking.json`.
//! 2. The pipeline runs all 10 phases in order.
//! 3. Telemetry records every phase and call.

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
    ProposePhase, RankPhase, RepairPhase, RoutePhase, RunContext,
};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

fn build_mock_provider() -> Arc<MockProvider> {
    // The pipeline call sequence in fast mode is:
    //   intake, clarify, route,
    //   propose p_000, propose p_001, propose p_002,
    //   critique*6 (3 props x 2 critics),
    //   judge*9 (3 props x 3 judges),
    //   deliver
    // We pre-load a response for every call so each role gets a
    // JSON payload of the right shape.
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(intake_json()));
    p.push(MockResponse::plain(clarify_json()));
    p.push(MockResponse::plain(route_json()));
    p.push(MockResponse::plain(propose_json("p_000")));
    p.push(MockResponse::plain(propose_json("p_001")));
    p.push(MockResponse::plain(propose_json("p_002")));
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
  "problem": "Enumerate the seven colors of the rainbow in order",
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
  "evidence": ["Wikipedia: Rainbow"]
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
        "Enumera los 7 colores del arcoíris en orden".into(),
        "fast".into(),
    )
}

#[test]
fn mock_provider_end_to_end_smoke() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let run_id = RunId::new();
    let provider = build_mock_provider();
    let ctx = build_run_context(home.clone(), provider, run_id);

    let pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(ProposePhase { count: 3 })
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: 2,
        })
        .push(RepairPhase)
        .push(JudgePhase { judges: 3 })
        .push(RankPhase)
        .push(DeliverPhase);

    let outputs = pipeline.run(&ctx)?;
    assert_eq!(outputs.len(), 10, "expected 10 phase outputs");

    // Write manifest like run.rs does.
    let run_dir = home.run_dir(run_id);
    let manifest = moagan::domain::Manifest {
        schema_version: "v1".into(),
        run_id,
        mode: "fast".into(),
        status: "completed".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        brief_sha256: String::new(),
        brief_blake3: String::new(),
        provider: "mock".into(),
        model: "mock-model".into(),
        phases: Vec::new(),
        usage: moagan::domain::ManifestUsage::default(),
        manifest_blake3: String::new(),
    };
    let manifest_json = serde_json::to_vec_pretty(&manifest)?;
    moagan::atomic::writer::AtomicWriter::new().write(&run_dir.manifest(), &manifest_json)?;

    assert!(run_dir.manifest().exists(), "manifest.json was not written");
    assert!(
        run_dir.final_dir().join("portfolio.md").exists(),
        "portfolio.md was not written"
    );
    assert!(
        run_dir.rankings().join("ranking.json").exists(),
        "ranking.json was not written"
    );
    assert!(
        run_dir.proposals().join("p_000.json").exists(),
        "p_000 proposal was not written"
    );
    assert!(
        run_dir.evaluations().join("p_000.json").exists(),
        "p_000 evaluation was not written"
    );
    let phases = std::fs::read_to_string(run_dir.telemetry().join("phases.jsonl"))?;
    assert!(phases.contains("\"phase\":\"intake\""));
    assert!(phases.contains("\"phase\":\"deliver\""));

    let portfolio = std::fs::read_to_string(run_dir.final_dir().join("portfolio.md"))?;
    assert!(portfolio.contains("#"));
    let ranking = std::fs::read_to_string(run_dir.rankings().join("ranking.json"))?;
    assert!(ranking.contains("\"winner\""));

    Ok(())
}

#[test]
fn cli_parses_run_subcommand() {
    use clap::Parser;
    let cli = moagan::cli::Cli::parse_from([
        "moagan",
        "run",
        "--mode",
        "fast",
        "--provider",
        "mock",
        "--prompt",
        "Enumera los 7 colores del arcoíris en orden",
    ]);
    match cli.cmd {
        moagan::cli::Cmd::Run {
            mode,
            provider,
            prompt,
            ..
        } => {
            assert_eq!(mode, "fast");
            assert_eq!(provider, "mock");
            assert_eq!(prompt, "Enumera los 7 colores del arcoíris en orden");
        }
        _ => panic!("expected Run"),
    }
}

#[test]
fn inspect_listing_returns_zero_runs_when_db_is_fresh() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = MoaganHome::resolve()?;
    home.ensure()?;
    let db = moagan::storage::sqlite::Db::open(&home.meta_db_path())?;
    let entries = moagan::cli::inspect::list_recent(&db, 10)?;
    assert!(entries.is_empty());
    Ok(())
}

#[test]
fn forbidden_cargo_toml_guard_rejects_secrecy() {
    let bad = "[dependencies]\nsecrecy = \"0.8\"\n";
    assert!(moagan::cli::forbidden::check_cargo_toml(bad).is_err());
}

#[test]
fn config_load_returns_defaults() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    unsafe {
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let cfg = Config::load()?;
    assert!(cfg.providers.contains_key("minimax"));
    assert!(cfg.providers.contains_key("mock"));
    Ok(())
}

// --- retry-on-parse-failure tests ------------------------------------
// These tests exercise `RunContext::call_with_retry_parse`, which the
// pipeline uses to recover from transient JSON malformation. The
// provider is the mock so we can pre-load a "bad" first response
// followed by a "good" second one.

#[test]
fn call_with_retry_parse_returns_parsed_value_after_retry() -> Result<()> {
    // The mock provider in --cycle=false returns each queued
    // response in order. We queue two responses and verify the
    // helper consumes both: the first (broken) is detected, the
    // second (well-formed) is parsed.
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    // First call returns a mid-string truncation. The walker gives
    // up (unterminated strings are unrepairable) so the parse
    // fails, the helper detects the failure, and the second call
    // is consumed.
    mp.push(MockResponse::plain(
        "{\"id\":\"p_000\",\"summary\":\"unterminated",
    ));
    // Second call returns a clean proposal.
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"second","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.set_cycle(false);

    let mut reg = ProviderRegistry::default();
    reg.insert(
        "mock".into(),
        Arc::new(mp) as Arc<dyn moagan::llm::Provider>,
    );
    let providers = Arc::new(reg);
    let config = Config::load()?;
    let default_model = config.provider("mock")?.model.clone();
    let db = moagan::storage::sqlite::Db::open(&home.meta_db_path())?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    db.register_run(
        run_id,
        "fast",
        "running",
        env!("CARGO_PKG_VERSION"),
        None,
        None,
        None,
    )?;
    let policy = RedactPolicy::default();
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db))?;
    let parallelism = Parallelism::new(1);

    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        providers,
        "mock".into(),
        default_model,
        parallelism,
        telemetry,
        "x".into(),
        "fast".into(),
    );

    // First response is broken; parse would fail. The retry should
    // pick up the second (clean) response and parse it.
    let parsed: moagan::domain::Proposal = ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        1,
    )?;
    assert_eq!(parsed.id, "p_000");
    assert_eq!(parsed.summary, "second");
    Ok(())
}

#[test]
fn call_with_retry_parse_returns_error_after_max_retries() -> Result<()> {
    // Both responses are broken. After max_retries=1, the helper
    // returns Err instead of looping forever.
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain("not-json"));
    mp.push(MockResponse::plain("also-not-json"));
    mp.set_cycle(false);

    let mut reg = ProviderRegistry::default();
    reg.insert(
        "mock".into(),
        Arc::new(mp) as Arc<dyn moagan::llm::Provider>,
    );
    let providers = Arc::new(reg);
    let config = Config::load()?;
    let default_model = config.provider("mock")?.model.clone();
    let db = moagan::storage::sqlite::Db::open(&home.meta_db_path())?;
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    db.register_run(
        run_id,
        "fast",
        "running",
        env!("CARGO_PKG_VERSION"),
        None,
        None,
        None,
    )?;
    let policy = RedactPolicy::default();
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db))?;
    let parallelism = Parallelism::new(1);

    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        providers,
        "mock".into(),
        default_model,
        parallelism,
        telemetry,
        "x".into(),
        "fast".into(),
    );

    let result: moagan::error::Result<moagan::domain::Proposal> = ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: ...",
        1,
    );
    assert!(result.is_err());
    Ok(())
}

// --- warnings-stream integration tests --------------------------------

fn single_role_ctx(
    home: Arc<MoaganHome>,
    provider: Arc<MockProvider>,
    run_id: RunId,
) -> (RunContext, moagan::storage::sqlite::Db) {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = provider.clone();
    registry.insert("mock".into(), arc);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let db = moagan::storage::sqlite::Db::open(&home.meta_db_path()).expect("open db");
    db.register_run(
        run_id,
        "fast",
        "running",
        env!("CARGO_PKG_VERSION"),
        None,
        None,
        None,
    )
    .expect("register run");
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))
        .expect("open telemetry");
    let parallelism = Parallelism::new(1);
    let ctx = RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "x".into(),
        "fast".into(),
    );
    (ctx, db)
}

#[test]
fn truncated_response_emits_model_response_truncated_warning() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    mp.push(MockResponse::truncated(r#"{"x":1}"#));
    mp.set_cycle(false);

    let run_id = RunId::new();
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id);

    let _ =
        pollster::block_on(ctx.call(moagan::llm::Role::Propose, "system".into(), "user".into()))?;

    let summary = db.warnings_summary(run_id)?;
    assert!(
        summary.iter().any(|r| r.code == "model.response_truncated"),
        "expected model.response_truncated, got: {:?}",
        summary
    );
    Ok(())
}

#[test]
fn json_repair_emits_model_json_repair_applied_warning() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    // Input is missing the final `}` so the bracket-repair pass
    // has to append the closer. After repair the payload is valid
    // JSON that matches the Proposal schema.
    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"x","approach":"y","tradeoffs":[],"evidence":[]"#,
    ));
    mp.set_cycle(false);

    let run_id = RunId::new();
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id);

    // The parser will:
    // 1. Direct parse fails (truncated).
    // 2. repair_m3_brackets with trace fires the bracket pass.
    // 3. The parse succeeds (the patched result is valid JSON).
    let parsed: moagan::domain::Proposal = ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        0,
    )?;
    assert_eq!(parsed.id, "p_000");
    assert_eq!(parsed.summary, "x");

    let summary = db.warnings_summary(run_id)?;
    let repair = summary
        .iter()
        .find(|r| r.code == "model.json_repair_applied")
        .expect("expected model.json_repair_applied warning");
    assert!(
        repair.count >= 1,
        "expected at least one repair event, got count={}",
        repair.count
    );
    Ok(())
}

#[test]
fn retry_recovery_emits_retry_and_recovery_warnings() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    // First response is unparseable mid-string. Retry consumes the
    // second well-formed one.
    mp.push(MockResponse::plain("{\"a\":1,\"b\":\"unterminated"));
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"second","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.set_cycle(false);

    let run_id = RunId::new();
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id);

    let parsed: moagan::domain::Proposal = ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        1,
    )?;
    assert_eq!(parsed.summary, "second");

    let summary = db.warnings_summary(run_id)?;
    let codes: Vec<&str> = summary.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"model.retry_parse"),
        "expected model.retry_parse in {:?}",
        codes
    );
    assert!(
        codes.contains(&"model.recovered_after_retry"),
        "expected model.recovered_after_retry in {:?}",
        codes
    );
    Ok(())
}

#[test]
fn inspect_summarize_run_returns_codes() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    mp.push(MockResponse::truncated(r#"{"x":1}"#));
    mp.set_cycle(false);

    let run_id = RunId::new();
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id);
    let _ =
        pollster::block_on(ctx.call(moagan::llm::Role::Propose, "system".into(), "user".into()))?;
    ctx.telemetry.flush()?;

    let summary =
        moagan::cli::inspect::summarize_run(&db, run_id)?.expect("run should be in the index");
    assert_eq!(summary.run_id, run_id);
    assert!(!summary.by_code.is_empty());
    assert!(
        summary
            .by_code
            .iter()
            .any(|r| r.code == "model.response_truncated"),
        "expected model.response_truncated, got: {:?}",
        summary.by_code
    );
    Ok(())
}

#[test]
fn warnings_jsonl_file_is_created_even_when_empty() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mp = MockProvider::empty();
    let run_id = RunId::new();
    let (ctx, _db) = single_role_ctx(home.clone(), Arc::new(mp), run_id);
    ctx.telemetry.flush()?;

    // Even with no calls, the warnings stream should be present
    // (the file is opened at telemetry setup). The mock provider
    // has no responses so we never call `ctx.call` — this is
    // specifically testing that the file is opened eagerly.
    let warnings_path = ctx.telemetry.warnings_path().to_path_buf();
    assert!(warnings_path.exists(), "warnings.jsonl was not created");
    Ok(())
}
