//! End-to-end smoke test for the MVP pipeline with the mock provider.
//!
//! Verifies the gates promised in the plan:
//! 1. `moagan run --mode fast --provider mock` produces
//!    `final/portfolio.md` and `rankings/ranking.json`.
//! 2. The pipeline runs all 10 phases in order.
//! 3. Telemetry records every phase and call.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use moagan::config::Config;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::MockProvider;
use moagan::llm::{MockResponse, Provider, ProviderRegistry, Request, Response, Usage};
use moagan::phases::{
    ClarifyPhase, CritiquePhase, DeliverPhase, GatePhase, IntakePhase, JudgePhase, Phase, Pipeline,
    ProposePhase, RankPhase, RepairPhase, RoutePhase, RunContext, SketchPhase,
};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

/// Process-wide mutex that serialises every test which mutates the
/// `MOAGAN_HOME` / `MOAGAN_CONFIG` environment variables. Without it,
/// the parallel test runner lets one test observe a `MOAGAN_HOME`
/// pointing at a sibling test's already-deleted tempdir, and SQLite
/// open fails with "unable to open database file". Adding the lock is
/// cheaper than restructuring every test to read the env through a
/// shared helper.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the process-wide env mutex and panic on poison (the only
/// way it can be poisoned is if another test panicked mid-mutation,
/// which is already a hard failure).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

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
    let _env = env_lock();
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
    assert_eq!(outputs.len(), 10, "expected 10 phase outputs");

    // Flush telemetry so the gzip stream is finalized before the
    // assertions read it. Without this, the on-disk gzip member has
    // no CRC/length trailer and `MultiGzDecoder` returns
    // `UnexpectedEof`.
    ctx.telemetry.flush()?;

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
        parent_run_id: None,
        shared_brief_hash: None,
        context_refs: Vec::new(),
        lineage_paths: None,
        cli_prompt: None,
        config_hash: None,
        created_at_iso: chrono::Utc::now().to_rfc3339(),
        last_resumed_at_iso: None,
        resume_count: 0,
        prohibited_decisions: Vec::new(),
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
    let phases =
        moagan::storage::compression::read_to_string(&run_dir.telemetry().join("phases.jsonl.gz"))?;
    assert!(
        phases.contains("\"phase\":\"intake\""),
        "phases.jsonl.gz did not contain intake event; raw=\n{phases}"
    );
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
            assert_eq!(mode, moagan::cli::Mode::Fast);
            assert_eq!(provider.as_deref(), Some("mock"));
            assert_eq!(prompt, "Enumera los 7 colores del arcoíris en orden");
        }
        _ => panic!("expected Run"),
    }
}

#[test]
fn inspect_listing_returns_zero_runs_when_db_is_fresh() -> Result<()> {
    let _env = env_lock();
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
fn config_load_returns_defaults() -> Result<()> {
    let _env = env_lock();
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
    let _env = env_lock();
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
    let default_model = config
        .provider("mock")?
        .models
        .first()
        .map(|m| m.id.clone())
        .unwrap_or_default();
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

    // Deep mode: the retry budget permits 2 attempts on parse /
    // schema failures, which is what this test exercises (the
    // first response is broken, the second is well-formed). In
    // fast or standard mode the per-(mode, reason) budget pins
    // the parse-failure cap to 1 attempt because the local JSON
    // repair pass already runs inline inside `parse_model_json`,
    // so the second mock response would never be consumed.
    let ctx = RunContext::new(
        run_id,
        Arc::clone(&home),
        providers,
        "mock".into(),
        default_model,
        parallelism,
        telemetry,
        "x".into(),
        "deep".into(),
    );

    // First response is broken; parse would fail. The retry should
    // pick up the second (clean) response and parse it.
    let parsed: moagan::domain::Proposal = pollster::block_on(ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        1,
    ))?;
    assert_eq!(parsed.id, "p_000");
    assert_eq!(parsed.summary, "second");
    Ok(())
}

#[test]
fn call_with_retry_parse_returns_error_after_max_retries() -> Result<()> {
    // Both responses are broken. After max_retries=1, the helper
    // returns Err instead of looping forever.
    let _env = env_lock();
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
    let default_model = config
        .provider("mock")?
        .models
        .first()
        .map(|m| m.id.clone())
        .unwrap_or_default();
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

    let result: moagan::error::Result<moagan::domain::Proposal> =
        pollster::block_on(ctx.call_with_retry_parse(
            moagan::llm::Role::Propose,
            "system".into(),
            "user".into(),
            "Proposal: ...",
            1,
        ));
    assert!(result.is_err());
    Ok(())
}

// --- warnings-stream integration tests --------------------------------

/// Build a one-role run context for the warnings / retry tests.
/// The `mode` parameter lets the retry tests pick `deep` so the
/// per-(mode, reason) budget permits the 2 attempts on parse /
/// schema failures that those tests exercise; other tests stay
/// on `fast` because they do not rely on the retry budget.
fn single_role_ctx(
    home: Arc<MoaganHome>,
    provider: Arc<MockProvider>,
    run_id: RunId,
    mode: &str,
) -> (RunContext, moagan::storage::sqlite::Db) {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = provider.clone();
    registry.insert("mock".into(), arc);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let db = moagan::storage::sqlite::Db::open(&home.meta_db_path()).expect("open db");
    db.register_run(
        run_id,
        mode,
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
        mode.to_string(),
    );
    (ctx, db)
}

#[test]
fn truncated_response_emits_model_response_truncated_warning() -> Result<()> {
    let _env = env_lock();
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
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "fast");

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
    let _env = env_lock();
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
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "fast");

    // The parser will:
    // 1. Direct parse fails (truncated).
    // 2. repair_m3_brackets with trace fires the bracket pass.
    // 3. The parse succeeds (the patched result is valid JSON).
    let parsed: moagan::domain::Proposal = pollster::block_on(ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        0,
    ))?;
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
    let _env = env_lock();
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
    // Deep mode: the retry budget permits 2 attempts on parse /
    // schema failures, which is what this test exercises (the
    // first response is broken, the second is well-formed).
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "deep");

    let parsed: moagan::domain::Proposal = pollster::block_on(ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        1,
    ))?;
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
    let _env = env_lock();
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
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "fast");
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
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mp = MockProvider::empty();
    let run_id = RunId::new();
    let (ctx, _db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "fast");
    ctx.telemetry.flush()?;

    // Even with no calls, the warnings stream should be present
    // (the file is opened at telemetry setup). The mock provider
    // has no responses so we never call `ctx.call` — this is
    // specifically testing that the file is opened eagerly.
    let warnings_path = ctx.telemetry.warnings_path().to_path_buf();
    assert!(warnings_path.exists(), "warnings.jsonl was not created");
    Ok(())
}

// --- cross-run LLM cache tests -----------------------------------------

#[test]
fn second_identical_call_is_served_from_cache() -> Result<()> {
    // Same prompt twice with the same provider + model. The mock is
    // pre-loaded with two responses; only the first call should reach
    // the provider. The second must come from the cross-run cache.
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"first","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"second","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.set_cycle(false);

    let run_id = RunId::new();
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "fast");

    let r1 =
        pollster::block_on(ctx.call(moagan::llm::Role::Propose, "system".into(), "user".into()))?;
    assert!(r1.text.contains("\"first\""));

    let r2 =
        pollster::block_on(ctx.call(moagan::llm::Role::Propose, "system".into(), "user".into()))?;
    // Cache hit: must return the first response, not the second one
    // the mock would otherwise have returned.
    assert!(r2.text.contains("\"first\""));
    assert!(!r2.text.contains("\"second\""));

    // Both calls are recorded in SQLite, the second with cache_hit=1.
    let calls =
        moagan::storage::sqlite::Db::open(&home.meta_db_path())?.list_calls_for_run(run_id)?;
    assert_eq!(calls.len(), 2);
    let misses = calls.iter().filter(|c| c.cache_hit == 0).count();
    let hits = calls.iter().filter(|c| c.cache_hit == 1).count();
    assert_eq!(misses, 1, "exactly one miss expected");
    assert_eq!(hits, 1, "exactly one hit expected");
    assert!(
        !calls[0].cache_key.is_empty(),
        "cache_key must be populated"
    );
    assert_eq!(calls[0].cache_key, calls[1].cache_key);

    // And a JSONL entry is written for the hit.
    ctx.telemetry.flush()?;
    let calls_jsonl = moagan::storage::compression::read_to_string(ctx.telemetry.calls_path())?;
    assert!(calls_jsonl.contains("\"cache_hit\":true"));
    let _ = db;
    Ok(())
}

#[test]
fn prompt_cache_short_circuits_identical_call() -> Result<()> {
    // PR-06 / D.6.4: two `Role::Propose` invocations with identical
    // `(role, system, user)` must result in exactly one HTTP call.
    // The second invocation short-circuits through `PromptCache`
    // without recomputing the content-hash lookup. The mock is
    // pre-loaded with two responses so a regression that dropped the
    // cache (and let the second call reach the provider) would be
    // caught by the response-text assertion as well as the calls
    // table.
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"first","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"second","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.set_cycle(false);

    let run_id = RunId::new();
    let (ctx, db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "fast");

    let r1 =
        pollster::block_on(ctx.call(moagan::llm::Role::Propose, "system".into(), "user".into()))?;
    assert!(r1.text.contains("\"first\""));

    let r2 =
        pollster::block_on(ctx.call(moagan::llm::Role::Propose, "system".into(), "user".into()))?;
    // Cache hit on the second call: must return the first response,
    // not the second one the mock would have served. If the
    // PromptCache wiring regressed, r2.text would contain "second".
    assert!(r2.text.contains("\"first\""));
    assert!(!r2.text.contains("\"second\""));

    // SQLite records both calls, the second with cache_hit=1.
    let calls =
        moagan::storage::sqlite::Db::open(&home.meta_db_path())?.list_calls_for_run(run_id)?;
    assert_eq!(calls.len(), 2);
    let misses = calls.iter().filter(|c| c.cache_hit == 0).count();
    let hits = calls.iter().filter(|c| c.cache_hit == 1).count();
    assert_eq!(misses, 1, "exactly one miss expected (real HTTP call)");
    assert_eq!(hits, 1, "exactly one hit expected (PromptCache shortcut)");
    assert_eq!(calls[0].cache_key, calls[1].cache_key);

    // JSONL mirrors: both calls produce a record, but only the
    // first one carries the real http_status from the provider.
    // The role-scoped invariant from the PR-06 verification rule
    // ("only 1 entry per role+prompt_id in calls.jsonl.gz") is
    // expressed as: exactly one row where cache_hit=false.
    ctx.telemetry.flush()?;
    let calls_jsonl = moagan::storage::compression::read_to_string(ctx.telemetry.calls_path())?;
    let role_propose_lines = calls_jsonl
        .lines()
        .filter(|line| line.contains("\"role\":\"propose\""))
        .collect::<Vec<_>>();
    assert_eq!(
        role_propose_lines.len(),
        2,
        "two call records expected for Role::Propose"
    );
    let propose_misses = role_propose_lines
        .iter()
        .filter(|line| line.contains("\"cache_hit\":false"))
        .count();
    assert_eq!(
        propose_misses, 1,
        "exactly one row with cache_hit=false for Role::Propose"
    );

    // After the second call, the PromptCache index must contain the
    // (role, cache_key) mapping that was registered on the first
    // call. Without it, the next lookup_by_id would miss and the
    // index would be silently bypassed.
    let expected_prompt_id = format!("propose@{}", calls[0].cache_key);
    assert!(
        ctx.prompt_cache
            .lock()
            .lookup_by_id(&expected_prompt_id)
            .is_some(),
        "PromptCache index must be populated after a successful call"
    );

    let _ = db;
    Ok(())
}

#[test]
fn retry_on_parse_failure_bypasses_cache() -> Result<()> {
    // Regression: if the first response is cached and broken, the
    // retry would otherwise keep returning the same broken response.
    // `call_with_retry_parse` must bypass the cache on retries.
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain("{\"a\":1,\"b\":\"unterminated"));
    mp.push(MockResponse::plain(
        r#"{"id":"p_000","summary":"second","approach":"","tradeoffs":[],"evidence":[]}"#,
    ));
    mp.set_cycle(false);

    let run_id = RunId::new();
    // Deep mode: the retry budget permits 2 attempts on parse /
    // schema failures, which is what this test exercises (the
    // first response is broken, the second is well-formed).
    let (ctx, _db) = single_role_ctx(home.clone(), Arc::new(mp), run_id, "deep");

    let parsed: moagan::domain::Proposal = pollster::block_on(ctx.call_with_retry_parse(
        moagan::llm::Role::Propose,
        "system".into(),
        "user".into(),
        "Proposal: {id, summary, approach, tradeoffs[], evidence[]}",
        1,
    ))?;
    assert_eq!(parsed.summary, "second");
    Ok(())
}

/// Build a mock provider pre-loaded with the v0.2 deep-mode call
/// sequence: 1 intake + 1 clarify + 1 route + 6 sketches + 5
/// proposals + 20 critiques (5 props × 4 critics) + 35 judge
/// evaluations (5 props × 7 judges) + 1 deliver = 69 calls. Each
/// `MockResponse` ships a payload that round-trips through the
/// role-specific domain type so `SketchPhase`'s filter and the
/// `ProposePhase::source_sketch` pairing both see real artefacts.
fn build_deep_mock_provider() -> std::sync::Arc<MockProvider> {
    use moagan::domain::Sketch;
    let mut p = MockProvider::empty();
    p.push(MockResponse::plain(intake_json()));
    p.push(MockResponse::plain(clarify_json()));
    p.push(MockResponse::plain(route_json()));
    // E5 (catalog 10-integrada-v0 §D.5.3): each sketch carries a
    // meaningfully different thesis + outline / strengths /
    // weaknesses / validation so the SketchPhase redundancy
    // filter (jaccard >= 0.85) does not collapse them onto a
    // single survivor. Earlier the loop produced 6 sketches
    // that differed only by an integer in the thesis text;
    // E5 legitimately rejected 5 of them.
    let distinct_theses = [
        // The deep-mode mock brief is about enumerating rainbow
        // colours; the six distinct theses line up with the
        // standard ROYGBIV enumeration so the E5 coverage check
        // (`>= 0.5` token overlap with the brief) keeps all of
        // them. Earlier versions of this fixture used
        // Rust-themed theses that shared 0 tokens with the
        // rainbow brief and E5 collapsed them onto a single
        // survivor.
        "Sketch 0 enumerates the seven rainbow colours in standard ROYGBIV order with no commentary.",
        "Sketch 1 lists the colours as a table with a single short caption beneath each name.",
        "Sketch 2 emits the canonical order, then prints a one-line mnemonic to lock the answer in memory.",
        "Sketch 3 picks an acrostic that walks the order forward and backward to defend against off-by-one.",
        "Sketch 4 spells the colours in upper case on one line and lower case on the next for emphasis.",
        "Sketch 5 cites a single dictionary definition to back each standard colour name in the answer.",
    ];
    let distinct_outlines = [
        "single text block; one column; ROYGBIV in plain prose",
        "two columns; colour name + short hint",
        "single paragraph; mnemonic footnote at the end",
        "stanza form; forward and reverse acrostic",
        "two lines; upper case then lower case; nothing else",
        "dictionary cite per line; rest of the answer stays terse",
    ];
    for i in 0..6 {
        let sk = Sketch {
            id: format!("sk_{i:03}"),
            thesis: distinct_theses[i].into(),
            key_decisions: vec![format!("d{i}-1"), format!("d{i}-2")],
            architecture_outline: distinct_outlines[i].into(),
            assumptions: vec![format!("assumption {i}")],
            strengths: vec![format!("strength {i}")],
            weaknesses: vec![format!("weakness {i}")],
            hard_constraint_check: [("no_serverless".to_string(), true)].into_iter().collect(),
            expected_validation: format!("smoke test {i}"),
            angle: format!("angle-{i}"),
        };
        p.push(MockResponse::plain(serde_json::to_string(&sk).unwrap()));
    }
    for i in 0..5 {
        p.push(MockResponse::plain(propose_json(&format!("p_{i:03}"))));
    }
    for _ in 0..20 {
        p.push(MockResponse::plain(critique_json()));
    }
    for _ in 0..35 {
        p.push(MockResponse::plain(judge_json()));
    }
    p.push(MockResponse::plain(deliver_json()));
    p.set_cycle(false);
    std::sync::Arc::new(p)
}

/// End-to-end smoke for `--mode deep` with the mock provider. The
/// pipeline now has 11 phases (sketch inserted between route and
/// propose). We assert the new sidecars exist and that each
/// proposal carries the `source_sketch` lineage field populated.
#[test]
fn deep_mode_pipeline_persists_sketches_and_proposals() -> Result<()> {
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    let provider = build_deep_mock_provider();
    let run_id = RunId::new();
    let ctx = build_run_context(home.clone(), provider, run_id);

    let pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(SketchPhase { count: 6 })
        .push(ProposePhase { count: 5 })
        .push(GatePhase)
        .push(CritiquePhase {
            critics_per_proposal: 4,
        })
        .push(RepairPhase::default())
        .push(JudgePhase {
            judges: 7,
            ..JudgePhase::default()
        })
        .push(RankPhase {
            config: Arc::new(Config::default()),
            replace_sources_enabled: false,
            stability_enabled: false,
        })
        .push(DeliverPhase);

    let outputs = pollster::block_on(pipeline.run(&ctx))?;
    assert_eq!(outputs.len(), 11, "deep mode runs 11 phases");

    ctx.telemetry.flush()?;

    let run_dir = home.run_dir(run_id);

    // Six sketch sidecars survived the filter.
    let sketch_files: Vec<_> = std::fs::read_dir(run_dir.sketches())?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && !p.to_string_lossy().ends_with(".meta.json")
        })
        .collect();
    assert_eq!(sketch_files.len(), 6, "expected 6 sketch artefacts");

    let summary_path = run_dir.final_dir().join("sketches_summary.json");
    let summary_text = std::fs::read_to_string(&summary_path)?;
    let summary: serde_json::Value = serde_json::from_str(&summary_text)?;
    assert_eq!(summary["raw"], 6);
    assert_eq!(summary["kept"], 6);
    assert_eq!(summary["dropped_empty_thesis"], 0);
    assert_eq!(summary["dropped_hard_constraint"], 0);

    // Each proposal carries a `source_sketch` pointing at the i-th
    // sketch.
    for i in 0..5 {
        let p_path = run_dir.proposals().join(format!("p_{i:03}.json"));
        let p_text = std::fs::read_to_string(&p_path)?;
        let p: serde_json::Value = serde_json::from_str(&p_text)?;
        let expected = format!("sk_{i:03}");
        assert_eq!(
            p["source_sketch"].as_str(),
            Some(expected.as_str()),
            "proposal {i} should point at sketch {expected}, got {}",
            p["source_sketch"]
        );
    }

    // Portfolio and ranking written by deliver.
    assert!(run_dir.final_dir().join("portfolio.md").exists());
    assert!(run_dir.rankings().join("ranking.json").exists());

    Ok(())
}

/// `explore` ends at sketches — no proposals, no judging, no
/// deliver. The pipeline returns 4 phase outputs and persists
/// 12 sketch sidecars without ever touching `proposals/`.
#[test]
fn explore_mode_pipeline_terminates_at_sketches() -> Result<()> {
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    // Build a mock provider with 1 intake + 1 clarify + 1 route + 12
    // sketches; nothing else needs to be queued because explore ends
    // at the sketch phase.
    use moagan::domain::Sketch;
    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain(intake_json()));
    mp.push(MockResponse::plain(clarify_json()));
    mp.push(MockResponse::plain(route_json()));
    // E5 (catalog 10-integrada-v0 §D.5.3): each of the 12
    // explore-mode sketches carries a unique thesis so the
    // SketchPhase redundancy filter (jaccard >= 0.85) keeps the
    // whole batch. Earlier the loop produced 12 sketches that
    // differed only by an integer in the thesis text; E5
    // legitimately collapsed them onto a single survivor.
    // E5 (catalog 10-integrada-v0 §D.5.3): each of the 12
    // explore-mode sketches covers a different angle of the same
    // rainbow-colour brief so the SketchPhase redundancy filter
    // (jaccard >= 0.85) and coverage filter (token overlap >=
    // 0.5) keep the whole batch. Earlier the loop produced 12
    // sketches that differed only by an integer in the thesis
    // text and shared no tokens with the brief; E5 collapsed
    // them onto a single survivor.
    let explore_theses = [
        "Sketch 0 lists the rainbow colours in the canonical ROYGBIV order with no commentary.",
        "Sketch 1 prints the colours in a column with a one-word label beside each.",
        "Sketch 2 emits the order as a single sentence and a memory hook for review.",
        "Sketch 3 picks an acrostic that walks forward and backward through the seven colours.",
        "Sketch 4 spells the standard names in upper case on one line and lower case on the next.",
        "Sketch 5 cites a single dictionary definition for each colour name in the answer.",
        "Sketch 6 groups the colours into warm and cool halves and prints them in two blocks.",
        "Sketch 7 tabulates the wavelength of each colour from a published reference table.",
        "Sketch 8 mixes the order to argue that the modern mnemonic is just one of many.",
        "Sketch 9 anchors each colour to a Roy Lichtenstein print whose palette uses that hue.",
        "Sketch 10 reads the colours aloud as if dictating to a recording for archival use.",
        "Sketch 11 pairs each colour with the flag that uses it most prominently in the G7 set.",
    ];
    for (i, thesis) in explore_theses.iter().enumerate() {
        let sk = Sketch {
            id: format!("sk_{i:03}"),
            thesis: (*thesis).into(),
            key_decisions: vec![format!("d{i}-1")],
            architecture_outline: format!("outline {i}"),
            assumptions: vec![],
            strengths: vec![format!("s{i}")],
            weaknesses: vec![format!("w{i}")],
            hard_constraint_check: std::collections::BTreeMap::new(),
            expected_validation: format!("ev {i}"),
            angle: format!("angle-{i}"),
        };
        mp.push(MockResponse::plain(serde_json::to_string(&sk).unwrap()));
    }
    mp.set_cycle(false);

    let provider = Arc::new(mp);
    let run_id = RunId::new();
    let ctx = build_run_context(home.clone(), provider, run_id);

    let pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(SketchPhase { count: 12 });

    let outputs = pollster::block_on(pipeline.run(&ctx))?;
    assert_eq!(
        outputs.len(),
        4,
        "explore mode runs exactly 4 phases (intake, clarify, route, sketch)"
    );

    ctx.telemetry.flush()?;

    let run_dir = home.run_dir(run_id);
    let sketch_files: Vec<_> = std::fs::read_dir(run_dir.sketches())?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && !p.to_string_lossy().ends_with(".meta.json")
        })
        .collect();
    assert_eq!(sketch_files.len(), 12);
    assert!(
        !run_dir.proposals().exists() || std::fs::read_dir(run_dir.proposals())?.next().is_none(),
        "explore must not produce proposals"
    );

    Ok(())
}

/// D.17.7: the sketch phase emits `telemetry/sketches_summary.csv`
/// alongside the existing JSON summary. The helper signature is
/// per-model aggregation (`model,sketch_count,total_tokens`), so a
/// single row is produced for the whole fan-out because every sketch
/// is generated through the same `default_model` provider.
///
/// `explore` is the smallest sketch-producing pipeline that still
/// exercises the full `intake → clarify → route → sketch` flow with
/// the mock provider.
#[test]
fn sketch_phase_emits_csv_summary() -> Result<()> {
    let _env = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;

    use moagan::domain::Sketch;
    let mut mp = MockProvider::empty();
    mp.push(MockResponse::plain(intake_json()));
    mp.push(MockResponse::plain(clarify_json()));
    // The route decision carries `sketches: 4` so the explore-mode
    // pipeline variant below sees `count = 4` (the smallest
    // non-zero sketch count in the mode table).
    let route_sketch4 = r#"{
  "mode": "explore",
  "reason": "Force a small sketch fan-out to exercise the CSV writer",
  "sketches": 4,
  "proposals": 0,
  "judges": 0
}"#;
    mp.push(MockResponse::plain(route_sketch4));
    let theses = [
        "Sketch 0 lists the rainbow colours in canonical ROYGBIV order with no commentary.",
        "Sketch 1 prints the colours in a column with a one-word label beside each name.",
        "Sketch 2 emits the order as a single sentence plus a memory hook for later review.",
        "Sketch 3 tabulates the wavelength of each colour from a published reference table.",
    ];
    for (i, thesis) in theses.iter().enumerate() {
        let sk = Sketch {
            id: format!("sk_{i:03}"),
            thesis: (*thesis).into(),
            key_decisions: vec![format!("d{i}-1")],
            architecture_outline: format!("outline {i}"),
            assumptions: vec![],
            strengths: vec![format!("s{i}")],
            weaknesses: vec![format!("w{i}")],
            hard_constraint_check: std::collections::BTreeMap::new(),
            expected_validation: format!("ev {i}"),
            angle: format!("angle-{i}"),
        };
        mp.push(MockResponse::plain(serde_json::to_string(&sk).unwrap()));
    }
    mp.set_cycle(false);

    let provider = Arc::new(mp);
    let run_id = RunId::new();
    let ctx = build_run_context(home.clone(), provider, run_id);

    let pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(SketchPhase { count: 4 });

    pollster::block_on(pipeline.run(&ctx))?;
    ctx.telemetry.flush()?;

    let run_dir = home.run_dir(run_id);
    let csv_path = run_dir.telemetry().join("sketches_summary.csv");
    assert!(
        csv_path.exists(),
        "sketches_summary.csv must exist at {}",
        csv_path.display()
    );
    let csv_text = std::fs::read_to_string(&csv_path)?;
    let mut lines = csv_text.lines();
    assert_eq!(
        lines.next(),
        Some("model,sketch_count,total_tokens"),
        "CSV must start with the documented header"
    );
    // The helper emits one row per model. All sketches in a single
    // phase use `ctx.default_model` ("mock-model" in this test), so
    // we expect exactly one data row.
    let row = lines
        .next()
        .expect("expected at least one data row after the header");
    let mut fields = row.split(',');
    assert_eq!(fields.next(), Some("mock-model"));
    assert_eq!(
        fields.next(),
        Some("4"),
        "row must report the four kept sketches"
    );
    // `total_tokens` is left at zero by the wire-up (see the comment
    // in `SketchPhase::execute`). Pin the contract here so a future
    // refactor that DOES populate it cannot silently break the
    // shape of the file.
    assert_eq!(fields.next(), Some("0"));
    assert!(
        lines.next().is_none(),
        "no further rows expected; got {:?}",
        lines.collect::<Vec<_>>()
    );

    // The empty-CSV branch (count == 0) must still emit the file
    // with just the header. Production `build_pipeline_for_mode`
    // omits `SketchPhase` entirely when `mode.runs_sketches()` is
    // false (`fast`); here we explicitly insert `SketchPhase { count: 0 }`
    // to drive the early-return branch directly.
    let tmp2 = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp2.path());
    }
    let home2 = Arc::new(MoaganHome::resolve()?);
    home2.ensure()?;
    let mut mp2 = MockProvider::empty();
    mp2.push(MockResponse::plain(intake_json()));
    mp2.push(MockResponse::plain(clarify_json()));
    mp2.push(MockResponse::plain(route_json())); // "sketches": 0
    mp2.set_cycle(false);
    let mp2 = Arc::new(mp2);
    let run_id2 = RunId::new();
    let ctx2 = build_run_context(home2.clone(), mp2, run_id2);

    let empty_pipeline = Pipeline::new()
        .push(IntakePhase)
        .push(ClarifyPhase)
        .push(RoutePhase)
        .push(SketchPhase { count: 0 });
    pollster::block_on(empty_pipeline.run(&ctx2))?;
    ctx2.telemetry.flush()?;
    let fast_csv = home2
        .run_dir(run_id2)
        .telemetry()
        .join("sketches_summary.csv");
    assert!(
        fast_csv.exists(),
        "SketchPhase{{count: 0}} must still emit the CSV (header-only)"
    );
    let fast_text = std::fs::read_to_string(&fast_csv)?;
    let mut fast_lines = fast_text.lines();
    assert_eq!(fast_lines.next(), Some("model,sketch_count,total_tokens"));
    assert!(
        fast_lines.next().is_none(),
        "count == 0 must produce a header-only CSV"
    );

    Ok(())
}

#[derive(Debug)]
struct DelayedJudgeProvider {
    active: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
    delay: Duration,
}

impl DelayedJudgeProvider {
    fn new(delay: Duration) -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            delay,
        }
    }
}

#[async_trait]
impl Provider for DelayedJudgeProvider {
    fn name(&self) -> &str {
        "delayed"
    }

    fn model(&self) -> &str {
        "delayed-model"
    }

    fn endpoint(&self) -> &str {
        "delayed://local"
    }

    async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok((
            200,
            Response {
                text: judge_json().to_owned(),
                finish_reason: Some("end_turn".into()),
                truncated: false,
                usage: Usage::default(),
            },
        ))
    }
}

fn judge_context(
    home: Arc<MoaganHome>,
    provider: Arc<dyn Provider>,
    provider_name: &str,
    model: &str,
    run_id: RunId,
    max_parallelism: usize,
) -> RunContext {
    let mut registry = ProviderRegistry::default();
    registry.insert(provider_name.into(), provider);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("open telemetry");
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        provider_name.into(),
        model.into(),
        Parallelism::new(max_parallelism),
        telemetry,
        "judge concurrency".into(),
        "deep".into(),
    )
}

fn seed_judge_proposals(home: &MoaganHome, run_id: RunId, count: usize) -> Result<()> {
    let proposals_dir = home.run_dir(run_id).proposals();
    std::fs::create_dir_all(&proposals_dir)?;
    for i in 0..count {
        let proposal = moagan::domain::Proposal {
            id: format!("p_{i:03}"),
            summary: format!("Proposal {i}"),
            approach: "A concrete approach".into(),
            tradeoffs: vec!["One tradeoff".into()],
            evidence: vec!["One source".into()],
            source_sketch: String::new(),
            artifacts: vec![],
            replaced_by: None,
            source_nodes: Vec::new(),
        };
        let bytes = serde_json::to_vec(&proposal)?;
        std::fs::write(proposals_dir.join(format!("p_{i:03}.json")), bytes)?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn judge_phase_respects_parallelism_cap() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
    home.ensure()?;
    let run_id = RunId::new();
    seed_judge_proposals(&home, run_id, 5)?;
    let provider = Arc::new(DelayedJudgeProvider::new(Duration::from_millis(20)));
    let ctx = judge_context(
        home.clone(),
        provider.clone(),
        "delayed",
        "delayed-model",
        run_id,
        4,
    );

    let output = JudgePhase {
        judges: 7,
        ..JudgePhase::default()
    }
    .execute(&ctx)
    .await?;
    let moagan::phases::PhaseOutput::Evaluations(paths) = output else {
        panic!("expected evaluations");
    };

    assert_eq!(provider.calls.load(Ordering::SeqCst), 35);
    assert!(provider.peak.load(Ordering::SeqCst) <= 4);
    assert_eq!(paths.len(), 5);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn judge_phase_completes_thirty_five_http_calls() -> Result<()> {
    use moagan::config::ProviderConfig;
    use moagan::llm::minimax::MinimaxProvider;
    use moagan::secret::SecretString;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let template = ResponseTemplate::new(200)
        .set_delay(Duration::from_millis(10))
        .set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": judge_json()}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        }));
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .respond_with(template)
        .mount(&server)
        .await;

    let spec = ProviderConfig {
        models: vec![moagan::config::ModelConfig {
            id: "MiniMax-M3".to_owned(),
            // v0.10 schema: the dispatcher picks the wire endpoint
            // off `models[].endpoint`, not the section-level
            // `endpoint`. The wiremock URL has to live on the
            // model entry so `MinimaxProvider::new` picks it up
            // when it reads `first.endpoint`.
            endpoint: Some(format!("{}/anthropic/v1", server.uri())),
            max_tokens: None,
        }],
        endpoint: None,
        temperature: None,
        top_p: None,
        omit_max_tokens: false,
        max_token_auto: None,
        max_token_auto_save: true,
        plan: None,
    };
    let provider: Arc<dyn Provider> = Arc::new(MinimaxProvider::new(
        &spec,
        SecretString::new("test-key".into()),
    )?);
    let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
    home.ensure()?;
    let run_id = RunId::new();
    seed_judge_proposals(&home, run_id, 5)?;
    let ctx = judge_context(home.clone(), provider, "minimax", "MiniMax-M3", run_id, 4);

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        JudgePhase {
            judges: 7,
            ..JudgePhase::default()
        }
        .execute(&ctx),
    )
    .await
    .map_err(|_| moagan::Error::Timeout {
        message: "local HTTP judge test".into(),
        http_status: None,
    })??;
    let moagan::phases::PhaseOutput::Evaluations(paths) = output else {
        panic!("expected evaluations");
    };
    ctx.telemetry.flush()?;

    assert_eq!(server.received_requests().await.unwrap().len(), 35);
    assert_eq!(paths.len(), 5);
    let calls = moagan::storage::compression::read_to_string(ctx.telemetry.calls_path())?;
    assert_eq!(calls.lines().count(), 35);
    Ok(())
}
