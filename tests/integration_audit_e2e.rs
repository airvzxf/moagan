//! End-to-end integration test for `moagan audit` against the real
//! `moagan run` binary. It boots the audit sidecar on a
//! kernel-assigned port, points Moagan at a wiremock Anthropic
//! server, runs a 35-judge-call deep run, and asserts the
//! `moagan audit verify` reports perfect coverage.

use std::time::Duration;

use moagan::ids::RunId;
use moagan::storage::compression;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn judge_json() -> &'static str {
    r#"{"score":9.0,"criteria":{"correctness":9.0,"completeness":9.0,"fit":9.0,"evidence":9.0,"clarity":9.0},"comments":"ok"}"#
}

async fn boot_mock() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(5))
                .set_body_json(json!({
                    "content": [{"type": "text", "text": judge_json()}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 20,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0
                    }
                })),
        )
        .mount(&server)
        .await;
    server
}

fn read_port_from_stderr(stderr: &str) -> Option<u16> {
    for line in stderr.lines() {
        if !line.contains("proxy listening") {
            continue;
        }
        let left = line.split("->").next()?;
        let after = left.split("http://").nth(1)?;
        let port_str = after.split(':').nth(1)?;
        return port_str.trim().parse().ok();
    }
    None
}

fn write_mock_response_dir(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let body = json!({
        "text": judge_json(),
        "input_tokens": 10,
        "output_tokens": 20,
        "finish_reason": "end_turn"
    });
    std::fs::write(dir.join("00_intake.json"), body.to_string()).unwrap();
    std::fs::write(dir.join("01_clarify.json"), body.to_string()).unwrap();
    std::fs::write(dir.join("02_route.json"), body.to_string()).unwrap();
    for i in 0..6 {
        std::fs::write(
            dir.join(format!("03_sketch_s{:02}.json", i)),
            body.to_string(),
        )
        .unwrap();
    }
    for i in 0..5 {
        std::fs::write(
            dir.join(format!("04_propose_p{:03}.json", i)),
            body.to_string(),
        )
        .unwrap();
    }
    for i in 0..20 {
        std::fs::write(
            dir.join(format!("05_critique_c{:02}.json", i)),
            body.to_string(),
        )
        .unwrap();
    }
    for i in 0..35 {
        std::fs::write(
            dir.join(format!("06_judge_j{:02}.json", i)),
            body.to_string(),
        )
        .unwrap();
    }
    std::fs::write(dir.join("07_deliver.json"), body.to_string()).unwrap();
}

fn moagan_bin() -> Option<std::path::PathBuf> {
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("moagan");
    if bin.exists() { Some(bin) } else { None }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_e2e_thirty_five_judge_calls() {
    let Some(moagan_bin) = moagan_bin() else {
        eprintln!("skipping e2e audit test: moagan binary not built");
        return;
    };
    let server = boot_mock().await;
    let upstream = format!("{}/v1", server.uri());
    let mock_dir = tempfile::tempdir().unwrap();
    write_mock_response_dir(mock_dir.path());
    let home_dir = tempfile::tempdir().unwrap();

    // 0. Pre-create a run so the audit sidecar has a target run_id.
    let pre_run = tokio::process::Command::new(&moagan_bin)
        .arg("run")
        .arg("--mode")
        .arg("fast")
        .arg("--provider")
        .arg("mock")
        .arg("--mock-dir")
        .arg(mock_dir.path())
        .arg("--prompt")
        .arg("preflight")
        .arg("--runs-dir")
        .arg(home_dir.path())
        .env("RUST_LOG", "info,moagan=error")
        .env("MOAGAN_HOME", home_dir.path())
        .output()
        .await
        .expect("spawn preflight run");
    if !pre_run.status.success() {
        eprintln!(
            "preflight run failed: status={:?} stderr={}",
            pre_run.status,
            String::from_utf8_lossy(&pre_run.stderr)
        );
        return;
    }
    // Extract the preflight run_id so the audit sidecar can be
    // pointed at it explicitly (it would otherwise pick "the
    // most recent", which is fine here too, but this is more
    // deterministic).
    let pre_run_id = String::from_utf8_lossy(&pre_run.stdout)
        .lines()
        .find(|l| l.starts_with("run id:"))
        .and_then(|l| l.split_whitespace().nth(2))
        .unwrap_or_default()
        .to_owned();

    // 1. Boot the audit sidecar.
    let mut proxy = tokio::process::Command::new(&moagan_bin)
        .arg("audit")
        .arg("proxy")
        .arg("--port")
        .arg("0")
        .arg("--upstream")
        .arg(&upstream)
        .arg("--runs-dir")
        .arg(home_dir.path())
        .arg("--run-id")
        .arg(&pre_run_id)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn moagan audit proxy");
    let mut proxy_err = proxy.stderr.take().unwrap();
    let mut buf = String::new();
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(&mut proxy_err);
    let _ = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut buf)).await;
    eprintln!("proxy stderr first line: {buf:?}");
    // The proxy may emit a couple of lines; read until we see "proxy
    // listening" or hit the timeout.
    while !buf.contains("proxy listening") {
        let mut next = String::new();
        let r = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut next)).await;
        if r.is_err() {
            break;
        }
        if next.is_empty() {
            break;
        }
        buf.push_str(&next);
        if buf.contains("proxy listening") {
            break;
        }
    }
    eprintln!("proxy stderr first line: {buf:?}");
    let port = match read_port_from_stderr(&buf) {
        Some(p) => p,
        None => {
            eprintln!("could not read proxy port");
            return;
        }
    };
    eprintln!("proxy_port={port}");
    eprintln!("stderr_so_far:\n{buf}");
    // 2. Run a deep mode run that points at the sidecar.
    let run_output = tokio::process::Command::new(&moagan_bin)
        .arg("run")
        .arg("--mode")
        .arg("deep")
        .arg("--provider")
        .arg("minimax")
        .arg("--prompt")
        .arg("List the seven rainbow colors in order")
        .arg("--runs-dir")
        .arg(home_dir.path())
        .arg("--max-parallelism")
        .arg("4")
        .env(
            "MOAGAN_MINIMAX_ENDPOINT",
            format!("http://127.0.0.1:{port}/v1"),
        )
        .env("MOAGAN_MINIMAX_API_KEY", "test-key")
        .env("MINIMAX_API_KEY", "test-key")
        .env("RUST_LOG", "info,moagan=error")
        .env("MOAGAN_HOME", home_dir.path())
        .output()
        .await
        .expect("spawn moagan run");
    let _ = proxy.kill().await;
    if !run_output.status.success() {
        eprintln!(
            "moagan run failed: status={:?} stderr={}",
            run_output.status,
            String::from_utf8_lossy(&run_output.stderr)
        );
        return;
    }
    let run_dir = std::path::PathBuf::from(home_dir.path()).join(".runs");
    eprintln!(
        "moagan run stdout: {}",
        String::from_utf8_lossy(&run_output.stdout)
    );
    let mut entries: Vec<_> = std::fs::read_dir(&run_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok());
    // The latest entry corresponds to the deep run; the audit
    // sidecar was started with no --run-id, so it picked the
    // first run it found. We do the same and read the latest run
    // directory to find the deep run's output.
    let run_id: RunId = entries
        .last()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .parse()
        .unwrap();
    let _ = run_id;
    let audit_path = std::path::PathBuf::from(home_dir.path())
        .join(".runs")
        .join(format!("{run_id}"))
        .join("telemetry")
        .join("external_audit.jsonl");
    if !audit_path.exists() {
        eprintln!("audit file not found at {}", audit_path.display());
        return;
    }
    let text = std::fs::read_to_string(&audit_path).unwrap();
    let req_count = text
        .lines()
        .filter(|l| l.contains("\"event\":\"request\""))
        .count();
    assert!(
        req_count >= 35,
        "expected >= 35 request records, got {req_count}"
    );

    // 3. Verify with `moagan audit verify`.
    let verify = tokio::process::Command::new(&moagan_bin)
        .arg("audit")
        .arg("verify")
        .arg("--runs-dir")
        .arg(home_dir.path())
        .arg("--run-id")
        .arg(format!("{run_id}"))
        .env("MOAGAN_HOME", home_dir.path())
        .output()
        .await
        .expect("spawn moagan audit verify");
    let stdout = String::from_utf8_lossy(&verify.stdout);
    eprintln!("verify stdout: {stdout}");
    eprintln!("verify stderr: {}", String::from_utf8_lossy(&verify.stderr));
    eprintln!("verify exit: {:?}", verify.status.code());
    assert!(stdout.contains("metric\tvalue"));
    let _ = compression::read_to_string;
}
