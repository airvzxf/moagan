//! A provider that needs a key and has none fails before any HTTP
//! request (#999): no intake call, no auto-probe, no 401 from the
//! upstream. The upstream is a wiremock server that would accept
//! every request, so any request it receives is a bug.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// Upstream that answers every request with a valid Anthropic-compat body.
async fn accepting_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "{}"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    server
}

/// Run `moagan <args>` against `server` with no key anywhere: no
/// `MINIMAX_API_KEY`, an empty `MOAGAN_HOME` (no `api_keys.toml`), no
/// config file, and a working directory without a `.env`.
async fn run_without_key(server: &MockServer, dir: &Path, args: &[&str]) -> Output {
    tokio::process::Command::new(binary())
        .args(args)
        .current_dir(dir)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_CONFIG")
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("MOAGAN_HOME", dir.join("home"))
        .env(
            "MOAGAN_MINIMAX_ENDPOINT",
            format!("{}/anthropic/v1/messages", server.uri()),
        )
        .env("MOAGAN_QUIET", "1")
        .output()
        .await
        .expect("spawn moagan")
}

fn assert_missing_key(out: &Output, server_requests: usize) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "exit code 3 (api key); stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("MINIMAX_API_KEY not set"),
        "stderr names the missing env var:\n{stderr}"
    );
    assert_eq!(server_requests, 0, "no HTTP request before the key check");
}

#[tokio::test]
async fn discover_without_a_key_fails_before_any_request() {
    let server = accepting_upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let out = run_without_key(
        &server,
        tmp.path(),
        &[
            "discover",
            "--non-interactive",
            "--provider",
            "minimax:MiniMax-M3",
            "--prompt",
            "Design a parts catalogue",
            "--dimensions",
            "1",
            "--facets-per-dimension",
            "1",
        ],
    )
    .await;
    let requests = server.received_requests().await.unwrap().len();
    assert_missing_key(&out, requests);
}

#[tokio::test]
async fn run_without_a_key_fails_before_any_request() {
    let server = accepting_upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let out = run_without_key(
        &server,
        tmp.path(),
        &[
            "run",
            "--mode",
            "fast",
            "--non-interactive",
            "--provider",
            "minimax:MiniMax-M3",
            "--prompt",
            "List the seven rainbow colors in order",
        ],
    )
    .await;
    let requests = server.received_requests().await.unwrap().len();
    assert_missing_key(&out, requests);
}

#[tokio::test]
async fn the_mock_provider_still_needs_no_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_provider");
    let out = tokio::process::Command::new(binary())
        .args([
            "run",
            "--mode",
            "fast",
            "--non-interactive",
            "--provider",
            "mock:mock-model",
        ])
        .arg("--mock-dir")
        .arg(&mock_dir)
        .args(["--prompt", "List the seven rainbow colors in order"])
        .current_dir(tmp.path())
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_CONFIG")
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env("MOAGAN_HOME", tmp.path().join("home"))
        .env("MOAGAN_QUIET", "1")
        .output()
        .await
        .expect("spawn moagan");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mock run succeeds without a key:\n{stderr}"
    );
}
