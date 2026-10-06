//! Discover's intake reuses a valid brief and only then skips the model.

use std::sync::Arc;

use super::*;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::ProviderRegistry;
use crate::llm::client::{LlmClient, ScriptedLlmClient};
use crate::test_support::with_moagan_home;

fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn a_brief_with_a_problem_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        "brief.json",
        r#"{"problem":"Catálogo de refacciones","constraints":["C"]}"#,
    );
    assert!(brief_is_valid(&path));
}

#[test]
fn a_missing_blank_or_broken_brief_is_not_valid() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!brief_is_valid(&dir.path().join("brief.json")));
    assert!(!brief_is_valid(&write(
        dir.path(),
        "blank.json",
        r#"{"problem":"  "}"#
    )));
    assert!(!brief_is_valid(&write(dir.path(), "empty.json", "{}")));
    assert!(!brief_is_valid(&write(
        dir.path(),
        "broken.json",
        r#"{"problem":"#
    )));
    std::fs::create_dir(dir.path().join("dir.json")).unwrap();
    assert!(!brief_is_valid(&dir.path().join("dir.json")));
}

#[test]
fn the_phase_keeps_the_intake_name() {
    assert_eq!(DiscoverIntakePhase.name(), "intake");
}

#[test]
fn a_valid_brief_is_reused_without_a_model_call() {
    with_moagan_home("discover-intake-reuse", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        let brief = r#"{"problem":"Catálogo de refacciones","constraints":["Presupuesto fijo"]}"#;
        std::fs::write(run_dir.brief(), brief).unwrap();

        let client = Arc::new(ScriptedLlmClient::empty());
        let dyn_client: Arc<dyn LlmClient> = client.clone();
        let mut registry = ProviderRegistry::default();
        registry.insert("mock".into(), dyn_client);
        let ctx = RunContext::new(
            run_id,
            Arc::new(home.clone()),
            Arc::new(registry),
            "mock".to_owned(),
            "mock-model".to_owned(),
            crate::execution::Parallelism::new(1),
            crate::telemetry::Telemetry::noop(),
            "Diseña el catálogo.".to_owned(),
            "discover".to_owned(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime.block_on(DiscoverIntakePhase.execute(&ctx)).unwrap();
        assert!(matches!(output, PhaseOutput::Intake(ref p) if *p == run_dir.brief()));
        assert_eq!(client.call_count(), 0);
        assert_eq!(std::fs::read_to_string(run_dir.brief()).unwrap(), brief);
        assert!(!run_dir.prompt().exists(), "nothing else is written");
    });
}
