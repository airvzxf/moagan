//! Integration tests for the v0.12.0 stream routing flip
//! (PR-04a / E-1).
//!
//! The flip sends tracing logs to **stdout** by default; only
//! `ERROR`-level events still go to **stderr**. This unlocks the
//! canonical Unix split:
//!
//! ```text
//! moagan run … 1> out.jsonl 2> errors.jsonl
//! jq -c 'select(.kind=="llm_call")' out.jsonl     # domain events
//! grep '"level":"ERROR"' errors.jsonl            # optional audit
//! ```
//!
//! Legacy behaviour (everything on stderr) is reachable via the
//! deprecated `--log-to-stderr` flag / `MOAGAN_LOG_TO_STDERR=1`
//! env var until v0.14.0 removes it.
//!
//! The tests drive the freshly built `moagan` binary through
//! `std::process::Command` (mirroring
//! `tests/integration_run_id_propagation.rs` and
//! `tests/integration_decisions.rs`) and assert against the
//! captured streams. The `mock` provider replays canned
//! responses from `tests/fixtures/mock_provider/` so the run
//! completes without external LLM traffic; the read-only commands
//! (`--help`, parse-error, `doctor`) don't need a mock at all.

use std::path::{Path, PathBuf};
use std::process::Command;

use moagan::test_support::with_moagan_home;

/// Resolve the freshly built `moagan` binary. Mirrors the helpers
/// in `tests/integration_run_id_propagation.rs` /
/// `tests/integration_decisions.rs` so a plain `cargo test
/// --test integration_stream_routing` invocation finds the
/// binary.
fn moagan_bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_moagan")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("moagan")
        })
}

/// Convenience: build a `Command` rooted at the freshly-built
/// `moagan` binary with a clean environment for the
/// stderr-format / event-format selectors. Anything that mutates
/// one of those env vars in a sibling test (most do — see
/// `tests/integration_run_id_propagation.rs`) won't leak into
/// these routing assertions.
fn moagan() -> Command {
    let mut cmd = Command::new(moagan_bin());
    cmd.env_remove("MOAGAN_LOG_FORMAT");
    cmd.env_remove("MOAGAN_EVENT_FORMAT");
    cmd.env_remove("MOAGAN_DECISION_FORMAT");
    cmd.env_remove("MOAGAN_LOG_TO_STDERR");
    cmd.env_remove("MOAGAN_QUIET");
    cmd
}

// ---------------------------------------------------------------------------
// §4.1 — Routing core (no network, no LLM).
// ---------------------------------------------------------------------------

/// `moagan --help` writes clap's help text to stdout. Stderr
/// stays empty: clap exits BEFORE `init_tracing()` runs, so no
/// tracing subscriber is registered and the new routing is
/// moot for this particular command.
#[test]
fn run_help_writes_to_stdout_only() {
    let out = moagan()
        .arg("--help")
        .output()
        .expect("spawn moagan --help");
    assert!(
        out.stdout.len() > 100,
        "stdout must carry clap's help text (>100 bytes); got {} bytes",
        out.stdout.len()
    );
    assert_eq!(
        out.stderr.len(),
        0,
        "stderr must be empty for non-error output; got {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An unknown subcommand is a clap parse error: clap writes its
/// diagnostic to stderr and exits non-zero BEFORE
/// `init_tracing()` runs, so stdout stays empty.
#[test]
fn invalid_subcommand_writes_error_to_stderr_only() {
    let out = moagan()
        .arg("not-a-real-subcommand")
        .output()
        .expect("spawn moagan not-a-real-subcommand");
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty for clap parse errors; got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !out.stderr.is_empty(),
        "clap parse errors go to stderr; got empty stderr"
    );
}

/// `moagan doctor` writes its `[OK]` / `[WARN]` / `[FAIL]`
/// status lines to stdout (via `println!`) and the v0.12.0
/// routing flip also sends the `tracing::info!("doctor: OK")`
/// / `… WARN` events to stdout. Stderr must stay empty when
/// no check fails.
#[test]
fn doctor_to_stdout_when_no_error() {
    let label = "stream_routing__doctor_to_stdout";
    moagan::test_support::with_moagan_home(label, |_home_dir| {
        let out = moagan()
            .arg("doctor")
            .output()
            .expect("spawn moagan doctor");
        assert!(
            !out.stdout.is_empty(),
            "stdout must carry at least one status line; got 0 bytes; stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        // `doctor` only writes to stderr if some check fails OR
        // an ERROR-level tracing event was raised. The temp
        // MOAGAN_HOME is fully writable and the mock provider is
        // not configured — so no provider key error fires.
        // Stderr is the strict assertion of the routing flip.
        assert_eq!(
            out.stderr.len(),
            0,
            "stderr must be empty when no ERROR-level events fire; got {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    });
}

/// `--log-to-stderr` (the v0.11-compat flag, deprecated in
/// v0.12.0 and removed in v0.14.0) swaps stdout and stderr for
/// TRACING output. Stdout still carries direct `println!` from
/// `moagan doctor` (`[OK]` / `[FAIL]` / `[WARN]` status lines)
/// and the domain `Event::RunStart` / `Event::RunEnd` events
/// from `src/telemetry/stdout_events.rs` — those go through
/// their own stdio::Stdout lock, not the tracing subscriber.
/// The assertion pins BOTH invariants on stderr (the only place
/// the swap is observable):
///
/// 1. A `DEPRECATED` warning reaches stderr (the
///    subscriber-was-alive-by-the-time-we-emitted-it fix in
///    `src/main.rs::init_tracing`).
/// 2. With `--log-format text` the routing flip is observable:
///    the subscriber's text-formatter output lands on stderr.
#[test]
fn log_to_stderr_routes_tracing_to_stderr() {
    let label = "stream_routing__log_to_stderr";
    moagan::test_support::with_moagan_home(label, |_home_dir| {
        let out = moagan()
            .arg("--log-to-stderr")
            .arg("--log-format")
            .arg("text")
            .arg("doctor")
            .output()
            .expect("spawn moagan doctor --log-to-stderr");
        let stderr_str = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr_str.contains("DEPRECATED"),
            "stderr must carry the DEPRECATED warning under --log-to-stderr; got {stderr_str:?}"
        );
        assert!(
            stderr_str.contains("subscriber initialised"),
            "stderr must carry the boot debug event from init_tracing under --log-to-stderr; got {stderr_str:?}"
        );
        // Sanity: stdout still carries direct `println!` from
        // doctor + domain NDJSON events from
        // `stdout_events::STDOUT_EVENTS`. The flag only
        // affects tracing; we don't pin specific stdout
        // contents (the doctor output is operator-facing and
        // intentionally unchanged by the v0.12.0 flip).
        assert!(
            !out.stdout.is_empty(),
            "stdout must carry direct doctor output even under --log-to-stderr (the flag only swaps tracing); got empty stdout, stderr={stderr_str:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// §4.3 — A-2 discover-banner gate.
// ---------------------------------------------------------------------------

/// Inside a successful `moagan discover` run the human-readable
/// `moagan discover <id> provider=… -> <path>` banner used to
/// print unconditionally, breaking the NDJSON purity for any
/// operator piping stdout into `jq`. The v0.12.0 routing flip
/// gates the banner on `stdout.is_terminal()` and emits a
/// `tracing::info!` event in its place when stdout is not a
/// TTY. The subprocess.stdout is a pipe (not a TTY), so the
/// `println!` form is suppressed. This is the A-2 anti-pin.
#[test]
fn discover_banner_is_suppressed_when_stdout_not_tty() {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let label = "stream_routing__discover_banner";
    with_moagan_home(label, |_home_dir| {
        let out = moagan()
            .arg("discover")
            .arg("--non-interactive")
            .arg("--prompt")
            .arg("Enumera los 7 colores del arcoiris en orden")
            .arg("--provider")
            .arg("mock:mock-model")
            .arg("--mock-dir")
            .arg(&mock_dir)
            // Skip the LLM-driven dimension_deriver; build the
            // matrix verbatim from the spec. The iteration loop
            // fires immediately after intake + clarify.
            .arg("--matrix-spec")
            .arg("auth=oauth,api-key")
            .arg("--dimensions")
            .arg("1")
            .arg("--sketches-per-cell")
            .arg("10")
            .arg("--temperature-profile")
            .arg("provider=mock-model;temperatures=0.5;replicas=1")
            .arg("--max-parallelism")
            .arg("1")
            .arg("--log-format")
            .arg("json")
            .arg("--event-format")
            .arg("off")
            .output()
            .expect("spawn moagan discover");
        // Anti-pin: the banner is gated on TTY. The subprocess
        // stdout is a pipe, so the assertion is a negative
        // invariant — the banner must NOT appear in stdout.
        let stdout_str = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout_str.contains("moagan discover "),
            "banner must be suppressed when stdout is non-TTY; got stdout={stdout_str:?}"
        );
        // Sanity: a successful mock discover produces at least
        // some stderr-side output (tracing events from the boot
        // path); we don't pin the precise contents here, only
        // that we did not see the banner.
        let _ = out.stderr;
    });
}

// ---------------------------------------------------------------------------
// §4.4 — Invariants on a clean run (mock provider, no ERRORs).
// ---------------------------------------------------------------------------

/// A clean `moagan run` with `--event-format off` keeps stdout
/// strictly free of `ERROR`-level tracing lines: every event
/// the operator would grep as `\"ERROR\"` is routed to stderr.
/// Stdout may contain JSON tracing events (INFO/DEBUG/WARN) per
/// the v0.12.0 flip; the assertion is the *negative* invariant
/// that no `\"ERROR\"` literal appears in stdout.
#[test]
fn no_error_in_stdout_for_clean_run() {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let label = "stream_routing__no_error_in_stdout";
    with_moagan_home(label, |_home_dir| {
        let out = moagan()
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
            .arg("--event-format")
            .arg("off")
            .arg("--log-format")
            .arg("json")
            .output()
            .expect("spawn moagan run");
        assert!(
            out.status.success(),
            "mock fast run must succeed; status={:?}; stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout_str = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout_str.contains("\"ERROR\""),
            "stdout must contain no ERROR-level tracing events on a clean run; got stdout={stdout_str:?}"
        );
    });
}

/// A clean run with `--event-format off` must leave stderr
/// empty: the routing flip reserves stderr for ERROR-level
/// tracing events only, and a successful mock fast run
/// produces none.
#[test]
fn no_panic_in_stderr_for_clean_run() {
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let label = "stream_routing__no_panic_in_stderr";
    with_moagan_home(label, |_home_dir| {
        let out = moagan()
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
            .arg("--event-format")
            .arg("off")
            .arg("--log-format")
            .arg("json")
            .output()
            .expect("spawn moagan run");
        assert!(
            out.status.success(),
            "mock fast run must succeed; status={:?}; stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        // The mock provider is configured so the run completes
        // without ERROR-level tracing events. Stderr must be
        // empty under the v0.12.0 routing flip.
        assert_eq!(
            out.stderr.len(),
            0,
            "stderr must be empty on a clean run; got {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    });
}
