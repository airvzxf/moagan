//! Integration test: verify v008 writers + v009 stability columns
//! are all populated after one end-to-end mock run.
//!
//! Covers the wiring delivered by fix(b) (579830e) for the writers
//! (outbox_events, provider_rollups, manifest_events, redact_audit)
//! and the v009_stability migration for the runs.stability_score /
//! runs.stability_label / runs.stability_sigma columns. Catches
//! regressions where any writer silently stops emitting rows or the
//! rank phase stops mirroring the stability verdict into SQLite.
//!
//! Single `moagan run --mode fast --provider mock` invocation — the
//! mock fixture exercises intake + clarify + route + propose x3 +
//! critique x6 + judge x9 + deliver = 22 calls; outbox_events should
//! see one `call.completed` per real (non-cache-hit) call.

use std::process::Command;

#[test]
fn mock_run_writes_all_v008_tables_and_v009_columns() {
    let bin = std::env::var("CARGO_BIN_EXE_moagan")
        .expect("CARGO_BIN_EXE_moagan — run via `cargo test`, not directly");

    // Fresh tmpdir roots the cross-run LLM cache (`<root>/cache/llm`)
    // and the `.runs/` dir, so every LLM call this run makes is a
    // guaranteed cache miss.
    let tmp = tempfile::TempDir::new().expect("tmpdir");

    let out = Command::new(&bin)
        .arg("run")
        .arg("--mode")
        .arg("fast")
        .arg("--provider")
        .arg("mock")
        .arg("--mock-dir")
        .arg("tests/fixtures/mock_provider")
        .arg("--prompt")
        .arg("v008/v009 writer smoke probe")
        .arg("--non-interactive")
        .arg("--runs-dir")
        .arg(tmp.path())
        .env_remove("MINIMAX_API_KEY")
        .output()
        .expect("moagan run");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mock run failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    // ---- Locate the run id -------------------------------------------------
    // src/cli/mod.rs:524 prints `run id: <full-uuid>` on success.
    // Strip the prefix and the trailing newline; the uuid formatter
    // uses hyphenated canonical form (8-4-4-4-12 hex chars).
    let run_id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("run id: "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("run id not found in stdout:\nstdout={stdout}\nstderr={stderr}"))
        .to_string();
    assert_eq!(run_id.len(), 36, "expected hyphenated uuid, got {run_id}");

    // ---- Locate the meta DB ------------------------------------------------
    // `--runs-dir` makes `MoaganHome::at(runs_dir)` the root; the
    // meta DB lives at `<root>/meta.sqlite` per fs_layout.rs:62.
    let db_path = tmp.path().join("meta.sqlite");
    assert!(db_path.exists(), "no meta.sqlite at {}", db_path.display());

    let db = rusqlite::Connection::open(&db_path).expect("open meta.sqlite");

    // ---- v008: outbox_events ----------------------------------------------
    // One `call.completed` row per non-cache-hit LLM call (the
    // telemetry module at src/telemetry/mod.rs:485 writes the row
    // before the sidecar lands). The fast-mode mock fixture is 22
    // calls (intake + clarify + route + 3 propose + 6 critique +
    // 9 judge + deliver) and none cache-hit on a fresh tmpdir, so
    // we expect >=22 rows. The test asserts >=1 to stay robust if
    // the fixture is trimmed later.
    let outbox_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM outbox_events \
             WHERE run_id = ?1 AND event_type = 'call.completed'",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query outbox_events");
    assert!(
        outbox_count >= 1,
        "expected >=1 outbox_events row of event_type='call.completed' for run {run_id}, got {outbox_count}"
    );

    // ---- v008: provider_rollups -------------------------------------------
    // Cross-run rollup keyed by (provider, model). The fast-mode
    // mock fires real (non-cache-hit) calls against the mock
    // provider, so `calls` must be >=1.
    let rollup_calls: i64 = db
        .query_row(
            "SELECT calls FROM provider_rollups WHERE provider = 'mock'",
            [],
            |r| r.get(0),
        )
        .expect("query provider_rollups");
    assert!(
        rollup_calls >= 1,
        "expected >=1 mock provider call in provider_rollups, got {rollup_calls}"
    );

    // ---- v008: manifest_events --------------------------------------------
    // src/cli/run.rs:399 emits one `run.completed` row after the
    // manifest sidecar is written. This is the canonical anchor
    // for the dashboard's "run.completed" timeline.
    let manifest_event_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM manifest_events \
             WHERE run_id = ?1 AND event_type = 'run.completed'",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query manifest_events");
    assert!(
        manifest_event_count >= 1,
        "expected >=1 manifest_events row of event_type='run.completed' for run {run_id}, got {manifest_event_count}"
    );

    // ---- v008: redact_audit ------------------------------------------------
    // The prompt is benign ("v008/v009 writer smoke probe"), so no
    // redact_audit rows are expected. We only assert the table
    // exists and is queryable — i.e. the v008 migration applied
    // and the redact writer plumbing is reachable.
    let redact_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM redact_audit WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query redact_audit");
    assert_eq!(
        redact_count, 0,
        "benign prompt should produce 0 redact_audit rows, got {redact_count}"
    );

    // ---- v009: stability columns ------------------------------------------
    // src/phases/rank.rs:255 mirrors the perturbation verdict into
    // the runs row via `db.record_run_stability`. With the default
    // StabilityConfig (enabled=true, n_perturbations=8) and 3
    // identical proposals in fast mode, the winner is stable.
    let stability_score: Option<f64> = db
        .query_row(
            "SELECT stability_score FROM runs WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query stability_score");
    assert!(
        stability_score.is_some(),
        "expected runs.stability_score to be populated for run {run_id}"
    );

    let stability_label: Option<String> = db
        .query_row(
            "SELECT stability_label FROM runs WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query stability_label");
    assert!(
        stability_label.is_some(),
        "expected runs.stability_label to be populated for run {run_id}"
    );

    // ---- v009: stability_sigma (added in the same migration) ---------------
    let stability_sigma: Option<f64> = db
        .query_row(
            "SELECT stability_sigma FROM runs WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query stability_sigma");
    assert!(
        stability_sigma.is_some(),
        "expected runs.stability_sigma to be populated for run {run_id}"
    );

    // ---- Sanity: legacy columns still present -----------------------------
    // shared_brief_hash was added by v007_lineage_context.sql and is
    // populated only when --context is used. None here is fine; we
    // just want to confirm the column is reachable and the run row
    // landed.
    let shared_brief_hash: Option<String> = db
        .query_row(
            "SELECT shared_brief_hash FROM runs WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query shared_brief_hash");
    let _ = shared_brief_hash;

    let status: String = db
        .query_row(
            "SELECT status FROM runs WHERE run_id = ?1",
            rusqlite::params![run_id],
            |r| r.get(0),
        )
        .expect("query runs.status");
    assert_eq!(status, "completed", "expected runs.status='completed'");
}

/// D.14.7 (`v0.5 roadmap PR-01`): wire up `moagan run --prompt -`
/// so the literal "-" is replaced with the contents of stdin before
/// the pipeline runs. The mock provider returns canned responses, so
/// the only on-disk evidence of the substitution is
/// `manifest.json::cli_prompt`, which `src/cli/run.rs:205` writes
/// straight from `opts.prompt` (and `src/cli/mod.rs:856-858` is now
/// the boot point that resolves the `-` sentinel — see the W1
/// comment in the dispatcher).
///
/// Test flow:
///   1. Spawn `moagan run --prompt - --provider mock --mock-dir ...`
///      with `Stdio::piped()` for stdin.
///   2. Write "hello world" into the child's stdin and close it,
///      so `read_prompt_from_stdin` reads the literal string.
///   3. Read the canonical `manifest.json` and verify
///      `cli_prompt == "hello world"`.
///   4. Cross-check the smoke-gate artifacts (`final/portfolio.md`
///      and `rankings/ranking.json`) so the run actually completed
///      end-to-end.
#[test]
fn mock_run_prompt_dash_reads_from_stdin() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let bin = std::env::var("CARGO_BIN_EXE_moagan")
        .expect("CARGO_BIN_EXE_moagan — run via `cargo test`, not directly");

    let tmp = tempfile::TempDir::new().expect("tmpdir");

    let mut child = Command::new(&bin)
        .arg("run")
        .arg("--mode")
        .arg("fast")
        .arg("--provider")
        .arg("mock")
        .arg("--mock-dir")
        .arg("tests/fixtures/mock_provider")
        .arg("--prompt")
        .arg("-")
        .arg("--non-interactive")
        .arg("--runs-dir")
        .arg(tmp.path())
        .env_remove("MINIMAX_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn moagan run --prompt -");

    // Feed the prompt into stdin and close the pipe so the child's
    // `read_to_string` returns EOF and the prompt is fully captured.
    {
        let mut stdin = child.stdin.take().expect("child stdin pipe");
        stdin
            .write_all(b"hello world")
            .expect("write prompt to stdin");
        // Drop stdin here, closing the pipe.
    }

    let out = child.wait_with_output().expect("wait moagan run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mock run failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    // ---- Locate the run id -------------------------------------------------
    // src/cli/mod.rs:875 prints `run id: <full-uuid>` on success.
    let run_id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("run id: "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("run id not found in stdout:\nstdout={stdout}\nstderr={stderr}"))
        .to_string();
    assert_eq!(run_id.len(), 36, "expected hyphenated uuid, got {run_id}");

    // ---- Verify cli_prompt was resolved from stdin -------------------------
    // The mock provider returns canned responses regardless of the
    // prompt, so the only on-disk evidence of the `--prompt -`
    // wire-up is the verbatim `cli_prompt` field on `manifest.json`.
    // It is set from `opts.prompt` in `src/cli/run.rs:205` BEFORE
    // the redactor in `src/cli/run.rs:392-399` runs, so the literal
    // stdin bytes (including the trailing newline dropped by
    // `read_to_string` on EOF — none here since we write exactly
    // "hello world" with no trailing \n) survive end-to-end.
    let manifest_path = tmp.path().join(".runs").join(&run_id).join("manifest.json");
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read manifest.json at {}: {e}", manifest_path.display()));
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw)
        .unwrap_or_else(|e| panic!("parse manifest.json: {e}\nbody={manifest_raw}"));
    let cli_prompt = manifest
        .get("cli_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("manifest.cli_prompt missing or null:\n{manifest_raw}"));
    assert_eq!(
        cli_prompt, "hello world",
        "manifest.cli_prompt must equal the stdin content; got {cli_prompt:?}"
    );

    // ---- Smoke-gate artifacts ----------------------------------------------
    // The fixture drives the full pipeline (intake + clarify + route
    // + propose x3 + critique x6 + judge x9 + deliver = 22 calls),
    // so on a successful mock run the canonical sidecars must exist.
    let run_root = tmp.path().join(".runs").join(&run_id);
    assert!(
        run_root.join("final").join("portfolio.md").exists(),
        "final/portfolio.md missing — run did not complete end-to-end"
    );
    assert!(
        run_root.join("rankings").join("ranking.json").exists(),
        "rankings/ranking.json missing — run did not complete end-to-end"
    );
}
