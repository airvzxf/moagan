//! Integration tests for `c2` (commit "feat(observability): emit
//! Decision events at curated decision points").
//!
//! Two invariants are pinned by this file:
//!
//! 1. **`winner_picked` is emitted at the default verbosity.** A
//!    `moagan run --mode fast --provider mock:mock-model` against
//!    the canned fixtures in `tests/fixtures/mock_provider/` must
//!    produce at least one `kind == "decision"` line with
//!    `decision_kind == "winner_picked"` on stdout. The event is
//!    classified as Summary (always emitted under the default
//!    `--decision-format summary`), so this test runs without any
//!    explicit `--decision-format` flag.
//!
//! 2. **`cache_miss` is suppressed under Summary and emitted under
//!    All.** The cache observability event is classified as AllOnly
//!    so the default verbosity does not flood stdout. A `moagan run
//!    … --decision-format summary` invocation must produce ZERO
//!    `cache_miss` decision lines; the same invocation with
//!    `--decision-format all` must produce at least one (every first
//!    call misses the prompt cache).
//!
//! Both tests drive the real binary through `std::process::Command`
//! (mirroring `tests/integration_run_id_propagation.rs`) and assert
// against the on-disk `events.jsonl` file.

use std::path::Path;
use std::process::Command;

use moagan::test_support::with_moagan_home;

/// Resolve the freshly built `moagan` binary. Mirrors
/// `tests/integration_run_id_propagation.rs::moagan_bin` so a plain
/// `cargo test --test integration_decisions` invocation still finds
/// the binary.
fn moagan_bin() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_moagan")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("moagan")
        })
}

/// Read every JSONL line from `path` and parse it as a
/// `serde_json::Value`. Lines that fail to parse are kept as raw
/// strings (with a `__raw__` marker) so a corrupted fixture surfaces
/// in the assertion failure message instead of being silently dropped.
fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .unwrap_or_else(|e| panic!("parse jsonl line {l:?}: {e}"))
        })
        .collect()
}

/// Count decision events with the given `decision_kind`. Mirrors
/// the `jq -c 'select(.kind == "decision" and .decision_kind ==
/// "<kind>")' events.jsonl | wc -l` operator-facing query.
fn count_decision_kind(events: &[serde_json::Value], kind: &str) -> usize {
    events
        .iter()
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("decision"))
        .filter(|v| v.get("decision_kind").and_then(|k| k.as_str()) == Some(kind))
        .count()
}

/// Drive a `moagan run --provider mock:mock-model` invocation with
/// the given `--decision-format` flag value, capture stdout
/// (events) and stderr (tracing) to files under `work`, and return
/// the parsed JSONL events plus the binary's exit code. The
/// returned `MoaganRunOutput::exit_code` is `Some(0)` when the
/// run succeeded, `Some(non-zero)` when the binary errored, and
/// `None` when the binary panicked (a panic before the run started
/// surfaces as `output.status.code() == None`).
fn drive_mock_run(home: &Path, work: &Path, decision_format: &str) -> RunOutput {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let stdout_path = work.join("events.jsonl");
    let stderr_path = work.join("log.jsonl");
    let output = Command::new(moagan_bin())
        .env("MOAGAN_HOME", home)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .arg("run")
        .arg("--mode")
        .arg("fast")
        .arg("--provider")
        .arg("mock:mock-model")
        .arg("--prompt")
        .arg("Enumera los 7 colores del arcoiris en orden")
        .arg("--mock-dir")
        .arg(&mock_dir)
        .arg("--non-interactive")
        .arg("--log-format")
        .arg("json")
        .arg("--event-format")
        .arg("jsonl")
        .arg("--decision-format")
        .arg(decision_format)
        .stdout(std::fs::File::create(&stdout_path).expect("create events.jsonl"))
        .stderr(std::fs::File::create(&stderr_path).expect("create log.jsonl"))
        .output()
        .expect("spawn moagan run");
    let exit_code = output.status.code();
    let events = read_jsonl(&stdout_path);
    let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    RunOutput {
        events,
        exit_code,
        stderr_text,
    }
}

/// Captured output of a single `moagan run` invocation.
struct RunOutput {
    /// Parsed JSONL events from stdout.
    events: Vec<serde_json::Value>,
    /// Binary exit code. `Some(0)` on success; `Some(non-zero)` when
    /// the binary returned an error; `None` if the binary was
    /// killed by a signal (e.g. SIGPIPE from a downstream filter).
    #[allow(dead_code)]
    exit_code: Option<i32>,
    /// Raw stderr text (NDJSON expected under `--log-format json`).
    /// Held for debugging when an assertion fails.
    #[allow(dead_code)]
    stderr_text: String,
}

/// `Event::Decision { decision_kind: "winner_picked", … }` is
/// emitted on stdout at the default `--decision-format summary`
/// verbosity. The fast-mode mock run completes the rank phase and
/// MUST produce at least one winner-picked event before the deliver
/// phase wraps the run. The test pins that the curated Summary
/// classification is honoured: a no-flag run still surfaces
/// `winner_picked`.
///
/// `winner_picked` is emitted AFTER the rank phase, so a successful
/// fast-mode mock run has at least one. The mock provider fixtures
/// include three judge / propose / critique / deliver mocks so the
/// pipeline can complete end-to-end.
#[test]
fn winner_picked_decision_emitted_in_summary_mode() {
    with_moagan_home("decision_winner_picked_summary", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_mock_run(home, work.path(), "summary");
        let winner_count = count_decision_kind(&out.events, "winner_picked");
        assert!(
            winner_count >= 1,
            "expected >= 1 winner_picked decision events under --decision-format summary; got {winner_count}; \
             exit_code={:?}; stderr={}",
            out.exit_code,
            out.stderr_text
        );
    });
}

/// `Event::Decision { decision_kind: "cache_miss", … }` is
/// suppressed under `--decision-format summary` (the default) and
/// emitted under `--decision-format all`.
#[test]
fn cache_miss_decision_suppressed_in_summary_but_emitted_in_all() {
    with_moagan_home("decision_cache_miss_suppression", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let summary = drive_mock_run(home, work.path(), "summary");
        let summary_cache_miss = count_decision_kind(&summary.events, "cache_miss");
        assert_eq!(
            summary_cache_miss, 0,
            "--decision-format summary must suppress cache_miss; \
             got {summary_cache_miss} events; \
             exit_code={:?}; stderr={}",
            summary.exit_code, summary.stderr_text
        );

        let all = drive_mock_run(home, work.path(), "all");
        let all_cache_miss = count_decision_kind(&all.events, "cache_miss");
        assert!(
            all_cache_miss > 0,
            "--decision-format all must emit cache_miss; \
             exit_code={:?}; stderr={}",
            all.exit_code,
            all.stderr_text
        );
    });
}
