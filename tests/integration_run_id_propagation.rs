//! Integration tests for `c1` (commit "feat(observability): thread real
//! run_id through cli::dispatch").
//!
//! Two invariants are pinned by this file:
//!
//! 1. The dispatcher propagates the real `run_id` (UUID v7) produced
//!    by the pipeline through to the `Event::RunStart` /
//!    `Event::RunEnd` payloads on stdout. Pre-`c1` the bus emitted the
//!    literal placeholder `"pre-dispatch"` for every event — operators
//!    who grepped `events.jsonl` for `kind == "run_start"` saw a
//!    string that did not match any directory under `MOAGAN_HOME`.
//!
//! 2. The `[moagan] loaded .env from …` notice emitted by `main.rs`
//!    on boot now flows through `tracing::info!` AFTER
//!    `init_tracing()`, respecting `--log-format` and `RUST_LOG`. The
//!    pre-`c1` version used `eprintln!` BEFORE the subscriber, which
//!    corrupted the NDJSON purity of stderr whenever a `.env` was
//!    present and `MOAGAN_QUIET` was unset (PR #618 follow-up).
//!
//! The tests drive the real binary through `std::process::Command`
//! (mirroring `tests/integration_coverage_cli.rs`) and assert against
//! the on-disk `events.jsonl` / `log.jsonl` files. The `mock` provider
//! replays canned responses from `tests/fixtures/mock_provider/` so
//! the run completes without any external LLM traffic.

use std::path::Path;
use std::process::Command;

use moagan::test_support::with_moagan_home;

/// Resolve the freshly built `moagan` binary. Mirrors
/// `tests/integration_coverage_cli.rs::moagan_bin` so a plain
/// `cargo test --test integration_run_id_propagation` invocation
/// still finds the binary.
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

/// Extract every `run_id` value from a list of `Event::RunStart` /
/// `Event::RunEnd` lines (matched by `kind == "run_start"` /
/// `kind == "run_end"`).
fn run_ids_by_kind(events: &[serde_json::Value], kind: &str) -> Vec<String> {
    events
        .iter()
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some(kind))
        .filter_map(|v| {
            v.get("run_id")
                .and_then(|r| r.as_str())
                .map(|s| s.to_owned())
        })
        .collect()
}

/// Drive a `moagan run --provider mock:mock-model` invocation with
/// the given `MOAGAN_QUIET` setting, capture stdout (events) and
/// stderr (tracing) to files under `work`, and return the parsed
/// `RunStart` / `RunEnd` event lines plus the first stderr line.
fn drive_mock_run(home: &Path, work: &Path, moagan_quiet: bool) -> RunOutput {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let stdout_path = work.join("events.jsonl");
    let stderr_path = work.join("log.jsonl");
    let mut cmd = Command::new(moagan_bin());
    cmd.env("MOAGAN_HOME", home)
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
        .arg("jsonl");
    if moagan_quiet {
        cmd.env_remove("MOAGAN_QUIET");
        // Explicit set after env_remove so a leaked env var from a
        // sibling test does not silently bypass the suppression path.
        cmd.env("MOAGAN_QUIET", "1");
    } else {
        cmd.env_remove("MOAGAN_QUIET");
    }
    let output = cmd
        .stdout(std::fs::File::create(&stdout_path).expect("create events.jsonl"))
        .stderr(std::fs::File::create(&stderr_path).expect("create log.jsonl"))
        .output()
        .expect("spawn moagan run");
    assert!(
        output.status.success(),
        "moagan run must exit 0; status={:?}; stdout={}; stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = read_jsonl(&stdout_path);
    let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    RunOutput {
        events,
        stderr_text,
    }
}

/// Captured output of a single `moagan run` invocation.
struct RunOutput {
    /// Parsed JSONL events from stdout.
    events: Vec<serde_json::Value>,
    /// Raw stderr text (NDJSON expected under `--log-format json`).
    /// Held so a future test can extend the harness with
    /// stderr-side assertions without re-spawning the binary;
    /// currently unused.
    #[allow(dead_code)]
    stderr_text: String,
}

/// `Event::RunStart.run_id` carries the real UUID v7 produced by
/// the pipeline, NOT the historic `"pre-dispatch"` placeholder.
/// The test drives a `mock:mock-model` fast-mode run against the
/// canned fixtures and asserts the `run_start` event's `run_id`
/// field is a UUID-shaped string.
#[test]
fn run_id_propagates_to_run_start_event() {
    with_moagan_home("run_id_propagates_start", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_mock_run(home, work.path(), false);
        let start_ids = run_ids_by_kind(&out.events, "run_start");
        assert_eq!(
            start_ids.len(),
            1,
            "exactly one run_start event expected, got {start_ids:?}"
        );
        let id = &start_ids[0];
        assert_ne!(
            id, "pre-dispatch",
            "run_start.run_id must NOT be the legacy placeholder; got {id:?}"
        );
        assert!(
            is_uuid_v7_string(id),
            "run_start.run_id must be a UUID-shaped string; got {id:?}"
        );
    });
}

/// `Event::RunEnd.run_id` matches the `run_start` id emitted
/// earlier in the same stream. Pre-`c1` both events hard-coded
/// `"pre-dispatch"`, so the equality held trivially (but the id
/// pointed at nothing on disk). The test pins the post-`c1`
/// contract: both events reference the SAME real run id.
#[test]
fn run_id_propagates_to_run_end_event() {
    with_moagan_home("run_id_propagates_end", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let out = drive_mock_run(home, work.path(), false);
        let start_ids = run_ids_by_kind(&out.events, "run_start");
        let end_ids = run_ids_by_kind(&out.events, "run_end");
        assert_eq!(start_ids.len(), 1, "expected one run_start: {start_ids:?}");
        assert_eq!(end_ids.len(), 1, "expected one run_end: {end_ids:?}");
        assert_eq!(
            start_ids[0], end_ids[0],
            "run_end.run_id must match run_start.run_id; got start={} end={}",
            start_ids[0], end_ids[0]
        );
        assert_ne!(start_ids[0], "pre-dispatch");
        assert!(is_uuid_v7_string(&start_ids[0]));
    });
}

/// Heuristic check for a UUID-shaped string. UUID v7 canonical form
/// is `xxxxxxxx-xxxx-7xxx-xxxx-xxxxxxxxxxxx` (36 chars, version
/// nibble = `7`). The check is intentionally loose — we accept
/// either UUID v7 canonical form or the hyphenless simple form so
/// the test is resilient against minor future shape tweaks.
fn is_uuid_v7_string(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[8] != b'-' || bytes[13] != b'-' || bytes[18] != b'-' || bytes[23] != b'-' {
        return false;
    }
    // Version nibble: the first hex digit of the third group.
    bytes[14] == b'7'
}

/// The pre-`c1` boot path emitted `[moagan] loaded .env from <path>`
/// via `eprintln!` BEFORE `init_tracing()`, corrupting NDJSON
/// purity on stderr. After `c1` the notice flows through
/// `tracing::info!(target: "moagan::boot", …)` AFTER
/// `init_tracing()`, so the first non-empty stderr line is
/// parseable JSONL with the boot target and message.
///
/// The test seeds a `.env` file in the working directory (the
/// dotenvy autoload picks up any `.env` in `cwd`), drives the
/// binary with `--log-format json`, and asserts both the new
/// JSONL shape and the absence of the old plain-text line.
#[test]
fn dotenvy_load_message_respects_log_format() {
    with_moagan_home("dotenvy_message_json", |home| {
        // Seed a `.env` file under a temp working directory so
        // `dotenvy::dotenv()` picks it up regardless of the
        // operator's actual cwd. `dotenvy` walks up from `cwd`
        // by default, so dropping the file directly into the
        // cargo test working directory is enough.
        let work = tempfile::tempdir().expect("workdir");
        let env_path = work.path().join(".env");
        std::fs::write(
            &env_path,
            "MOAGAN_DOTENV_TEST_MARKER=1\n# an empty marker so dotenvy treats the file as present\n",
        )
        .unwrap();
        let stderr_path = work.path().join("log.jsonl");
        let stdout_path = work.path().join("events.jsonl");

        // The test runs from `work.path()` so `dotenvy` finds
        // the seeded `.env` exactly there. `Command::new` lets
        // us set `current_dir` for the child process without
        // disturbing the parent's cwd. `inspect --limit 1` is a
        // read-only subcommand that always exits 0 on a fresh
        // home (no runs yet) — unlike `doctor`, which surfaces
        // provider/env warnings as non-zero on CI.
        let mut cmd = Command::new(moagan_bin());
        cmd.env("MOAGAN_HOME", home)
            .env_remove("MOAGAN_QUIET")
            .current_dir(work.path())
            .arg("inspect")
            .arg("--limit")
            .arg("1")
            .arg("--log-format")
            .arg("json");
        let output = cmd
            .stdout(std::fs::File::create(&stdout_path).expect("create stdout"))
            .stderr(std::fs::File::create(&stderr_path).expect("create stderr"))
            .output()
            .expect("spawn moagan inspect");
        assert!(
            output.status.success(),
            "moagan inspect must exit 0; status={:?}; stderr={}",
            output.status.code(),
            std::fs::read_to_string(&stderr_path).unwrap_or_default()
        );
        let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();

        // Negative assertion: the legacy plain-text line must
        // NOT appear anywhere on stderr. A regression here is
        // the loudest possible signal that the boot-path fix
        // reverted.
        assert!(
            !stderr_text.contains("[moagan] loaded .env"),
            "stderr must not contain the legacy plain-text '[moagan] loaded .env ...' line; \
             pre-c1 eprintln! leaked into NDJSON. stderr was:\n{stderr_text}"
        );

        // PR-04a (E-1) stream routing flip: the boot event is an
        // INFO-level tracing event, which under v0.12.0 routes to
        // stdout (NOT stderr as it did under v0.11). The test
        // still pins the JSONL contract — the boot event is
        // emitted via `tracing::info!`, not `eprintln!` — but the
        // stream it scans is now stdout.
        let stdout_text = std::fs::read_to_string(&stdout_path).expect("read stdout");
        let boot_lines: Vec<&str> = stdout_text
            .lines()
            .filter(|l| l.contains("\"target\":\"moagan::boot\""))
            .collect();
        assert!(
            !boot_lines.is_empty(),
            "stdout must contain a JSONL event with target=moagan::boot under PR-04a routing; got:\n{stdout_text}"
        );
        // And the boot event must NOT have leaked onto stderr —
        // the routing flip reserves stderr for ERROR-level only.
        assert!(
            !stderr_text.contains("\"target\":\"moagan::boot\""),
            "boot event must NOT leak to stderr under PR-04a routing; stderr:\n{stderr_text}"
        );
        // Parse the first boot event to confirm it is valid
        // JSONL AND carries the expected message field.
        let first_boot = boot_lines[0];
        let v: serde_json::Value = serde_json::from_str(first_boot).unwrap_or_else(|e| {
            panic!("boot event not parseable as JSONL: {e}\nline={first_boot}")
        });
        assert_eq!(
            v.get("target").and_then(|t| t.as_str()),
            Some("moagan::boot"),
            "boot event target must be 'moagan::boot'; got {v:?}"
        );
        let msg = v
            .pointer("/fields/message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        assert!(
            msg.contains("main: .env loaded"),
            "boot event message must contain 'main: .env loaded'; got {msg:?}"
        );
    });
}

/// Companion test for the `MOAGAN_QUIET=1` boot-path suppression
/// contract. When the env var is set, the operator-facing notice
/// must be silenced (matching the pre-`c1` legacy behaviour)
/// even though `dotenvy` still autoloads the file. The test
/// seeds a `.env` so the autoload path is exercised, sets
/// `MOAGAN_QUIET=1`, and asserts no `moagan::boot` line lands on
/// stderr.
#[test]
fn dotenvy_load_message_suppressed_by_moagan_quiet() {
    with_moagan_home("dotenvy_message_quiet", |home| {
        let work = tempfile::tempdir().expect("workdir");
        let env_path = work.path().join(".env");
        std::fs::write(&env_path, "MOAGAN_DOTENV_TEST_MARKER=1\n").unwrap();
        let stderr_path = work.path().join("log.jsonl");
        let stdout_path = work.path().join("events.jsonl");
        let mut cmd = Command::new(moagan_bin());
        cmd.env("MOAGAN_HOME", home)
            .env("MOAGAN_QUIET", "1")
            .current_dir(work.path())
            .arg("inspect")
            .arg("--limit")
            .arg("1")
            .arg("--log-format")
            .arg("json");
        let output = cmd
            .stdout(std::fs::File::create(&stdout_path).expect("create stdout"))
            .stderr(std::fs::File::create(&stderr_path).expect("create stderr"))
            .output()
            .expect("spawn moagan inspect (quiet)");
        assert!(output.status.success(), "inspect must exit 0");
        let stderr_text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let stdout_text = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        // PR-04a (E-1): the boot notice is an INFO-level tracing
        // event, which routes to stdout under v0.12.0. Pre-flip
        // the test could pin the contract by checking stderr only
        // (the legacy routing); post-flip we have to pin BOTH
        // streams so a regression that re-emits the boot event
        // anywhere surfaces.
        let stderr_boot_lines: Vec<&str> = stderr_text
            .lines()
            .filter(|l| l.contains("\"target\":\"moagan::boot\""))
            .collect();
        let stdout_boot_lines: Vec<&str> = stdout_text
            .lines()
            .filter(|l| l.contains("\"target\":\"moagan::boot\""))
            .collect();
        assert!(
            stderr_boot_lines.is_empty(),
            "MOAGAN_QUIET=1 must suppress the moagan::boot notice on stderr; got: {stderr_boot_lines:?}"
        );
        assert!(
            stdout_boot_lines.is_empty(),
            "MOAGAN_QUIET=1 must suppress the moagan::boot notice on stdout (PR-04a routing); got: {stdout_boot_lines:?}"
        );
        assert!(
            !stderr_text.contains("[moagan] loaded .env"),
            "MOAGAN_QUIET=1 must keep the legacy eprintln! suppressed; got:\n{stderr_text}"
        );
    });
}

/// v0.11.2 audit-fix pin: every event emitted DURING dispatch that
/// carries a `pipeline` span MUST also carry a non-null `run_id`
/// inside that span. The pre-v0.11.2 implementation declared
/// `run_id = tracing::field::Empty` on the span and called
/// `Span::record` AFTER `cli::dispatch` returned; by that point
/// ~98.9% of events had already inherited a null `run_id`. The
/// fix pre-allocates the `RunId` in `run_with_cli` and constructs
/// the span with `run_id = %run_id` so events emitted during
/// dispatch (intake, phases, llm_call, probe, …) inherit a real
/// UUID v7. This test pins the contract end-to-end by running the
/// mock fast pipeline and counting events whose `pipeline` span
/// has a null `run_id` — the count must be zero.
#[test]
fn run_id_present_in_pipeline_span_for_every_event() {
    with_moagan_home("run_id_in_pipeline_span", |home| {
        let work = tempfile::tempdir().expect("workdir");
        // The drive helper captures stderr into `log.jsonl` only
        // when `moagan_quiet` is false and the harness pipes
        // stderr to a file. We re-spawn the binary with explicit
        // paths so the assertion can read the JSONL stream
        // independently of stdout events.
        let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("mock_provider");
        let stderr_path = work.path().join("log.jsonl");
        let stdout_path = work.path().join("events.jsonl");
        let mut cmd = Command::new(moagan_bin());
        cmd.env("MOAGAN_HOME", home)
            .env_remove("MOAGAN_QUIET")
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
            .arg("jsonl");
        let output = cmd
            .stdout(std::fs::File::create(&stdout_path).expect("create events.jsonl"))
            .stderr(std::fs::File::create(&stderr_path).expect("create log.jsonl"))
            .output()
            .expect("spawn moagan run");
        assert!(
            output.status.success(),
            "moagan run must exit 0; status={:?}; stdout={}; stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // PR-04a (E-1) stream routing flip: the tracing JSONL events
        // (which carry the `pipeline` span list) now route to
        // stdout, not stderr. We scan stdout for the spans instead
        // of stderr — the v0.11 contract under which this test
        // was originally written had every tracing event on
        // stderr, so the assertion silently produced 0 events.
        let stdout_text = std::fs::read_to_string(&stdout_path).expect("read stdout");
        let stderr_text = std::fs::read_to_string(&stderr_path).expect("read stderr");

        // Count every JSONL line on stdout that carries a
        // `pipeline` span with a null `run_id`. Pre-v0.11.2 this
        // would have been ~98.9% of events; post-fix it must be
        // zero because the span is constructed with `run_id =
        // %candidate_run_id` from the very start.
        let mut null_count: usize = 0;
        let mut pipeline_count: usize = 0;
        let mut null_examples: Vec<String> = Vec::new();
        for line in stdout_text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // tolerate any non-JSONL diagnostic
            };
            let Some(spans) = v.get("spans").and_then(|s| s.as_array()) else {
                continue;
            };
            for span in spans {
                if span.get("name").and_then(|n| n.as_str()) != Some("pipeline") {
                    continue;
                }
                pipeline_count += 1;
                let run_id_null = span.get("run_id").map(|r| r.is_null()).unwrap_or(true);
                if run_id_null {
                    null_count += 1;
                    if null_examples.len() < 3 {
                        null_examples.push(trimmed.to_owned());
                    }
                }
            }
        }
        assert!(
            pipeline_count > 0,
            "expected the mock fast run to emit at least one event with a `pipeline` span on stdout; got 0; stderr=\n{stderr_text}"
        );
        assert_eq!(
            null_count,
            0,
            "every `pipeline` span must carry a non-null `run_id`; got {null_count}/{pipeline_count} null. Examples:\n{}",
            null_examples.join("\n")
        );
    });
}

/// v0.11.2 audit-fix pin: `Event::RunEnd.status` must reflect the
/// dispatcher exit code. Pre-v0.11.2 the bus always emitted
/// `status: "ok"`, which lied when the run exited non-zero (e.g.
/// a missing or invalid `--provider`). The fix makes
/// `resolved_status` a conditional: `"ok"` iff `exit_code == 0`,
/// otherwise `"error"`. This test drives a failing run
/// (`--provider ""` — the dispatcher rejects empty providers with
/// `Error::InvalidArgs` and exit code 2) and asserts the
/// `RunEnd` event carries `status: "error"`. A positive control
/// run with a valid provider confirms `status: "ok"` on success.
#[test]
fn run_end_status_reflects_non_zero_exit_code() {
    with_moagan_home("run_end_status", |home| {
        // ---- Negative case: invalid --provider -> exit 2, status "error" ----
        let work_err = tempfile::tempdir().expect("workdir err");
        let stdout_err = work_err.path().join("events.jsonl");
        let stderr_err = work_err.path().join("log.jsonl");
        let mut cmd_err = Command::new(moagan_bin());
        cmd_err
            .env("MOAGAN_HOME", home)
            .env_remove("MOAGAN_QUIET")
            .arg("run")
            .arg("--mode")
            .arg("fast")
            .arg("--provider")
            .arg("") // empty -> InvalidArgs, exit 2
            .arg("--prompt")
            .arg("x")
            .arg("--non-interactive")
            .arg("--log-format")
            .arg("json")
            .arg("--event-format")
            .arg("jsonl");
        let output_err = cmd_err
            .stdout(std::fs::File::create(&stdout_err).expect("create events.jsonl"))
            .stderr(std::fs::File::create(&stderr_err).expect("create log.jsonl"))
            .output()
            .expect("spawn moagan run (invalid provider)");
        assert_eq!(
            output_err.status.code(),
            Some(2),
            "empty --provider must surface as exit code 2 (InvalidArgs); got {:?}; stderr={}",
            output_err.status.code(),
            String::from_utf8_lossy(&output_err.stderr)
        );
        let events_err = read_jsonl(&stdout_err);
        let run_end_err: Vec<&serde_json::Value> = events_err
            .iter()
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("run_end"))
            .collect();
        assert_eq!(
            run_end_err.len(),
            1,
            "exactly one run_end expected on failure; got {run_end_err:?}"
        );
        let status_err = run_end_err[0]
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let exit_code_err = run_end_err[0]
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .unwrap_or(-1);
        assert_eq!(
            status_err, "error",
            "run_end.status must be \"error\" when exit_code != 0; got {status_err:?} (exit_code={exit_code_err})"
        );
        assert_eq!(
            exit_code_err, 2,
            "run_end.exit_code must echo the process exit code 2; got {exit_code_err}"
        );

        // ---- Positive control: valid mock provider -> exit 0, status "ok" ----
        let work_ok = tempfile::tempdir().expect("workdir ok");
        let stdout_ok = work_ok.path().join("events.jsonl");
        let stderr_ok = work_ok.path().join("log.jsonl");
        let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("mock_provider");
        let mut cmd_ok = Command::new(moagan_bin());
        cmd_ok
            .env("MOAGAN_HOME", home)
            .env_remove("MOAGAN_QUIET")
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
            .arg("jsonl");
        let output_ok = cmd_ok
            .stdout(std::fs::File::create(&stdout_ok).expect("create events.jsonl"))
            .stderr(std::fs::File::create(&stderr_ok).expect("create log.jsonl"))
            .output()
            .expect("spawn moagan run (mock ok)");
        assert!(
            output_ok.status.success(),
            "mock fast run must exit 0; status={:?}; stderr={}",
            output_ok.status.code(),
            String::from_utf8_lossy(&output_ok.stderr)
        );
        let events_ok = read_jsonl(&stdout_ok);
        let run_end_ok: Vec<&serde_json::Value> = events_ok
            .iter()
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("run_end"))
            .collect();
        assert_eq!(run_end_ok.len(), 1, "exactly one run_end on success");
        let status_ok = run_end_ok[0]
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        assert_eq!(
            status_ok, "ok",
            "run_end.status must be \"ok\" on success; got {status_ok:?}"
        );
    });
}
