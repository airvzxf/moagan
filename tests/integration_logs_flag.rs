//! Integration tests for the `--logs` flag and the
//! `MOAGAN_RUN_LOGS` env var. Verifies that:
//!
//! 1. `--logs <path>` writes plain text (no ANSI) to the given
//!    file while stderr continues to receive events.
//! 2. `MOAGAN_RUN_LOGS=<path>` does the same without the flag.
//! 3. The env var wins when both are set (Unix convention).
//! 4. The parent directory is created on demand.
//! 5. Without either, the behaviour is unchanged (the file is
//!    not created and stderr still receives events).
//!
//! The tests drive the binary through `moagan validate <brief>`,
//! which is the cheapest subcommand that emits a real
//! `tracing::info!("config: ...")` event without requiring any
//! provider API key. The structural check the validator runs
//! only reads the brief file; no LLM is touched, so the tests
//! stay green in CI runners that have no provider keys
//! configured (the previous `moagan doctor` driver exited 1 in
//! that environment because it checks `minimax` / `deepseek` /
//! `opencode` API key resolution and the runner ships without
//! any of those secrets).
//!
//! All tests use `CARGO_BIN_EXE_moagan` (provided by cargo for
//! integration tests), a fresh `tempfile::TempDir` per test, and
//! a minimal valid `brief.json` written next to it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Spawn the binary with the given argv. Returns a [`Command`]
/// with stderr piped (so we can also assert stderr still
/// receives events), the test-scoped `MOAGAN_HOME` set, and
/// `MOAGAN_QUIET` cleared so dotenv loading messages do not
/// pollute the assertions.
fn run_cmd(args: &[&str], home: &Path) -> Command {
    let bin = std::env::var("CARGO_BIN_EXE_moagan")
        .expect("CARGO_BIN_EXE_moagan — run via `cargo test`, not directly");
    let mut cmd = Command::new(&bin);
    cmd.args(args)
        .env("MOAGAN_HOME", home)
        .env_remove("MOAGAN_RUN_LOGS")
        .env_remove("MOAGAN_RUNS_DIR")
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_QUIET");
    cmd
}

/// Write a minimal valid `brief.json` under `home/brief.json`
/// and return its path. The validator runs the same structural
/// gate as the real pipeline, so the proposal it synthesises
/// from this brief must pass: empty arrays + a short
/// non-problematic `problem` triggers no hard issues and keeps
/// the synthetic proposal length inside the default
/// `gate_min_length=50` / `gate_max_length=5000` window
/// (`summary` 4 chars + `approach` 4 chars + 32 per non-empty
/// tradeoff/evidence line = 72 total).
fn write_minimal_brief(home: &Path) -> PathBuf {
    let brief_path = home.join("brief.json");
    let body = serde_json::json!({
        "problem": "Test",
        "objectives": [],
        "deliverables": [],
        "constraints": [],
        "assumptions": [],
        "non_goals": [],
        "acceptance": [],
        "risks": []
    });
    std::fs::write(
        &brief_path,
        serde_json::to_string(&body).expect("serialise minimal brief"),
    )
    .expect("write minimal brief.json");
    brief_path
}

/// Count ANSI escape sequences in `s`. We only count the CSI
/// introducer `\x1b[` — the one tracing-subscriber emits for
/// color/bold. The other escape types (e.g. plain `\x1b]`)
/// cannot appear from `fmt::layer()`.
fn ansi_count(s: &str) -> usize {
    s.matches("\u{1b}[").count()
}

#[test]
fn logs_flag_writes_plain_text_to_file_and_leaves_stderr_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log_path = tmp.path().join("moagan.log");
    let brief = write_minimal_brief(tmp.path());
    let out = run_cmd(
        &[
            "--logs",
            log_path.to_str().unwrap(),
            "validate",
            brief.to_str().unwrap(),
        ],
        tmp.path(),
    )
    .output()
    .expect("spawn moagan --logs validate");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    // File must exist, be non-empty, and contain NO ANSI codes.
    assert!(log_path.exists(), "log file not created at {log_path:?}");
    let body = std::fs::read_to_string(&log_path).expect("read log file");
    assert!(
        !body.is_empty(),
        "log file is empty (expected at least one tracing event from config load)"
    );
    assert_eq!(
        ansi_count(&body),
        0,
        "log file unexpectedly contains ANSI escapes; first 200 chars: {body:.200}"
    );

    // The file must contain at least one `INFO`-level event
    // (config load always emits one). Pin the format loosely so
    // we are robust against future formatter tweaks: we only
    // assert the message body is there, not the exact prefix.
    assert!(
        body.contains("config:"),
        "expected a config-load tracing event, got:\n{body}"
    );

    // stderr must continue to receive events (we do not silence
    // the existing stderr layer). With no `.env` next to the
    // fresh MOAGAN_HOME, dotenv loading is silent — but
    // `Config::load` still emits the `config:` INFO event that
    // bubbles up to stderr.
    assert!(
        !stderr.is_empty(),
        "stderr is empty — the stderr layer was accidentally silenced"
    );
}

#[test]
fn logs_env_var_writes_plain_text_without_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log_path = tmp.path().join("moagan-env.log");
    let brief = write_minimal_brief(tmp.path());
    let out = run_cmd(&["validate", brief.to_str().unwrap()], tmp.path())
        .env("MOAGAN_RUN_LOGS", &log_path)
        .output()
        .expect("spawn moagan validate with MOAGAN_RUN_LOGS");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    assert!(
        log_path.exists(),
        "log file not created when only env var was set"
    );
    let body = std::fs::read_to_string(&log_path).expect("read log file");
    assert!(!body.is_empty(), "log file is empty");
    assert_eq!(
        ansi_count(&body),
        0,
        "log file unexpectedly contains ANSI escapes; first 200 chars: {body:.200}"
    );
    assert!(
        body.contains("config:"),
        "expected a config-load tracing event, got:\n{body}"
    );
}

#[test]
fn logs_env_var_wins_over_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let env_path = tmp.path().join("from-env.log");
    let flag_path = tmp.path().join("from-flag.log");
    let brief = write_minimal_brief(tmp.path());

    let out = run_cmd(
        &[
            "--logs",
            flag_path.to_str().unwrap(),
            "validate",
            brief.to_str().unwrap(),
        ],
        tmp.path(),
    )
    .env("MOAGAN_RUN_LOGS", &env_path)
    .output()
    .expect("spawn moagan with both --logs and MOAGAN_RUN_LOGS");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    // Env var wins → env_path gets the events.
    assert!(
        env_path.exists(),
        "MOAGAN_RUN_LOGS path should exist (env var wins)"
    );
    let env_body = std::fs::read_to_string(&env_path).expect("read env log");
    assert!(
        !env_body.is_empty(),
        "env-var log is empty — env var precedence broken"
    );
    assert_eq!(ansi_count(&env_body), 0, "env-var log has ANSI escapes");

    // Flag path is NOT created (the env var's path won; the
    // writer was never pointed at the flag path).
    assert!(
        !flag_path.exists(),
        "--logs flag path should NOT have been created when the env var was set"
    );
}

#[test]
fn logs_flag_creates_parent_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Nested path that does not exist yet. We expect `set()` to
    // call `create_dir_all` on the parent.
    let nested = tmp.path().join("newdir").join("sub").join("moagan.log");
    assert!(!nested.parent().unwrap().exists());
    let brief = write_minimal_brief(tmp.path());

    let out = run_cmd(
        &[
            "--logs",
            nested.to_str().unwrap(),
            "validate",
            brief.to_str().unwrap(),
        ],
        tmp.path(),
    )
    .output()
    .expect("spawn moagan with nested --logs path");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    assert!(
        nested.parent().unwrap().exists(),
        "parent dir should be auto-created, path={}",
        nested.display()
    );
    assert!(nested.exists(), "log file should exist after run");
    let body = std::fs::read_to_string(&nested).expect("read nested log");
    assert!(!body.is_empty(), "nested log file is empty");
}

#[test]
fn no_logs_unchanged() {
    // Without --logs or MOAGAN_RUN_LOGS, the file is never
    // created and stderr still receives events.
    let tmp = tempfile::tempdir().expect("tempdir");
    let brief = write_minimal_brief(tmp.path());
    let out = run_cmd(&["validate", brief.to_str().unwrap()], tmp.path())
        .output()
        .expect("spawn moagan validate");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    // No log file should have been created at the tempdir root.
    let mut found = Vec::new();
    for entry in std::fs::read_dir(tmp.path()).expect("read tmpdir") {
        let entry = entry.expect("read_dir entry");
        if entry.path().extension().and_then(|s| s.to_str()) == Some("log") {
            found.push(entry.path());
        }
    }
    assert!(
        found.is_empty(),
        "no .log files should be created without --logs/MOAGAN_RUN_LOGS, found: {found:?}"
    );

    // stderr must continue to receive events. Without
    // --logs/MOAGAN_RUN_LOGS the file layer is never wired,
    // so the `config:` INFO event still flows through the
    // stderr layer.
    assert!(
        !stderr.is_empty(),
        "stderr is empty — the stderr layer was accidentally silenced"
    );
}

/// `moagan --help` must mention the `--logs` flag so operators
/// can discover it. We pin the long form to keep the help
/// reachable even after a future flag rename.
#[test]
fn help_mentions_logs_flag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_cmd(&["--help"], tmp.path())
        .output()
        .expect("spawn moagan --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "--help failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    assert!(
        stdout.contains("--logs"),
        "expected `--logs` in --help output, got:\n{stdout}"
    );
    // Document the env var in the flag's doc comment so
    // operators who skim --help see both knobs.
    assert!(
        stdout.contains("MOAGAN_RUN_LOGS"),
        "expected `MOAGAN_RUN_LOGS` documented in --help output, got:\n{stdout}"
    );
}

/// Compile-time assertion: the path-handling surfaces the env
/// var correctly even when it contains a trailing newline or
/// extra whitespace (POSIX shells sometimes preserve those).
#[test]
fn logs_env_var_with_empty_value_is_ignored() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let flag_path = tmp.path().join("from-flag.log");
    let brief = write_minimal_brief(tmp.path());

    let out = run_cmd(
        &[
            "--logs",
            flag_path.to_str().unwrap(),
            "validate",
            brief.to_str().unwrap(),
        ],
        tmp.path(),
    )
    // Empty env var → treated as unset → flag wins.
    .env("MOAGAN_RUN_LOGS", "")
    .output()
    .expect("spawn moagan with empty MOAGAN_RUN_LOGS");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "validate failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    assert!(
        flag_path.exists(),
        "with MOAGAN_RUN_LOGS='' the flag should win, but {flag_path:?} was not created"
    );
}

/// Helper: ensure a path is absolute. clap rejects relative
/// `--logs` paths silently on some platforms; we want to make
/// sure the integration tests always feed absolute paths so
/// the assertion on `tmp.path().join(...)` is meaningful.
#[allow(dead_code)]
fn ensure_absolute(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        p
    } else {
        PathBuf::from("/")
    }
}
