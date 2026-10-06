//! Discover integration checks that need the public crate API: the
//! quality error's wire shape, and the sketch phase's retry telemetry.

use std::io::Read;
use std::sync::Arc;

use moagan::config::Config;
use moagan::error::Error;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::client::{LlmClient, MockClient};
use moagan::llm::{MockResponse, ProviderRegistry};
use moagan::phases::{DiscoverSketchesPhase, Phase, PhaseOutput, RunContext};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

#[test]
fn error_discovery_quality_too_low_serializes_with_counts() {
    let err = Error::DiscoveryQualityTooLow {
        failed: 6,
        total: 10,
        threshold_pct: 50,
    };
    // Display form carries the counts so logs / telemetry surfaces
    // the numbers without needing the structured payload.
    let s = err.to_string();
    assert!(s.contains("6"), "display must include failed count: {s}");
    assert!(s.contains("10"), "display must include total: {s}");
    assert!(s.contains("50"), "display must include threshold: {s}");

    // Exit code is the ContextError bucket (80) so CI scripts branch
    // the same way as for the existing "zero sketches" abort.
    assert_eq!(err.exit_code(), moagan::error::ExitCode::ContextError);
    // The stable wire code stays inside the InvalidState bucket.
    assert_eq!(
        err.code().stable(),
        "INVALID_STATE",
        "DiscoveryQualityTooLow must map to INVALID_STATE"
    );
}

/// Valid sketch the mock returns after one unparseable answer.
fn valid_sketch() -> String {
    serde_json::json!({
        "id": "model-chosen",
        "thesis": "Ship a single Rust binary that bundles config, embed, and runtime.",
        "key_decisions": ["static link", "rust runtime"],
        "architecture_outline": "One binary implements every phase and persists artefacts under MOAGAN_HOME.",
        "assumptions": ["Linux + macOS only"],
        "strengths": ["easy install"],
        "weaknesses": ["larger binary"],
        "hard_constraint_check": {"C1": true},
        "expected_validation": "A fresh container rebuilds the suite from one tarball."
    })
    .to_string()
}

/// Every row of `calls.jsonl.gz` (an appended multi-member gzip).
fn call_rows(path: &std::path::Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A sketch whose first answer does not parse is retried uncached; each
/// attempt is its own `calls` row with `retry_count` 0, 1 and a fresh
/// `call_id`, and the sketch is written under its fan-out id.
#[tokio::test]
async fn a_retried_sketch_records_one_call_row_per_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    std::fs::write(
        run_dir.root().join("prompt.md"),
        "Design a single-binary CLI.",
    )
    .unwrap();
    let brief = serde_json::json!({
        "problem": "Design a single-binary CLI",
        "constraints": ["Single Rust binary"]
    });
    std::fs::write(run_dir.brief(), brief.to_string()).unwrap();

    let mut mock = MockClient::empty();
    mock.push(MockResponse::plain("not-json-at-all"));
    mock.push(MockResponse::plain(valid_sketch()));
    mock.set_cycle(false);
    let mock = Arc::new(mock);
    let client: Arc<dyn LlmClient> = mock.clone();
    let mut registry = ProviderRegistry::default();
    registry.insert("mock".to_owned(), client);
    let mut cfg = Config::default();
    cfg.discovery_matrix.matrix_spec = vec!["a=x".to_owned()];
    cfg.discovery_matrix.sketches_per_cell = 1;
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).unwrap();
    let ctx = RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        telemetry,
        "Design a single-binary CLI.".into(),
        "discover".into(),
        Arc::new(cfg),
    );

    let output = DiscoverSketchesPhase.execute(&ctx).await.unwrap();
    ctx.telemetry.flush().unwrap();

    assert_eq!(mock.calls().len(), 2, "1 broken answer + 1 valid retry");
    let rows = call_rows(ctx.telemetry.calls_path());
    let retry_counts: Vec<u64> = rows
        .iter()
        .map(|r| r["retry_count"].as_u64().unwrap())
        .collect();
    assert_eq!(retry_counts, [0, 1]);
    let call_ids: std::collections::BTreeSet<&str> = rows
        .iter()
        .map(|r| r["call_id"].as_str().unwrap())
        .collect();
    assert_eq!(call_ids.len(), 2, "each attempt has its own call_id");

    let PhaseOutput::Sketches(paths) = output else {
        panic!("expected PhaseOutput::Sketches");
    };
    assert_eq!(paths.len(), 1);
    let sketch: moagan::domain::Sketch = moagan::phases::util::read_json(&paths[0]).unwrap();
    assert_eq!(sketch.id, "sk_0000");
    assert_eq!(
        sketch.thesis,
        "Ship a single Rust binary that bundles config, embed, and runtime."
    );
}
