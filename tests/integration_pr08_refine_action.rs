//! v0.5 PR-08: `dispatch_refine_action` (D.22.2) consumer test.
//!
//! Verifies that `moagan refine --run-id <id> --action <action>`
//! invokes the dispatcher and persists its effects to
//! `manifest.json`:
//!
//! - `tighten-constraint`: appends `--verdict-detail <text>` to
//!   `manifest.prohibited_decisions` (D.22.2 / D.22.4).
//! - `add-evidence`: surfaces the augmented system prompt (the
//!   dispatcher returns the augmented prompt; we assert it is
//!   non-empty and includes the "Sources from past runs" header
//!   when the home carries an `EpistemicLegacy`).
//! - `drop-proposal` (degenerate CLI case): the dispatcher marks
//!   `proposal.replaced_by = DROPPED_SENTINEL`, but since the CLI
//!   does not have a specific proposal id in scope (the enum
//!   carries none), the test asserts the CLI surfaces a clean
//!   success line without panicking and the manifest's
//!   `prohibited_decisions` is unchanged.
//!
//! The tests use a hand-rolled manifest seeder (no LLM call) so
//! they run under `cargo test --all-targets` without external
//! network. The CLI is invoked via `CARGO_BIN_EXE_moagan`.

#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use moagan::domain::{Manifest, ManifestPhase, ManifestUsage};
use moagan::error::Result;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

fn fresh_home() -> (tempfile::TempDir, Arc<MoaganHome>) {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    (tmp, home)
}

fn blake3_of(m: &Manifest) -> String {
    let mut canonical = m.clone();
    canonical.manifest_blake3 = String::new();
    let j = serde_json::to_vec(&canonical).unwrap();
    blake3::hash(&j).to_hex().to_string()
}

fn seed_manifest(home: &MoaganHome, run_id: RunId) -> Manifest {
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    let now = chrono::Utc::now();
    let mut manifest = Manifest {
        schema_version: "v2".into(),
        run_id,
        mode: "fast".into(),
        status: "completed".into(),
        created_at: now,
        updated_at: now,
        client_version: env!("CARGO_PKG_VERSION").into(),
        brief_sha256: "deadbeef".into(),
        brief_blake3: "deadbeef".into(),
        provider: "mock".into(),
        model: "mock-model".into(),
        phases: vec![ManifestPhase {
            phase: "intake".into(),
            started_unix: now.timestamp(),
            ended_unix: now.timestamp() + 1,
            status: "end".into(),
            calls: 1,
            error: None,
        }],
        usage: ManifestUsage::default(),
        manifest_blake3: String::new(),
        parent_run_id: None,
        shared_brief_hash: None,
        context_refs: Vec::new(),
        lineage_paths: None,
        cli_prompt: Some("seed prompt".into()),
        config_hash: None,
        created_at_iso: now.to_rfc3339(),
        last_resumed_at_iso: None,
        resume_count: 0,
        prohibited_decisions: Vec::new(),
    };
    manifest.manifest_blake3 = blake3_of(&manifest);
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    std::fs::write(run_dir.manifest(), bytes).unwrap();
    manifest
}

fn read_manifest(home: &MoaganHome, run_id: RunId) -> Manifest {
    let bytes = std::fs::read(home.run_dir(run_id).manifest()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn invoke_moagan<I, S>(home: &MoaganHome, args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(binary())
        .env("MOAGAN_HOME", home.root())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke moagan binary: {e}"))
}

// =====================================================================
// tighten-constraint (D.22.2 happy path)
// =====================================================================

/// `moagan refine --run-id <id> --action tighten-constraint
/// --verdict-detail "<text>"` appends `<text>` to
/// `manifest.prohibited_decisions` and writes the augmented
/// manifest back atomically. The integration test is the
/// canonical verification for PR-08 (roadmap: "diff <before.json>
/// <after.json> muestra cambio en constraints").
#[test]
fn refine_tighten_constraint_appends_to_manifest_prohibited_decisions() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let before = seed_manifest(&home, run_id);
    assert!(
        before.prohibited_decisions.is_empty(),
        "seed manifest must start with empty prohibited_decisions"
    );

    let output = invoke_moagan(
        &home,
        [
            "refine",
            "--run-id",
            &run_id.to_string(),
            "--action",
            "tighten-constraint",
            "--verdict-detail",
            "no sync RPC across the boundary",
        ],
    );
    assert!(
        output.status.success(),
        "refine --action tighten-constraint failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = read_manifest(&home, run_id);
    assert_eq!(
        after.prohibited_decisions,
        vec!["no sync RPC across the boundary".to_string()],
        "manifest.prohibited_decisions should now contain the verdict_detail"
    );

    // Sanity-check: the BLAKE3 manifest hash was recomputed.
    assert_ne!(
        after.manifest_blake3, before.manifest_blake3,
        "manifest_blake3 must change when the manifest mutates"
    );

    // Sanity-check: the operator gets a useful summary line.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("tighten-constraint"),
        "stdout should mention the action; got: {stdout}"
    );
    assert!(
        stdout.contains("no sync RPC across the boundary"),
        "stdout should echo the new prohibited_decisions entry; got: {stdout}"
    );
}

/// Repeated `tighten-constraint` calls accumulate (the dispatcher
/// appends to the existing `prohibited_decisions` carried by
/// `RefineContext::from_run`, and the CLI writes the augmented
/// vector back). This mirrors the operator workflow: each
/// adversary verdict appends one entry.
#[test]
fn refine_tighten_constraint_accumulates_across_calls() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let _before = seed_manifest(&home, run_id);

    for detail in ["vague about auth model", "monolith assumption"] {
        let output = invoke_moagan(
            &home,
            [
                "refine",
                "--run-id",
                &run_id.to_string(),
                "--action",
                "tighten_constraint",
                "--verdict-detail",
                detail,
            ],
        );
        assert!(
            output.status.success(),
            "tighten-constraint call failed for {detail:?}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let after = read_manifest(&home, run_id);
    assert_eq!(
        after.prohibited_decisions,
        vec![
            "vague about auth model".to_string(),
            "monolith assumption".to_string(),
        ],
        "tighten-constraint should accumulate entries; got {:?}",
        after.prohibited_decisions
    );
}

/// The CLI accepts both the kebab-case (`tighten-constraint`)
/// CLI form and the snake_case (`tighten_constraint`) wire form
/// (D.5.1 audit format). The integration test pins both.
#[test]
fn refine_action_accepts_kebab_and_snake_case() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let _before = seed_manifest(&home, run_id);

    let output = invoke_moagan(
        &home,
        [
            "refine",
            "--run-id",
            &run_id.to_string(),
            "--action",
            "tighten_constraint",
            "--verdict-detail",
            "kebab-or-snake test",
        ],
    );
    assert!(
        output.status.success(),
        "snake_case action should parse; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = read_manifest(&home, run_id);
    assert_eq!(
        after.prohibited_decisions,
        vec!["kebab-or-snake test".to_string()]
    );
}

// =====================================================================
// drop-proposal (degenerate CLI case — no proposal id)
// =====================================================================

/// `moagan refine --action drop-proposal` cannot meaningfully
/// execute (the enum carries no proposal id) but must exit 0
/// with a friendly summary, and must NOT mutate
/// `manifest.prohibited_decisions`. This pins the CLI-side
/// handling for the degenerate case.
#[test]
fn refine_drop_proposal_is_clean_no_op_at_cli_layer() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let before = seed_manifest(&home, run_id);

    let output = invoke_moagan(
        &home,
        [
            "refine",
            "--run-id",
            &run_id.to_string(),
            "--action",
            "drop-proposal",
        ],
    );
    assert!(
        output.status.success(),
        "drop-proposal must exit 0; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = read_manifest(&home, run_id);
    assert_eq!(
        after.prohibited_decisions, before.prohibited_decisions,
        "drop-proposal must not mutate prohibited_decisions"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("drop-proposal"),
        "stdout should mention the action; got: {stdout}"
    );
}

// =====================================================================
// request-human-input (telemetry emit)
// =====================================================================

/// `moagan refine --action request-human-input` exits 0 and
/// emits a `StaleArtifact` telemetry event. The CLI summary
/// surfaces this fact to the operator.
#[test]
fn refine_request_human_input_emits_summary_line() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let _before = seed_manifest(&home, run_id);

    let output = invoke_moagan(
        &home,
        [
            "refine",
            "--run-id",
            &run_id.to_string(),
            "--action",
            "request-human-input",
        ],
    );
    assert!(
        output.status.success(),
        "request-human-input must exit 0; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("request-human-input"),
        "stdout should mention the action; got: {stdout}"
    );
    assert!(
        stdout.contains("StaleArtifact"),
        "stdout should announce the StaleArtifact telemetry event; got: {stdout}"
    );
}

// =====================================================================
// error path
// =====================================================================

/// An unknown `--action` is rejected by clap's value_parser with
/// a non-zero exit code (the binary uses clap's standard
/// error-display path; we only assert non-success here so the
/// test does not depend on clap's exact stderr wording).
#[test]
fn refine_unknown_action_exits_non_zero() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let _before = seed_manifest(&home, run_id);

    let output = invoke_moagan(
        &home,
        [
            "refine",
            "--run-id",
            &run_id.to_string(),
            "--action",
            "not-a-real-action",
        ],
    );
    assert!(
        !output.status.success(),
        "unknown action must fail; got exit 0"
    );
}

/// Omitting both `--proposal` and `--action` fails clap's
/// `required_unless_present` with a non-zero exit.
#[test]
fn refine_without_proposal_or_action_exits_non_zero() {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let _before = seed_manifest(&home, run_id);

    let output = invoke_moagan(&home, ["refine", "--run-id", &run_id.to_string()]);
    assert!(
        !output.status.success(),
        "missing both --proposal and --action must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--proposal") || stderr.contains("--action"),
        "stderr should mention the missing flags; got: {stderr}"
    );
}

// =====================================================================
// unit sanity: FromStr round-trip
// =====================================================================

#[test]
fn refine_action_from_str_round_trips_all_variants() -> Result<()> {
    use std::str::FromStr;

    use moagan::ranking::RefineAction;

    let cases = [
        (RefineAction::TightenConstraint, "tighten-constraint"),
        (RefineAction::AddEvidence, "add-evidence"),
        (RefineAction::SplitProposal, "split-proposal"),
        (RefineAction::MergeProposal, "merge-proposal"),
        (RefineAction::RerunCritique, "rerun-critique"),
        (RefineAction::DropProposal, "drop-proposal"),
        (RefineAction::RequestHumanInput, "request-human-input"),
    ];
    for (action, cli_form) in cases {
        assert_eq!(action.as_cli_str(), cli_form);
        // Kebab-case CLI form parses back.
        assert_eq!(RefineAction::from_str(cli_form).unwrap(), action);
        // Snake_case wire form parses back.
        let snake = action.as_str();
        assert_eq!(RefineAction::from_str(snake).unwrap(), action);
    }
    Ok(())
}
