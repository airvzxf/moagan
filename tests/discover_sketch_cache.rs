//! The sketch answers are cached per run (#1030).
//!
//! A second discover run started from the first run's brief sends the
//! same sketch payloads; it must still ask the upstream for every
//! sketch. `moagan continue` on the first run reuses that run's own
//! cached answers.

use std::path::{Path, PathBuf};

use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// One answer that parses as an intake brief and as a sketch.
fn canned_answer() -> String {
    json!({
        "problem": "Inventory for a small bike shop",
        "objectives": ["Find any part in seconds"],
        "constraints": ["Minimal budget"],
        "non_goals": ["Invoicing"],
        "open_questions": [],
        "raw_prompt": "Design the inventory",
        "id": "x",
        "thesis": "Keep the inventory in one shared spreadsheet with one row per part and a bin code.",
        "key_decisions": ["spreadsheet"],
        "architecture_outline": "One sheet, one row per part, bin codes printed on labels.",
        "assumptions": ["one operator"],
        "strengths": ["cheap"],
        "weaknesses": ["no history"],
        "hard_constraint_check": {"C1": true},
        "expected_validation": "Time ten lookups of random parts."
    })
    .to_string()
}

async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": canned_answer()}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    server
}

fn moagan(server: &MockServer, home: &Path, work: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(binary());
    cmd.current_dir(work)
        .env("MOAGAN_HOME", home)
        .env("MINIMAX_API_KEY", "test-key")
        .env(
            "MOAGAN_MINIMAX_ENDPOINT",
            format!("{}/anthropic/v1/messages", server.uri()),
        )
        .env("XDG_CONFIG_HOME", work.join("xdg"))
        .env("MOAGAN_QUIET", "1")
        .env("MOAGAN_MAX_TOKEN_AUTO", "false")
        .env("MOAGAN_TEMPERATURE_AUTO", "false")
        .env("MOAGAN_TOP_P_AUTO", "false")
        .env("MOAGAN_TOP_K_AUTO", "false")
        .env_remove("MOAGAN_CONFIG");
    cmd
}

fn discover_args(extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "discover",
        "--non-interactive",
        "--provider",
        "minimax:MiniMax-M3",
        "--prompt",
        "Design the inventory",
        "--matrix-spec",
        "storage=engine,backup",
        "--sketches-per-cell",
        "1",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    args
}

/// Sketch requests the upstream received, from request `skip` on.
async fn sketch_requests(server: &MockServer, skip: usize) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .skip(skip)
        .filter(|r| String::from_utf8_lossy(&r.body).contains("Exploration cell:"))
        .count()
}

fn run_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(home.join(".runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_run_from_the_same_brief_asks_the_upstream_for_every_sketch() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = moagan(&server, &home, tmp.path())
        .args(discover_args(&[]))
        .output()
        .unwrap();
    assert_ok(&out, "first run");
    assert_eq!(sketch_requests(&server, 0).await, 2, "one sketch per cell");
    let first = run_dirs(&home).remove(0);

    let before = server.received_requests().await.unwrap().len();
    let brief = first.join("brief.json");
    let out = moagan(&server, &home, tmp.path())
        .args(discover_args(&["--brief", brief.to_str().unwrap()]))
        .output()
        .unwrap();
    assert_ok(&out, "second run");
    assert_eq!(run_dirs(&home).len(), 2, "two runs");
    assert_eq!(
        sketch_requests(&server, before).await,
        2,
        "the second run's sketches are not answered from the first run's cache"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_on_the_same_run_reuses_its_own_sketch_answers() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = moagan(&server, &home, tmp.path())
        .args(discover_args(&[]))
        .output()
        .unwrap();
    assert_ok(&out, "run");
    let run_dir = run_dirs(&home).remove(0);
    let sketch = run_dir.join("sketches/sk_0000.json");
    assert!(sketch.is_file(), "first sketch written");
    std::fs::remove_file(&sketch).unwrap();

    let before = server.received_requests().await.unwrap().len();
    let run_id = run_dir.file_name().unwrap().to_string_lossy().into_owned();
    let out = moagan(&server, &home, tmp.path())
        .args(["continue", "--kind", "discovery", "--run-id", &run_id])
        .arg("--non-interactive")
        .output()
        .unwrap();
    assert_ok(&out, "continue");
    assert!(sketch.is_file(), "the deleted sketch is written again");
    assert_eq!(
        sketch_requests(&server, before).await,
        0,
        "the redone sketch comes from this run's cache"
    );
}
