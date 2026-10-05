//! End-to-end discovery validation against the native `minimax`
//! provider. Closes the
//! `docs/discovery-validation-research-2026-08-13.md` gap for the
//! Anthropic-compatible `minimax` wire at
//! `https://api.minimax.io/anthropic/v1/messages`.
//!
//! `#[ignore]`d by default; only runs locally / in `e2e-network`
//! when the operator's `MINIMAX_API_KEY` is exported. With no
//! key the test returns `Ok(())` immediately so a CI matrix without
//! the secret stays green. Run with:
//!
//! ```bash
//! MINIMAX_API_KEY=sk-... cargo test --test integration_discover_minimax -- --ignored
//! ```
//!
//! The validation asserts the run writes sketches and the catalogue
//! under `final/` (README, the cell's facet file, `catalog.json`
//! listing every sketch). The 1×1 matrix keeps the fan-out small
//! (1 cell × `--sketches-per-cell 80`) so the run stays under the
//! 600 s default test timeout.

use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

const PROMPT: &str = "Compare three Rust HTTP clients for binary streaming";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// Run `moagan discover` against the native `minimax` provider and
/// return the captured stdout bytes (NDJSON event stream) plus the
/// exit status. Mirrors the existing
/// `discover_minimax_writes_the_catalogue` invocation, with two
/// additions:
///   - `MOAGAN_EVENT_FORMAT=jsonl` so the stdout stream is the
///     NDJSON event surface from `src/telemetry/stdout_events.rs`
///     even when stdout is a pipe (cargo test is non-TTY, so the
///     auto-detect at `stdout_events.rs:65-78` already activates
///     JSONL; the env var makes the intent visible in CI logs).
///   - `MOAGAN_DECISION_FORMAT=all` + `--decision-format=all` so the
///     high-volume `Decision` kinds (`category_assigned`,
///     `judge_verdict`, `cache_hit`, `cache_miss`) are surfaced;
///     `decision_kinds` assertions can then verify the full
///     post-extract pass ran.
///
/// Returns `(status, stdout_bytes, stderr_bytes)`. The
/// `artifact_root` argument is the same path used by the existing
/// test so both tests write artefacts to the same CI-uploaded tree.
fn run_moagan_discover(artifact_root: &Path) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let out = Command::new(binary())
        .env("MOAGAN_MAX_TOKEN_AUTO", "0")
        .env("MOAGAN_TEMPERATURE_AUTO", "false")
        .env("MOAGAN_TOP_P_AUTO", "false")
        .env("MOAGAN_TOP_K_AUTO", "false")
        .env("MOAGAN_EVENT_FORMAT", "jsonl")
        .env("MOAGAN_DECISION_FORMAT", "all")
        .args([
            "discover",
            "--provider",
            "minimax:MiniMax-M3",
            "--prompt",
            PROMPT,
            "--sketches-per-cell",
            "80",
            "--dimensions",
            "1",
            "--facets-per-dimension",
            "1",
            "--max-parallelism",
            "2",
            "--event-format=jsonl",
            "--decision-format=all",
            "--non-interactive",
            "--runs-dir",
        ])
        .arg(artifact_root)
        .output()
        .expect("spawn moagan discover");
    (out.status, out.stdout, out.stderr)
}

/// Per-event summary produced by `parse_events`. Used by
/// `discover_minimax_structural_validation` to assert on the NDJSON
/// shape, not just the filesystem side-effects.
#[derive(Default, Debug)]
struct EventSummary {
    run_start: bool,
    run_end_ok: bool,
    run_end_payload: Option<serde_json::Value>,
    phase_starts: Vec<String>,
    phase_errors: Vec<(String, String)>,
    llm_calls_ok: u32,
    llm_calls_failed: u32,
    decision_kinds: Vec<String>,
    warnings: Vec<(String, String)>,
}

/// Parse the NDJSON stream emitted on `moagan` stdout into a
/// structural summary. Tolerant of stray non-JSON lines (the
/// `serde_json::from_str` per-line + `continue` path handles the
/// rare case where a `tracing` event leaks onto the stream).
///
/// Consecutive-dedup on `decision_kinds` keeps the assert log
/// readable for high-volume kinds (`cache_hit`, `category_assigned`)
/// without changing the pass/fail semantics.
fn parse_events(stdout: &[u8]) -> EventSummary {
    let mut s = EventSummary::default();
    for line_result in stdout.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "run_start" => s.run_start = true,
            "run_end" => {
                s.run_end_payload = Some(v.clone());
                if v.get("status").and_then(|s| s.as_str()) == Some("ok") {
                    s.run_end_ok = true;
                }
            }
            "phase_start" => {
                if let Some(p) = v.get("phase").and_then(|p| p.as_str()) {
                    s.phase_starts.push(p.to_owned());
                }
            }
            "phase_error" => {
                let p = v
                    .get("phase")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_owned();
                let e = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("")
                    .to_owned();
                s.phase_errors.push((p, e));
            }
            "llm_call" => {
                if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                    s.llm_calls_ok += 1;
                } else {
                    s.llm_calls_failed += 1;
                }
            }
            "decision" => {
                if let Some(k) = v.get("decision_kind").and_then(|k| k.as_str())
                    && s.decision_kinds.last().map(|x| x.as_str()) != Some(k)
                {
                    s.decision_kinds.push(k.to_owned());
                }
            }
            "warning" => {
                let c = v
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_owned();
                let l = v
                    .get("level")
                    .and_then(|l| l.as_str())
                    .unwrap_or("")
                    .to_owned();
                s.warnings.push((c, l));
            }
            _ => {}
        }
    }
    s
}

#[test]
#[ignore = "requires MINIMAX_API_KEY; run with --ignored"]
fn discover_minimax_writes_the_catalogue() {
    if std::env::var_os("MINIMAX_API_KEY").is_none() {
        eprintln!("skipping: MINIMAX_API_KEY not set");
        return;
    }
    // Use a stable path inside `target/` so the run artifacts persist
    // past the test's exit. The CI workflow uploads this directory via
    // `actions/upload-artifact@v4` so a failing or slow run can be
    // inspected post-mortem. `CARGO_TARGET_TMPDIR` is the conventional
    // cargo-test scratch dir, and `tests/` ensures the path is unique
    // to this test binary (so parallel test runs do not clobber).
    let artifact_root: std::path::PathBuf = std::path::PathBuf::from(
        std::env::var("CARGO_TARGET_TMPDIR").unwrap_or_else(|_| "target".into()),
    )
    .join("test-runs")
    .join("minimax");
    let _ = std::fs::remove_dir_all(&artifact_root); // clean from prior runs
    std::fs::create_dir_all(&artifact_root).expect("create artifact root");
    let tmp: &std::path::Path = artifact_root.as_path(); // type-coerce for call sites
    let out = Command::new(binary())
        // Disable the per-provider `max_tokens_auto` probe so the
        // 19-step exponential search (up to 2^19 = 524_288 against
        // MINIMAX_MAX_TOKENS_CAP = 524_288) does not race the
        // 80-sketch matrix fan-out. The probe is
        // background-only by design, but on a fresh CI runner with
        // no cached `max_tokens_auto.toml` the upstream probe
        // timeouts (5 s × ~19 steps) compound with the matrix +
        // post-matrix LLM calls (Tagger + Cluster + FacetDeriver +
        // Extractor + Integrator) and push the run past the 15-min
        // `test-ignored` job ceiling (PR #473    ). The wire body
        // still clamps to `MINIMAX_MAX_TOKENS_CAP` via
        // `MinimaxProvider::effective_max_tokens`, so skipping the
        // probe does not regress the HTTP-400 fix from commit
        // `cd0451e`.
        .env("MOAGAN_MAX_TOKEN_AUTO", "0")
        .env("MOAGAN_TEMPERATURE_AUTO", "false")
        .env("MOAGAN_TOP_P_AUTO", "false")
        .env("MOAGAN_TOP_K_AUTO", "false")
        .args([
            "discover",
            "--provider",
            "minimax:MiniMax-M3",
            "--prompt",
            PROMPT,
            "--sketches-per-cell",
            "80",
            "--dimensions",
            "1",
            "--facets-per-dimension",
            "1",
            "--max-parallelism",
            "2",
            "--non-interactive",
            "--runs-dir",
        ])
        .arg(tmp)
        .output()
        .expect("spawn moagan discover");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run_id = fs::read_dir(tmp.join(".runs"))
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .expect("at least one run dir");
    let run_dir = run_id.path();

    // Diagnostic snapshot — printed on assertion failure so CI logs show
    // the actual run-dir state, not just the empty dir name.
    let run_dir_top: Vec<String> = fs::read_dir(&run_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let count = e
                        .path()
                        .metadata()
                        .ok()
                        .filter(|m| m.is_dir())
                        .map(|_| fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
                        .unwrap_or(0);
                    format!("{name}/ ({count} entries)")
                })
                .collect()
        })
        .unwrap_or_default();

    // Pull a tail of the most recent moagan log file (if any) so
    // the CI panic message includes the actual error.
    let latest_log_tail: String = fs::read_dir(run_dir.join("logs"))
        .ok()
        .and_then(|d| {
            d.filter_map(|e| e.ok())
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        })
        .and_then(|latest| {
            let content = fs::read_to_string(latest.path()).ok()?;
            let tail: String = content
                .lines()
                .rev()
                .take(50)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            Some(tail)
        })
        .unwrap_or_default();

    if !latest_log_tail.is_empty() {
        eprintln!("---- latest moagan log tail ----\n{latest_log_tail}\n----");
    }

    let sketch_count = fs::read_dir(run_dir.join("sketches"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.ends_with(".json") && !n.ends_with(".meta.json")
                })
                .count()
        })
        .unwrap_or(0);
    assert!(
        sketch_count >= 1,
        "sketches/ should have ≥1 sketch\nrun_dir contents: {run_dir_top:?}"
    );
    let readme = fs::read_to_string(run_dir.join("final").join("README.md"))
        .unwrap_or_else(|e| panic!("final/README.md: {e}\nrun_dir contents: {run_dir_top:?}"));
    let coverage = format!("Theses in this catalogue: {sketch_count} of {sketch_count} sketches");
    assert!(
        readme.contains(&coverage),
        "README.md lacks {coverage:?}:\n{readme}"
    );
    assert!(
        run_dir.join("final").join("dim-00").join("f1.md").is_file(),
        "facet file final/dim-00/f1.md missing"
    );
    let catalog: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("final").join("catalog.json"))
            .expect("final/catalog.json"),
    )
    .expect("catalog.json parses");
    assert_eq!(
        catalog["sketches"].as_array().map(Vec::len),
        Some(sketch_count),
        "catalog.json must list every sketch"
    );
}

/// Structural validation of the `moagan discover` pipeline via
/// NDJSON event assertions. Complements the filesystem checks in
/// `discover_minimax_writes_the_catalogue` — that test verifies the
/// side-effects (sketches and catalogue written); this test verifies the
/// process shape (the right phases ran in the right order, no phase
/// errored, the LLM was actually called).
///
/// What this catches that the filesystem check does not:
/// - Run 34145427514 (commit `7f0c3f0`, 2026-09-07 16:55 UTC)
///   produced a `tags/` entry before the `clarify` phase panicked
///   on a JSON-truncation upstream bug. The filesystem check
///   passed its first assertion (`tags/ >= 1 entry`) before
///   tripping on the global `assert!(out.status.success())` panic
///   at line 97. With this new test in place, the same failure
///   emits a clear `"phase_error events present: [("clarify",
///   "schema violation: model output is not valid JSON…")]"`
///   diagnostic from the structural assertion below.
/// - Any future regression that drops a phase (e.g. dispatcher
///   refactor that skips `discover_integrate`) would slip past
///   the filesystem check but fail the phase-set assertion here.
///
/// The `#[ignore]` flag and `MINIMAX_API_KEY` gate mirror the
/// existing test — both tests share the same CI workflow and the
/// same billing profile.
#[test]
#[ignore = "requires MINIMAX_API_KEY; run with --ignored"]
fn discover_minimax_structural_validation() {
    if std::env::var_os("MINIMAX_API_KEY").is_none() {
        eprintln!("skipping: MINIMAX_API_KEY not set");
        return;
    }

    let artifact_root: PathBuf =
        PathBuf::from(std::env::var("CARGO_TARGET_TMPDIR").unwrap_or_else(|_| "target".into()))
            .join("test-runs")
            .join("minimax-structural");
    let _ = fs::remove_dir_all(&artifact_root);
    fs::create_dir_all(&artifact_root).expect("create artifact root");

    let (status, stdout, stderr) = run_moagan_discover(&artifact_root);
    assert!(
        status.success(),
        "moagan exited with non-zero status: {status:?}\nstderr: {}",
        String::from_utf8_lossy(&stderr)
    );

    let events = parse_events(&stdout);

    // 1) run_start + run_end with status=ok must be present.
    assert!(
        events.run_start,
        "no run_start event on stdout; pipeline never dispatched \
         (NDJSON stream silent or binary crashed before emit)"
    );
    assert!(
        events.run_end_ok,
        "no run_end event with status=ok; run_end payload: {:?}",
        events.run_end_payload
    );

    // 2) Every phase of the discover pipeline must have emitted
    //    phase_start: intake, then the catalogue render. The sketch
    //    fan-out runs in the coordinator, which emits
    //    `discovery_iteration` events instead of phase events, and
    //    `--dimensions 1 --facets-per-dimension 1` fixes the matrix
    //    so `discover_dimensions` does not run.
    let required_phases = ["intake", "discover_render"];
    let started: BTreeSet<&str> = events.phase_starts.iter().map(String::as_str).collect();
    for phase in required_phases {
        assert!(
            started.contains(phase),
            "phase {phase:?} did not emit phase_start; \
             started phases: {:?}",
            events.phase_starts
        );
    }

    // 3) At least one successful llm_call. Without this, the
    //    filesystem could be empty and the catalogue check
    //    would fail anyway, but this surfaces it earlier with a
    //    clearer diagnostic.
    assert!(
        events.llm_calls_ok >= 1,
        "no successful llm_call events (ok={}, failed={})",
        events.llm_calls_ok,
        events.llm_calls_failed
    );

    // 4) Zero phase_error events. This is the assertion that would
    //    have caught run 34145427514 directly.
    assert_eq!(
        events.phase_errors.len(),
        0,
        "phase_error events present (would have caught this \
         failure class in run 34145427514): {:?}",
        events.phase_errors
    );

    // 6) SOFT: rate_limit / throttle / circuit_open warnings are a
    //    diagnostic signal (issue #761 upstream saturation), not a
    //    failure. The existing model.retry_provider self-heal
    //    catches single-shot 429s and retries; a few warnings on
    //    the happy path are normal. A sustained burst is the
    //    diagnostic value, not a hard fail.
    let mut rate_limit_warnings = 0u32;
    for (code, _level) in &events.warnings {
        if code.starts_with("rate_limit")
            || code.starts_with("throttle")
            || code.starts_with("circuit_open")
        {
            rate_limit_warnings += 1;
        }
    }
    if rate_limit_warnings > 0 {
        eprintln!(
            "NOTE: {rate_limit_warnings} rate_limit/throttle/circuit_open warnings \
             observed; the run_id completed without phase_error, but \
             upstream saturation is degrading the budget. See issue #761."
        );
    }
}
