//! Integration tests pinning the four bugs that the shell scripts in
//! `scripts/` hit and had to fix in-line. Running these locally is the
//! fast feedback path; the live equivalents live under
//! `scripts/e2e_audit_proxy.sh` and need real API tokens to run.
//!
//! Bug inventory (mirrors the prompt that requested this file):
//!
//! - **#1 — banner / wrapper extraction**: `moagan audit proxy`
//!   prints its banner
//!   `proxy listening on http://127.0.0.1:PORT -> UPSTREAM` to
//!   **stderr** (`src/cli/audit.rs:138-144`). With v0.12.0 stream
//!   routing, `tracing::debug!` lines from `src/main.rs:343` (and
//!   friends) go to **stdout**. The original wrapper extracted the
//!   port with `head -1` from the merged stream, which captured a
//!   debug line instead of the banner. The fix in the shell scripts
//!   is `grep -m1 'proxy listening'`. We pin the Rust invariant here:
//!   the banner is locatable by **pattern**, not by position.
//!
//! - **#2 — bare `--provider minimax` without `:MODEL`**: rejected
//!   with `Error::InvalidArgs` whenever the resolved section has
//!   more than one model (`src/cli/mod.rs:1393-1422`). The default
//!   `minimax` section ships with 4 models
//!   (`src/config/mod.rs:1057-1062`), so a bare `--provider minimax`
//!   always fails. The error message includes `requires a model id`.
//!
//! - **#3 — `wire_format_from_url` rejects non-canonical
//!   endpoints**: `src/llm/wire_format.rs:467-487` demands that the
//!   endpoint ends in `/messages`, `/chat/completions`, or
//!   `/responses`; anything else surfaces as `Error::InvalidArgs`
//!   with the substring `no recognised wire-format suffix`. The
//!   asymmetry the script has to know about:
//!   `MOAGAN_MINIMAX_ENDPOINT` MUST end in `/messages`, but
//!   `audit proxy --upstream` MUST NOT (the proxy's
//!   `join_upstream` in `src/audit/proxy.rs:1161-1206` appends the
//!   request path).
//!
//! - **#4 — `max_tokens` cap chain in `MinimaxProvider::send`**: the
//!   three-layer clamp at `src/llm/minimax.rs:398-422` pins the
//!   wire body to `operator_cap.min(table_cap).min(MINIMAX_MAX_TOKENS_CAP)`,
//!   where `MINIMAX_MAX_TOKENS_CAP = 524_288`
//!   (`src/llm/capabilities.rs:35`) and the default
//!   `ModelConfig::max_tokens` is `1_000_000`
//!   (`src/config/mod.rs:1074`). MiniMax-M2.7 rejects values above
//!   131072 with HTTP 400, so the operator-side fix is a TOML with
//!   `max_tokens = 131072` per model. The test pins both: the TOML
//!   override clamps the wire body, and the mock upstream rejects
//!   anything above 131072 with a 400 — so a regression in the
//!   clamp surfaces as a 400 in `server.received_requests()` and a
//!   failed assertion on the largest observed `max_tokens`.
//!
//! The `tests/integration_audit_e2e.rs` harness is the structural
//! reference for spawning the binary, draining stderr into a
//! `tokio::io::Lines`, and the SIGTERM/exit-0 lifecycle. Tests in
//! this file reuse its `wait_for_proxy_port` / `binary` shape but
//! stay self-contained so a future refactor of the reference file
//! cannot silently lose coverage here.

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// Parse a banner line like
/// `proxy listening on http://127.0.0.1:54321 -> https://upstream`.
/// Returns `None` when no parseable port is present so the caller
/// can keep scanning rather than panicking — the prompt contract is
/// "locate by pattern", not "the first line must match".
fn parse_proxy_port(line: &str) -> Option<u16> {
    let after_scheme = line.split("http://").nth(1)?;
    // The address is delimited by the first whitespace character or
    // the ` ->` separator the eprintln! uses. Stopping at either
    // lets the parser tolerate trailing text that
    // `tokio::io::Lines` may have left in the buffer.
    let address = after_scheme
        .split([' ', '\t'])
        .next()?
        .split(" ->")
        .next()?;
    let port_str = address.rsplit(':').next()?;
    let trimmed: String = port_str
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

/// Drain stderr into a `Lines` until the proxy banner appears, then
/// return the parsed port. The banner is matched by substring; the
/// bug we're pinning is the assumption that it sits on line 1.
async fn wait_for_proxy_port<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
) -> u16 {
    loop {
        let line = lines
            .next_line()
            .await
            .expect("read proxy stderr")
            .expect("proxy exited before announcing address");
        if line.contains("proxy listening on http://")
            && let Some(port) = parse_proxy_port(&line)
        {
            return port;
        }
    }
}

/// Anthropic-shaped body that the judge LLM is expected to produce
/// in `Mode::Fast`. Mirrors `judge_json()` in
/// `tests/integration_audit_e2e.rs` so a future refactor of either
/// side stays in sync.
fn judge_response() -> &'static str {
    r#"{"score":9.0,"criteria":{"correctness":9.0,"completeness":9.0,"fit":9.0,"evidence":9.0,"clarity":9.0},"comments":"ok"}"#
}

/// Mount a wiremock at `/anthropic/v1/messages` that:
///
/// - inspects the body and rejects `max_tokens > 131072` with an
///   Anthropic-style 400 (the M2.7 ceiling), preserving the bug
///   regression signal;
/// - returns a valid Anthropic-shape 200 otherwise.
///
/// The 200 body is shaped so the `judge` role in the pipeline can
/// parse a numeric `score`; the propose / synthesize roles accept
/// freeform text and ignore the body shape.
async fn mount_minimax_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            let max_tokens = body
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if max_tokens > 131_072 {
                ResponseTemplate::new(400).set_body_string(
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens exceeds model ceiling (131072)"}}"#,
                )
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "content": [{"type": "text", "text": judge_response()}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0
                    }
                }))
            }
        })
        .mount(server)
        .await;
}

/// Read every recorded request body on the mock and extract the
/// maximum `max_tokens` value. Returns 0 when the mock saw zero
/// requests so a caller assertion can distinguish "no traffic"
/// (`== 0`) from "traffic but all under the cap" (`> 0 && <= cap`).
async fn max_tokens_seen_by_mock(server: &MockServer) -> u64 {
    let requests = server.received_requests().await.unwrap_or_default();
    let mut max_seen = 0u64;
    for req in requests {
        if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&req.body)
            && let Some(v) = body.get("max_tokens").and_then(|v| v.as_u64())
        {
            max_seen = max_seen.max(v);
        }
    }
    max_seen
}

/// Find the latest run directory written under `<root>/.runs`.
/// Mirrors the helper in `tests/integration_audit_e2e.rs`; kept
/// local so this file does not depend on that file's internal
/// helpers.
fn latest_run_dir(root: &Path) -> Option<PathBuf> {
    let runs = root.join(".runs");
    let mut entries: Vec<_> = std::fs::read_dir(&runs)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            if path.is_dir() {
                Some((name, path))
            } else {
                None
            }
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries.into_iter().next().map(|(_, path)| path)
}

/// Write a minimal `moagan.toml` overriding the default `minimax`
/// section with a single model capped at 131072. The point of
/// `[[providers.minimax.models]]` here (rather than relying on the
/// default which carries `max_tokens = 1_000_000`) is to pin the
/// operator-side fix for bug #4. Endpoint is set to the canonical
/// MiniMax Anthropic URL so `MOAGAN_MINIMAX_ENDPOINT` env rewrite
/// has a `/messages`-bearing anchor to match.
fn write_minimax_cap_config(path: &Path) {
    let body = r#"# Minimal override for integration_e2e_script_paths::run_through_audit_proxy_*
# (tests/integration_e2e_script_paths.rs).
#
# Pins bug #4: the operator-side max_tokens cap must clamp the
# wire body so MiniMax-M2.7 does not return HTTP 400. A
# regression that drops this TOML (or that ignores
# `ModelConfig::max_tokens` in `MinimaxProvider::send`) makes
# every request exceed 131072 on the wire and the mock returns
# 400; the test below asserts on the largest observed
# `max_tokens`, so the regression fails explicitly.

[providers.minimax]
endpoint = "https://api.minimax.io/anthropic/v1/messages"

[[providers.minimax.models]]
id = "MiniMax-M2.7"
max_tokens = 131072
"#;
    std::fs::write(path, body).expect("write moagan.toml override");
}

// ---------------------------------------------------------------------------
// Test #1 — Bug #2: bare `--provider minimax` (no `:MODEL`) is rejected
// ---------------------------------------------------------------------------

/// `moagan run --provider minimax` (without `:MODEL`) must fail
/// with the friendly `requires a model id` message because the
/// default `minimax` section ships with 4 models
/// (`src/config/mod.rs:1057-1062`).
///
/// Pin: `src/cli/mod.rs:1393-1422`. The shell scripts hit this in
/// the wild when someone tried to launch without picking a model;
/// the operator-side fix is "always pass `--provider
/// SECTION:MODEL`". This test catches a regression that drops the
/// friendly message and falls back to a generic dispatcher error.
#[test]
fn bare_minimax_provider_is_rejected_with_model_id_hint() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runs_dir = tmp.path().join("runs");
    std::fs::create_dir_all(&runs_dir).expect("mkdir runs");

    // Point MOAGAN_CONFIG at a path that DOES NOT exist inside the
    // tmpdir so `Config::load` falls through to the built-in
    // defaults (4 minimax models). If we pointed at the host
    // `moagan.toml` instead, a custom operator config could hide
    // the 4-model default and the test would silently stop
    // pinning the bug.
    let missing_config = tmp.path().join("missing-config.toml");

    let output = StdCommand::new(binary())
        .current_dir(tmp.path())
        .args([
            "run",
            "--mode",
            "fast",
            "--provider",
            "minimax",
            "--prompt",
            "Smoke test prompt for bare-provider path",
            "--runs-dir",
        ])
        .arg(&runs_dir)
        // The dotenv guard is mandatory: `main.rs:21` calls
        // `dotenvy::dotenv()` and would otherwise load the
        // repo's real `.env`. Setting `current_dir` to a fresh
        // empty tmpdir already breaks the discovery chain
        // (`.env` does not exist in the cwd), and we strip the
        // variable directly to defend against an inherited
        // `MINIMAX_API_KEY` slipping in via the test runner.
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .env("MOAGAN_CONFIG", &missing_config)
        .env("MOAGAN_HOME", &runs_dir)
        .output()
        .expect("spawn moagan run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");

    assert!(
        !output.status.success(),
        "bare `--provider minimax` must fail (status={:?}); \
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        combined.contains("requires a model id"),
        "expected 'requires a model id' in error output; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Test #2 — Bug #3: `MOAGAN_MINIMAX_ENDPOINT` without `/messages` is rejected
// ---------------------------------------------------------------------------

/// `wire_format_from_url` rejects any endpoint that does not end
/// in `/messages`, `/chat/completions`, or `/responses`
/// (`src/llm/wire_format.rs:467-487`). The shell scripts used to
/// hit this when an operator passed `MOAGAN_MINIMAX_ENDPOINT` to
/// a base URL (no `/messages`) — the dispatcher would then fail
/// with `endpoint '...' has no recognised wire-format suffix`.
///
/// The asymmetry the test pins: `--upstream` on the proxy must be
/// a **base** URL (because `join_upstream` appends the request
/// path), but `MOAGAN_MINIMAX_ENDPOINT` must end in `/messages`.
/// This test exercises the LLM-side failure mode.
#[test]
fn minimax_endpoint_without_messages_suffix_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runs_dir = tmp.path().join("runs");
    std::fs::create_dir_all(&runs_dir).expect("mkdir runs");
    let missing_config = tmp.path().join("missing-config.toml");

    // A non-routable loopback IP+port so the test stays cheap if
    // somehow the validator is bypassed and the run actually
    // fires a request. The point of the assertion is the
    // validator error, not the wire behaviour.
    let bad_endpoint = "http://127.0.0.1:1/anthropic/v1";

    let output = StdCommand::new(binary())
        .current_dir(tmp.path())
        .args([
            "run",
            "--mode",
            "fast",
            "--provider",
            "minimax:MiniMax-M2.7",
            "--prompt",
            "Smoke test prompt for wire-format path",
            "--runs-dir",
        ])
        .arg(&runs_dir)
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .env("MOAGAN_CONFIG", &missing_config)
        .env("MOAGAN_HOME", &runs_dir)
        // The override rewrites every section whose endpoint
        // contains `/messages` (`src/config/mod.rs:1841-1860`),
        // so the defaults' `.../v1/messages` is replaced by our
        // bad URL — which lacks the suffix.
        .env("MOAGAN_MINIMAX_ENDPOINT", bad_endpoint)
        .output()
        .expect("spawn moagan run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");

    assert!(
        !output.status.success(),
        "endpoint without /messages must fail (status={:?}); \
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        combined.contains("no recognised wire-format suffix"),
        "expected 'no recognised wire-format suffix' in error \
         output; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Note: we deliberately do NOT assert that the raw
    // `bad_endpoint` string appears verbatim in the error —
    // `RedactWriter` rewrites `127.0.0.1` (a private-IP range)
    // to `[REDACTED:private_ip]` on its way to stderr
    // (`src/redact/patterns.rs:153-155`), so the literal URL
    // will not match. The `no recognised wire-format suffix`
    // substring is enough to pin the validator.
}

// ---------------------------------------------------------------------------
// Test #3 — Bugs #1 + #4: e2e through the audit proxy, real CLI, mock LLM
// ---------------------------------------------------------------------------

/// End-to-end run through the audit proxy. Pins two bugs at once:
///
/// - **Bug #4** is the primary pin: the TOML override forces
///   `max_tokens = 131072` and the mock rejects anything above
///   131072 with HTTP 400. If `MinimaxProvider::send` stops
///   honouring `ModelConfig::max_tokens`, the mock sees a body
///   above the cap and returns 400. The test then asserts
///   `max_tokens_seen <= 131072`, which fails explicitly because
///   the largest observed value is the regression's smoking gun.
///
/// - **Bug #1** is pinned indirectly: the test reads the proxy
///   banner by **pattern** (`wait_for_proxy_port`), not by
///   position. Any future regression that pushes tracing
///   output ahead of the banner on stderr is detectable by
///   reading stderr in a `tokio::io::Lines` loop and matching on
///   the substring.
///
/// Decision on the run-exit-code contract: the mock returns a
/// well-shaped Anthropic body so `Mode::Fast` can finish a full
/// proposal → judge → portfolio cycle. The hard contract this
/// test enforces is **only**:
///   1. the NDJSON `run_start` event fires on stdout;
///   2. the largest observed `max_tokens` over the wire is
///      `<= 131072` (the operator-side cap from the TOML);
///   3. the proxy wrote its gzip audit log under the run dir.
///
/// The pipeline's exit code is **not** asserted: `Mode::Fast`
/// can fail for reasons unrelated to any of the four pinned bugs
/// (judge schema drift, additional required phases, transient
/// process-lifecycle issues under the multi-thread runtime), and
/// none of those failures should mask a real regression on the
/// bugs the test exists to cover. If the run does exit non-zero,
/// the helper at the end of the test prints the exit status and
/// stderr as a diagnostic via `eprintln!` but does not fail the
/// test — the relaxed contract above is by design.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_through_audit_proxy_emits_run_start_and_respects_max_tokens_cap() {
    let server = MockServer::start().await;
    mount_minimax_mock(&server).await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let runs_dir = tmp.path().join("runs");
    std::fs::create_dir_all(&runs_dir).expect("mkdir runs");

    // Pin bug #4: write the operator-side override.
    let config_path = tmp.path().join("moagan.toml");
    write_minimax_cap_config(&config_path);

    // The `temperature` startup auto-probe is disabled via the
    // `MOAGAN_TEMPERATURE_AUTO=false` env var below (issue
    // #657 fix #3, v0.12.15). Pre-v0.12.15 this test had to
    // pre-populate `<MOAGAN_HOME>/temperatures_auto.toml` to
    // avoid the 21-candidate fan-out inflating the wall-clock
    // past 10 s; the workaround function was removed when the
    // env var landed. The mock answers every temperature probe
    // with HTTP 200 so the probe WOULD succeed without the
    // opt-out, but each candidate is still a sequential HTTP
    // round-trip against the proxy.

    // Boot the proxy on an ephemeral loopback port, pointed at the
    // wiremock upstream. `--upstream` is a **base** URL (no
    // `/messages`) — `join_upstream` will append the request path.
    let mut proxy = Command::new(binary())
        .args([
            "audit",
            "proxy",
            "--port",
            "0",
            "--upstream",
            &format!("{}/anthropic/v1", server.uri()),
            "--runs-dir",
        ])
        .arg(&runs_dir)
        .current_dir(tmp.path())
        // Keep the proxy quiet on stdout; the banner is on
        // stderr, which we read explicitly below.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .env("MOAGAN_HOME", &runs_dir)
        .env("MOAGAN_CONFIG", &config_path)
        .env("MOAGAN_MAX_TOKEN_AUTO", "false")
        .env("MOAGAN_MAX_TOKEN_AUTO_SAVE", "false")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn audit proxy");
    let stderr = proxy.stderr.take().expect("proxy stderr pipe");
    let mut lines = BufReader::new(stderr).lines();
    let port = tokio::time::timeout(Duration::from_secs(10), wait_for_proxy_port(&mut lines))
        .await
        .expect("proxy startup timeout");
    let stderr_drain = tokio::spawn(async move {
        let mut buf = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    // Spawn the run. `MOAGAN_MINIMAX_ENDPOINT` ends in
    // `/messages` (mandatory for `wire_format_from_url`); the
    // proxy's `join_upstream` will append the path that the
    // client actually requested (`/anthropic/v1/messages` →
    // `<base>/anthropic/v1` + `/anthropic/v1/messages`).
    let run_output = Command::new(binary())
        .args([
            "run",
            "--mode",
            "fast",
            "--provider",
            "minimax:MiniMax-M2.7",
            "--prompt",
            "Smoke test prompt for audit-proxy e2e path",
            "--runs-dir",
        ])
        .arg(&runs_dir)
        .current_dir(tmp.path())
        .env("MOAGAN_HOME", &runs_dir)
        .env("MOAGAN_CONFIG", &config_path)
        .env(
            "MOAGAN_MINIMAX_ENDPOINT",
            format!("http://127.0.0.1:{port}/anthropic/v1/messages"),
        )
        .env("MINIMAX_API_KEY", "test-key")
        // Disable the `max_tokens` and `temperature` startup
        // auto-probes. The override TOML sets
        // `max_tokens = 131072` directly, so the max-tokens
        // probe is redundant — the operator-side cap is the
        // source of truth. The temperature probe is the
        // matching env-var opt-out added in v0.12.15 (issue
        // #657 fix #3); pre-v0.12.15 this test pre-populated
        // `<MOAGAN_HOME>/temperatures_auto.toml` to skip it.
        // Both env vars map to the per-provider
        // `Some(0)` / `Some(false)` opt-out sentinels, which the
        // gates at `src/llm/provider.rs:1663` and `:1952-1962`
        // honour to skip the probes entirely.
        .env("MOAGAN_MAX_TOKEN_AUTO", "false")
        .env("MOAGAN_MAX_TOKEN_AUTO_SAVE", "false")
        .env("MOAGAN_TEMPERATURE_AUTO", "false")
        // Quiet tracing: WARN/ERROR only, so the run stdout is
        // a clean NDJSON stream. NDJSON emission is itself
        // conditional on `stdout` not being a TTY, which is
        // automatically true here (we read `.output()`).
        .env("RUST_LOG", "warn")
        .env_remove("MOAGAN_QUIET")
        // Strip any inherited `MOAGAN_LOG_FORMAT` so the
        // `run_start` NDJSON event assertion cannot be masked by a
        // parent-process override. A non-JSON value changes the
        // tracing layer shape and does break the assertions
        // (verified by mutation).
        //
        // `MOAGAN_EVENT_FORMAT` is NOT stripped: issue #657 fix #2
        // (v0.12.15) made `MOAGAN_EVENT_FORMAT=off` reach the
        // runtime resolver end-to-end, so any inherited value
        // here is intentional. See
        // `tests/integration_stream_routing.rs::env_event_format_off_suppresses_stdout_events`
        // for the canonical proof.
        .env_remove("MOAGAN_LOG_FORMAT")
        .output()
        .await
        .expect("spawn moagan run");
    let run_stdout = String::from_utf8_lossy(&run_output.stdout);
    let run_stderr = String::from_utf8_lossy(&run_output.stderr);

    // Pin bug #1 secondary invariant: even if the run succeeded,
    // the proxy must have emitted `run_start` on stdout. The
    // shell scripts broke here because they grepped the merged
    // stream for `run id:`, which only prints on a TTY
    // (`src/cli/mod.rs:1580-1582`). The NDJSON `kind=run_start`
    // event is the TTY-independent replacement.
    assert!(
        run_stdout.contains("\"kind\":\"run_start\""),
        "stdout must contain the NDJSON run_start event; \
         stdout:\n{run_stdout}\nstderr:\n{run_stderr}"
    );

    // Pin bug #4: every request observed by the mock must carry
    // `max_tokens <= 131072`. The mock returns 400 above that
    // threshold, so a regression that drops the clamp surfaces
    // as both a 400 in the audit log AND a value above 131072
    // here. We assert on the value, not on the 400, because the
    // value is the direct evidence of the clamp's correctness.
    //
    // What the test actually pins: `phase.rs:1217-1224` rewrites
    // `hash_input.max_tokens` with `effective_max_tokens`
    // (`src/llm/minimax.rs:424-447`) BEFORE calling
    // `MinimaxProvider::send`, so the clamp that fixes the wire
    // body is the `effective_max_tokens` chain. The test
    // therefore pins that no request exceeds the operator cap.
    // It does NOT distinguish that chain from the redundant
    // inner clamp at `src/llm/minimax.rs:398-422` inside
    // `MinimaxProvider::send`: deleting either chain in
    // isolation leaves the wire body still clamped via the
    // other, and the test stays green. Only deleting both
    // chains surfaces here. Distinguishing the two belongs to a
    // more targeted unit test that pins the inner clamp
    // directly; that test is out of scope for this PR.
    let max_seen = max_tokens_seen_by_mock(&server).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests.is_empty(),
        "mock must have observed at least one request; \
         run stdout:\n{run_stdout}\nstderr:\n{run_stderr}"
    );
    assert!(
        max_seen <= 131_072,
        "no request may carry max_tokens > 131072 \
         (observed {max_seen} across {} request(s))",
        requests.len()
    );

    // The pipeline is allowed to fail for reasons unrelated to
    // the bugs under test (judge schema drift, additional
    // required phases, etc.). The relaxed contract the prompt
    // permits is: the run_start event fired AND the max_tokens
    // invariant held. We try exit-0 first and only fall back to
    // the relaxed contract if the run fails for an unrelated
    // reason. The message below documents the fallback so a
    // future maintainer knows what to look for.
    if !run_output.status.success() {
        eprintln!(
            "moagan run did not exit 0 (status={:?}); \
             asserting only the relaxed contract (run_start + \
             max_tokens). Full stderr:\n{}",
            run_output.status.code(),
            run_stderr
        );
    }

    // Pin bug #1 indirectly: the proxy wrote its audit log under
    // the run directory. This proves the proxy was actually
    // accepting requests and writing to disk — the assertion is
    // intentionally not about line count (that drifts with
    // pipeline changes) but about the file's existence and
    // gzip magic, mirroring `tests/integration_audit_e2e.rs`.
    let run_dir = latest_run_dir(&runs_dir).expect("run dir created");
    let audit_path = run_dir.join("telemetry").join("external_audit.jsonl.gz");
    assert!(
        audit_path.exists(),
        "proxy audit log not written at {}",
        audit_path.display()
    );
    assert_eq!(
        &std::fs::read(&audit_path).expect("read audit log")[..2],
        &[0x1f, 0x8b],
        "audit log must be gzip"
    );

    // Tear the proxy down cleanly with SIGTERM and assert exit 0.
    // We send the signal directly so the proxy's signal handler
    // can flush its audit log; `kill_on_drop(true)` is the
    // panic-path fallback.
    let pid = proxy.id().expect("proxy pid");
    let signal_status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await
        .expect("send SIGTERM");
    assert!(signal_status.success(), "kill -TERM failed");
    let proxy_status = tokio::time::timeout(Duration::from_secs(10), proxy.wait())
        .await
        .expect("proxy did not exit after SIGTERM")
        .expect("wait for proxy");
    let proxy_stderr = stderr_drain.await.expect("join stderr drain");
    assert!(
        proxy_status.success(),
        "proxy exited with status {:?}; stderr:\n{proxy_stderr}",
        proxy_status.code()
    );
}

// ---------------------------------------------------------------------------
// Test #4 — Bug #1: the proxy banner is locatable by pattern, not by position
// ---------------------------------------------------------------------------

/// The proxy banner `proxy listening on http://127.0.0.1:PORT ->
/// UPSTREAM` goes to **stderr** (`src/cli/audit.rs:138-144`). With
/// v0.12.0 stream routing (`src/main.rs:245-269`), DEBUG/INFO
/// tracing events go to **stdout**, while ERROR-level events still
/// land on **stderr**. The original shell wrapper extracted the
/// port with `head -1` from the merged `2>&1` stream — a brittle
/// contract because:
///   - If stderr is read alone, `head -1` is the first ERROR-level
///     tracing event (or the banner, depending on timing).
///   - If `2>&1` is used, the first merged line is whichever
///     stream wrote first — the broken position-dependent
///     extraction that hit `scripts/e2e_audit_proxy.sh`.
///
/// The fix the shell landed is `grep -m1 'proxy listening'`, which
/// is pattern-based and position-independent. This test pins the
/// Rust invariant the fix relies on: a banner line is present in
/// stderr (not stdout), contains the listen URL, and parses to a
/// port — regardless of which line number it occupies.
///
/// We enable `RUST_LOG=debug` to force debug-level tracing
/// activity, which is the exact scenario that broke the original
/// script. The assertion deliberately reads stderr and only
/// stderr; the broken script relied on ordering across streams,
/// the fixed pattern-based extraction does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_banner_is_locatable_by_pattern_not_by_position() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runs_dir = tmp.path().join("runs");
    std::fs::create_dir_all(&runs_dir).expect("mkdir runs");

    let mut proxy = Command::new(binary())
        .args([
            "audit",
            "proxy",
            "--port",
            "0",
            // Real upstream does not matter — the proxy will
            // never forward during this test. The URL just
            // needs to be syntactically valid for `Url::parse`.
            "--upstream",
            "https://example.invalid/anthropic/v1",
            "--runs-dir",
        ])
        .arg(&runs_dir)
        .current_dir(tmp.path())
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .env("MOAGAN_HOME", &runs_dir)
        .env("MOAGAN_CONFIG", tmp.path().join("missing-config.toml"))
        // Force the exact v0.12.0 routing that broke the
        // script: DEBUG/INFO events go to stdout, the proxy
        // banner `eprintln!` stays on stderr. The combination is
        // what produced the original `head -1` failure.
        .env("RUST_LOG", "debug")
        // Strip any inherited log-format override so the
        // `"level":"DEBUG"` / `"level":"INFO"` line scan below
        // matches the JSON shape emitted by the default
        // tracing-subscriber JSON layer. An inherited
        // `MOAGAN_LOG_FORMAT=text` (or `MOAGAN_LOG_FORMAT=Text`)
        // would switch the layer to the text formatter and the
        // stdout line scan would silently never match.
        .env_remove("MOAGAN_LOG_FORMAT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn audit proxy");

    let stdout = proxy.stdout.take().expect("proxy stdout pipe");
    let stderr = proxy.stderr.take().expect("proxy stderr pipe");

    // Stdout drain: just collect every line for the cross-stream
    // assertion below. We don't search for the banner here — the
    // banner lives on stderr (and the cross-stream assertion
    // confirms it does NOT live on stdout).
    let stdout_drain = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });
    // Stderr drain: same collection, but we also signal as soon
    // as the banner appears so the main task can SIGTERM the
    // proxy and stop the test. Using a `oneshot` here keeps the
    // drain logic single-purpose: it does not have to know about
    // timeouts or process lifecycle.
    let (banner_tx, banner_rx) = tokio::sync::oneshot::channel::<()>();
    let stderr_drain = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr).lines();
        let mut banner_tx = Some(banner_tx);
        while let Ok(Some(line)) = lines.next_line().await {
            // Send the banner signal exactly once: take the
            // sender out of the Option when we see the banner so
            // subsequent iterations no-op the `if let`.
            if banner_tx.is_some() && line.contains("proxy listening on http://") {
                // Ignore send errors: the receiver may have
                // already given up after the timeout, in which
                // case we just keep draining.
                let _ = banner_tx.take().unwrap().send(());
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    // Wait for the banner on stderr. The pattern is
    // `proxy listening on http://127.0.0.1:` — same shape the
    // shell script's `grep -m1 'proxy listening'` matched.
    let banner_seen = tokio::time::timeout(Duration::from_secs(10), banner_rx)
        .await
        .map(|rx| rx.is_ok())
        .unwrap_or(false);
    assert!(
        banner_seen,
        "proxy did not emit its 'proxy listening on http://' \
         banner within 10s; the bug the test pins is exactly \
         that this banner can be hidden by tracing output"
    );

    // Tear the proxy down cleanly with SIGTERM. The drainers
    // finish once the pipes close (which they do when the
    // proxy exits).
    let pid = proxy.id().expect("proxy pid");
    let signal_status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await
        .expect("send SIGTERM");
    assert!(signal_status.success(), "kill -TERM failed");
    let proxy_status = tokio::time::timeout(Duration::from_secs(10), proxy.wait())
        .await
        .expect("proxy did not exit after SIGTERM")
        .expect("wait for proxy");
    let stderr_buf = stderr_drain.await.expect("join stderr drain");
    let stdout_text = stdout_drain.await.expect("join stdout drain");
    assert!(
        proxy_status.success(),
        "proxy exited with status {:?}; stderr:\n{stderr_buf}",
        proxy_status.code()
    );

    // Bug #1 invariant: a line in stderr contains the banner
    // pattern, and that line parses to a valid u16 port. We do
    // NOT require the line to be line 1 — the position
    // independence is the whole point.
    let banner_line = stderr_buf
        .lines()
        .find(|line| line.contains("proxy listening on http://127.0.0.1:"))
        .unwrap_or_else(|| {
            panic!(
                "stderr must contain the proxy banner 'proxy listening on \
                 http://127.0.0.1:' by pattern; stdout:\n{stdout_text}\
                 \nstderr:\n{stderr_buf}"
            )
        });
    let parsed_port = parse_proxy_port(banner_line).unwrap_or_else(|| {
        panic!(
            "banner line must parse to a u16 port: {banner_line:?}; \
             full stderr:\n{stderr_buf}"
        )
    });
    assert!(
        parsed_port > 0,
        "ephemeral proxy port must be non-zero (got {parsed_port}); \
         banner: {banner_line:?}"
    );

    // Sanity-check the asymmetry the prompt calls out: with
    // `RUST_LOG=debug`, DEBUG/INFO tracing lands on stdout
    // (v0.12.0 routing). Two halves of the contract together make
    // the inversion test load-bearing:
    //
    //   1. stdout must NOT contain the proxy banner (the
    //      banner-`eprintln!` lives on stderr);
    //   2. stdout must contain JSON tracing events at DEBUG or
    //      INFO level — under v0.12.0 routing those are exactly
    //      the events that go to stdout, so their presence is the
    //      direct evidence the routing layer is doing the
    //      redirection in the right direction. Without (2) the
    //      test passes vacuously under legacy routing
    //      (`MOAGAN_LOG_TO_STDERR=true`), because all tracing
    //      then goes to stderr and stdout is empty: the assertion
    //      `!stdout.contains(banner)` is trivially true on an
    //      empty stream. Asserting on a non-empty stdout with
    //      DEBUG/INFO events pins the v0.12.0 routing itself.
    assert!(
        !stdout_text.contains("proxy listening on http://"),
        "proxy banner must NOT appear on stdout under v0.12.0 \
         routing; stdout:\n{stdout_text}\nstderr:\n{stderr_buf}"
    );
    // `tracing-subscriber`'s default JSON layer emits each event
    // as one JSON object per line. The level appears as the
    // `"level"` field at the top level of the object (`DEBUG`,
    // `INFO`, `WARN`, `ERROR`). We scan for either DEBUG or INFO
    // to avoid coupling to the exact level the binary happens to
    // emit at startup; both are routed to stdout by the v0.12.0
    // writer (see `src/main.rs:255-267`).
    let stdout_has_tracing_event = stdout_text
        .lines()
        .any(|line| line.contains("\"level\":\"DEBUG\"") || line.contains("\"level\":\"INFO\""));
    assert!(
        stdout_has_tracing_event,
        "stdout must contain at least one DEBUG or INFO \
         tracing event under RUST_LOG=debug + v0.12.0 routing; \
         if this fails after a routing-layer change, check that \
         the fix did not roll the stream routing back to the \
         pre-v0.12.0 behaviour (the bug the test pins is exactly \
         that `head -1` could match a tracing line instead of \
         the banner). stdout:\n{stdout_text}\nstderr:\n{stderr_buf}"
    );
}
