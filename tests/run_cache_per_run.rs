//! `moagan run` caches its theses and proposals per run: a later run
//! with the same prompt asks the upstream again, while the other roles
//! still answer an identical payload from the cross-run cache.

use std::sync::Arc;

use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::client::{LlmClient, MockClient};
use moagan::llm::{MockResponse, ProviderRegistry, Role};
use moagan::phases::RunContext;
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// A run whose mock upstream answers every call with `answer`.
fn run_answering(home: Arc<MoaganHome>, answer: &str) -> RunContext {
    let mut mock = MockClient::empty();
    mock.push(MockResponse::plain(answer));
    let mut registry = ProviderRegistry::default();
    let client: Arc<dyn LlmClient> = Arc::new(mock);
    registry.insert("mock".into(), client);
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let db = moagan::storage::sqlite::Db::open(&home.meta_db_path()).expect("open db");
    db.register_run(
        run_id,
        "standard",
        "running",
        env!("CARGO_PKG_VERSION"),
        None,
        None,
        None,
    )
    .expect("register run");
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db))
        .expect("open telemetry");
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        telemetry,
        "x".into(),
        "standard".to_string(),
    )
}

/// What a second run with the same prompt gets for `role` after a
/// first run answered it with `"first"`.
fn second_run_answer(role: Role) -> Result<String> {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
        std::env::set_var("MOAGAN_CONFIG", "/dev/null/moagan-test-config");
    }
    let home = Arc::new(MoaganHome::resolve()?);
    home.ensure()?;
    let first = run_answering(home.clone(), r#"{"summary":"first"}"#);
    pollster::block_on(first.call(role, "system".into(), "user".into()))?;
    let second = run_answering(home, r#"{"summary":"second"}"#);
    let answer = pollster::block_on(second.call(role, "system".into(), "user".into()))?;
    Ok(answer.text)
}

#[test]
fn a_second_run_asks_the_upstream_for_its_proposals_again() -> Result<()> {
    let _env = env_lock();
    assert!(second_run_answer(Role::Propose)?.contains("second"));
    Ok(())
}

#[test]
fn a_second_run_asks_the_upstream_for_its_theses_again() -> Result<()> {
    let _env = env_lock();
    assert!(second_run_answer(Role::Thesis)?.contains("second"));
    Ok(())
}

#[test]
fn a_second_run_still_answers_a_critique_from_the_cross_run_cache() -> Result<()> {
    let _env = env_lock();
    assert!(second_run_answer(Role::Critique)?.contains("first"));
    Ok(())
}
