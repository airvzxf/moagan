//! Every stdout line is one whole JSON object under a wide fan-out
//! (#1034).
//!
//! Events (`--event-format jsonl`) and `tracing` log lines
//! (`--log-format json`, every non-ERROR level goes to stdout) are
//! written by many threads at once. Before the fix an event and a log
//! line could land on one line; a discover run with 320 mock sketches
//! at parallelism 128 showed it in 4 of 5 runs.

use std::path::{Path, PathBuf};
use std::process::Command;

const RUNS: usize = 4;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

fn mock_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider")
}

/// One wide mock discover run; returns its stdout.
fn wide_run(home: &Path) -> String {
    let out = Command::new(binary())
        .env("MOAGAN_HOME", home)
        .env("RUST_LOG", "debug")
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_LOG_TO_STDERR")
        .env_remove("MOAGAN_EVENT_FORMAT")
        .args([
            "discover",
            "--non-interactive",
            "--prompt",
            "Enumera los 7 colores del arcoiris en orden",
            "--provider",
            "mock:mock-model",
            "--mock-dir",
        ])
        .arg(mock_dir())
        .args([
            "--matrix-spec",
            "a=b,c,d,e;f=g,h,i,j;k=l,m,n,o;p=q,r,s,t",
            "--sketches-per-cell",
            "20",
            "--temperature-profile",
            "provider=mock;temperatures=0.5;replicas=1",
            "--max-parallelism",
            "128",
            "--log-format",
            "json",
            "--event-format",
            "jsonl",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn moagan");
    assert!(
        out.status.success(),
        "discover failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn every_stdout_line_of_a_wide_fan_out_is_one_json_object() {
    let mut events = 0;
    let mut logs = 0;
    for run in 0..RUNS {
        let home = tempfile::tempdir().expect("home");
        let stdout = wide_run(home.path());
        for (n, line) in stdout.lines().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("run {run}, stdout line {n} is not one JSON object ({e}): {line}")
            });
            if value.get("kind").is_some() {
                events += 1;
            }
            if value.get("level").is_some() {
                logs += 1;
            }
        }
    }
    assert!(events > 0, "no event line on stdout");
    assert!(logs > 0, "no log line on stdout");
}
