//! A discover run recorded through `moagan audit proxy` completes with the
//! registry auto-probes enabled (#996). The probes go through the proxy
//! like every other call, and the sidecar records each exchange.
//!
//! Measured on 2026-10-13 against the real MiniMax-M3 upstream (60 recorded
//! exchanges, all four sidecars written, no `Broken pipe`); this test pins
//! the same path against a local upstream.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncBufReadExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// One answer that parses as an intake brief and as a thesis, so every
/// call of a one-cell discover run (and every probe) gets a 200.
fn canned_answer() -> String {
    json!({
        "problem": "Parts catalogue for a small bike shop",
        "objectives": ["Sell spare parts online"],
        "constraints": ["Fixed budget"],
        "non_goals": ["Invoicing"],
        "open_questions": [],
        "raw_prompt": "Design a parts catalogue",
        "id": "x",
        "claim": "List every part once with its fitting bikes and one price per SKU.",
        "key_decisions": ["one price per SKU"],
        "architecture_outline": "A static catalogue generated from one spreadsheet.",
        "assumptions": ["prices change monthly"],
        "strengths": ["simple"],
        "weaknesses": ["manual updates"],
        "hard_constraint_check": {"C1": true},
        "expected_validation": "Count orders per week."
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

fn proxy_port(line: &str) -> Option<u16> {
    line.split("http://")
        .nth(1)?
        .split(" ->")
        .next()?
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// `(requests, responses, records with an error)` of a run's sidecar.
fn audit_counts(run_dir: &Path) -> (usize, usize, usize) {
    let file = std::fs::File::open(run_dir.join("telemetry/external_audit.jsonl.gz"))
        .expect("external_audit.jsonl.gz");
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(file)
        .read_to_string(&mut text)
        .expect("gunzip");
    let mut counts = (0, 0, 0);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line).expect("audit record");
        match record["event"].as_str() {
            Some("request") => counts.0 += 1,
            Some("response") => counts.1 += 1,
            _ => {}
        }
        if !record["error"].is_null() {
            counts.2 += 1;
        }
    }
    counts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_discover_run_through_the_proxy_completes_with_the_probes_enabled() {
    let server = upstream().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let mut proxy = tokio::process::Command::new(binary())
        .args(["audit", "proxy", "--port", "0", "--upstream"])
        .arg(format!("{}/anthropic/v1", server.uri()))
        .arg("--runs-dir")
        .arg(&home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn audit proxy");
    let mut lines = tokio::io::BufReader::new(proxy.stderr.take().unwrap()).lines();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("proxy exited");
            if let Some(port) = line
                .contains("proxy listening")
                .then(|| proxy_port(&line))
                .flatten()
            {
                return port;
            }
        }
    })
    .await
    .expect("proxy announced its port");
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    let out = tokio::process::Command::new(binary())
        .args([
            "discover",
            "--non-interactive",
            "--provider",
            "minimax:MiniMax-M3",
            "--prompt",
            "Design a parts catalogue",
            "--matrix-spec",
            "pricing=one-price",
            "--theses-per-cell",
            "1",
        ])
        .current_dir(tmp.path())
        .env("MOAGAN_HOME", &home)
        .env("MINIMAX_API_KEY", "test-key")
        .env(
            "MOAGAN_MINIMAX_ENDPOINT",
            format!("http://127.0.0.1:{port}/anthropic/v1/messages"),
        )
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env("MOAGAN_QUIET", "1")
        .env_remove("MOAGAN_CONFIG")
        .env_remove("MOAGAN_MAX_TOKEN_AUTO")
        .env_remove("MOAGAN_TEMPERATURE_AUTO")
        .env_remove("MOAGAN_TOP_P_AUTO")
        .env_remove("MOAGAN_TOP_K_AUTO")
        .env_remove("MOAGAN_PROBE_TTL_SECS")
        .output()
        .await
        .expect("spawn moagan discover");
    let _ = proxy.kill().await;
    assert!(
        out.status.success(),
        "discover through the proxy failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    for sidecar in [
        "max_tokens_auto.toml",
        "temperatures_auto.toml",
        "top_p_auto.toml",
        "top_k_auto.toml",
    ] {
        assert!(home.join(sidecar).is_file(), "the probes wrote {sidecar}");
    }
    let run_dir = std::fs::read_dir(home.join(".runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("one run dir");
    assert!(run_dir.join("final/README.md").is_file());
    let (requests, responses, errors) = audit_counts(&run_dir);
    let upstream_requests = server.received_requests().await.unwrap().len();
    assert!(
        requests > 10,
        "the probes went through the proxy: {requests} requests"
    );
    assert_eq!(
        requests, responses,
        "every recorded request has its response"
    );
    assert_eq!(errors, 0, "no recorded exchange failed");
    assert!(
        upstream_requests >= requests,
        "the proxy forwarded what it recorded: {upstream_requests} upstream, {requests} recorded"
    );
}
