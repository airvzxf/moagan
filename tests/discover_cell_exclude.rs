//! `--cell-exclude` reaches the upstream only in its own cell (#1010).
//!
//! A two-cell discover run against a local upstream: the thesis request
//! of the excluded cell carries the exclusion, the other cell's request
//! does not, the run spec stores it, and `moagan continue` sends it
//! again when it redoes that cell.

use std::path::{Path, PathBuf};

use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

const EXCLUSION_LINE: &str = "Do NOT propose in this cell: Session cookies";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// One answer that parses as an intake brief and as a thesis.
fn canned_answer() -> String {
    json!({
        "problem": "Sign-in for a small bike shop",
        "objectives": ["Let customers sign in"],
        "constraints": ["Fixed budget"],
        "non_goals": ["Invoicing"],
        "open_questions": [],
        "raw_prompt": "Design the sign-in",
        "id": "x",
        "claim": "Sign customers in with a one-time link sent by email, no passwords.",
        "key_decisions": ["email link"],
        "architecture_outline": "One table of pending links with a 15 minute expiry.",
        "assumptions": ["every customer has an email"],
        "strengths": ["simple"],
        "weaknesses": ["email delays"],
        "hard_constraint_check": {"C1": true},
        "expected_validation": "Count failed sign-ins per week."
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
        "Design the sign-in",
        "--matrix-spec",
        "auth=oauth,api-key",
        "--theses-per-cell",
        "1",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    args
}

/// Bodies of the thesis requests the upstream received, from `skip` on.
async fn thesis_bodies(server: &MockServer, skip: usize) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .skip(skip)
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .filter(|body| body.contains("Exploration cell:"))
        .collect()
}

fn run_dir(home: &Path) -> PathBuf {
    std::fs::read_dir(home.join(".runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("one run dir")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_excluded_cell_carries_the_exclusion_and_continue_keeps_it() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = moagan(&server, &home, tmp.path())
        .args(discover_args(&[
            "--cell-exclude",
            "auth/oauth=Session cookies",
        ]))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "discover failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bodies = thesis_bodies(&server, 0).await;
    let excluded_cell: Vec<&String> = bodies
        .iter()
        .filter(|b| b.contains("(auth:oauth)"))
        .collect();
    let other_cell: Vec<&String> = bodies
        .iter()
        .filter(|b| b.contains("(auth:api-key)"))
        .collect();
    assert_eq!(
        (excluded_cell.len(), other_cell.len()),
        (1, 1),
        "one thesis per cell"
    );
    assert!(
        excluded_cell[0].contains(EXCLUSION_LINE),
        "{}",
        excluded_cell[0]
    );
    assert!(
        !other_cell[0].contains("Session cookies"),
        "{}",
        other_cell[0]
    );

    let run_dir = run_dir(&home);
    let spec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("discover_run.json")).unwrap())
            .unwrap();
    assert_eq!(
        spec["cell_excludes"],
        json!([{"dimension_id": "auth", "facet_id": "oauth", "text": "Session cookies"}])
    );

    // Redo the excluded cell: `continue` reads the exclusion back. The
    // response caches go too, so the redone call reaches the upstream.
    std::fs::remove_file(run_dir.join("theses/th_0000.json")).unwrap();
    for cache in [run_dir.join("cache"), home.join("cache")] {
        if cache.is_dir() {
            std::fs::remove_dir_all(&cache).unwrap();
        }
    }
    let before = server.received_requests().await.unwrap().len();
    let run_id = run_dir.file_name().unwrap().to_string_lossy().into_owned();
    let out = moagan(&server, &home, tmp.path())
        .args(["continue", "--kind", "discovery", "--run-id", &run_id])
        .arg("--non-interactive")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "continue failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let redone = thesis_bodies(&server, before).await;
    assert_eq!(redone.len(), 1, "only the deleted thesis is redone");
    assert!(redone[0].contains("(auth:oauth)"));
    assert!(redone[0].contains(EXCLUSION_LINE), "{}", redone[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cell_exclude_naming_no_cell_of_the_matrix_spec_stops_before_any_call() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let out = moagan(&server, &home, tmp.path())
        .args(discover_args(&["--cell-exclude", "auth/password=Bcrypt"]))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--cell-exclude auth/password: the matrix spec has no such cell"),
        "{stderr}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}
