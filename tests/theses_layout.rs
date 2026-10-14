//! Both pipelines produce theses: a run dir holds `theses/th_*.json`,
//! each thesis states its `claim`, the calls are logged under the
//! `thesis` role, and the old `sketch` names are gone.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn only_run_dir(home: &Path) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(home.join(".runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "exactly one run");
    dirs.remove(0)
}

/// Names of the thesis files (no sidecars) under `run_dir/theses`.
fn thesis_files(run_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(run_dir.join("theses"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json") && !n.ends_with(".meta.json"))
        .collect();
    names.sort();
    names
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn calls_log(run_dir: &Path) -> String {
    moagan::storage::compression::read_to_string(&run_dir.join("telemetry/calls.jsonl.gz")).unwrap()
}

fn assert_ok(out: &std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_explore_run_writes_its_theses_under_theses() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = std::process::Command::new(binary())
        .current_dir(tmp.path())
        .env("MOAGAN_HOME", &home)
        .env("MOAGAN_QUIET", "1")
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env_remove("MOAGAN_CONFIG")
        .args(["run", "--mode", "explore", "--provider", "mock:mock-model"])
        .arg("--mock-dir")
        .arg(repo_root().join("tests/fixtures/mock_provider"))
        .args([
            "--non-interactive",
            "--prompt",
            "Design an inventory for a bike shop",
        ])
        .output()
        .unwrap();
    assert_ok(&out, "explore run");
    let run_dir = only_run_dir(&home);
    assert!(!run_dir.join("sketches").exists(), "no sketches/ dir");
    let files = thesis_files(&run_dir);
    assert!(!files.is_empty(), "at least one thesis");
    assert!(files.iter().all(|n| n.starts_with("th_")), "{files:?}");
    let thesis = read_json(&run_dir.join("theses").join(&files[0]));
    assert!(thesis["claim"].is_string(), "a thesis states its claim");
    assert!(thesis.get("thesis").is_none(), "no inner `thesis` field");
    assert!(run_dir.join("final/theses_summary.json").is_file());
    let calls = calls_log(&run_dir);
    assert!(calls.contains("\"role\":\"thesis\""));
    assert!(!calls.contains("\"role\":\"sketch\""));
}

/// One answer that parses as an intake brief and as a thesis.
fn canned_answer() -> String {
    json!({
        "problem": "Inventory for a small bike shop",
        "objectives": ["Find any part in seconds"],
        "constraints": ["Minimal budget"],
        "non_goals": ["Invoicing"],
        "open_questions": [],
        "raw_prompt": "Design the inventory",
        "id": "x",
        "claim": "Keep the inventory in one shared spreadsheet with one row per part and a bin code.",
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

fn discover(
    server: &MockServer,
    home: &Path,
    work: &Path,
    per_cell_flag: &str,
) -> std::process::Output {
    std::process::Command::new(binary())
        .current_dir(work)
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
        .env_remove("MOAGAN_CONFIG")
        .args([
            "discover",
            "--non-interactive",
            "--provider",
            "minimax:MiniMax-M3",
        ])
        .args([
            "--prompt",
            "Design the inventory",
            "--matrix-spec",
            "storage=engine,backup",
        ])
        .args([per_cell_flag, "1"])
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discover_run_numbers_its_theses_th_and_catalogues_them() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = discover(&server, &home, tmp.path(), "--theses-per-cell");
    assert_ok(&out, "discover run");
    let run_dir = only_run_dir(&home);
    assert!(!run_dir.join("sketches").exists(), "no sketches/ dir");
    assert_eq!(thesis_files(&run_dir), ["th_0000.json", "th_0001.json"]);
    let thesis = read_json(&run_dir.join("theses/th_0000.json"));
    assert_eq!(thesis["id"], "th_0000");
    assert!(
        thesis["claim"]
            .as_str()
            .unwrap()
            .starts_with("Keep the inventory")
    );
    let catalog = std::fs::read_to_string(run_dir.join("final/catalog.json")).unwrap();
    assert!(catalog.contains("th_0000") && catalog.contains("th_0001"));
    assert!(!catalog.contains("sk_"), "no sk_ ids in the catalogue");
    assert!(calls_log(&run_dir).contains("\"role\":\"thesis\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_old_sketches_per_cell_flag_is_gone() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = discover(&server, &home, tmp.path(), "--sketches-per-cell");
    assert!(!out.status.success(), "--sketches-per-cell is rejected");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--sketches-per-cell"));
}
