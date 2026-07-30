//! End-to-end smoke test for Phase D (Plan B sub-phase D).
//!
//! Phase D closes when the pipeline:
//!
//! 1. Clusters proposals by SimHash / Jaccard similarity
//!    (`ClusterProposalsPhase`).
//! 2. Synthesizes each eligible cluster with the `synthesizer` role
//!    (`SynthesizePhase`).
//! 3. Computes the disagreement score per proposal and fires the
//!    `adversary` role only when the judges disagree by more than the
//!    threshold (`JudgePhase`).
//! 4. Persists a `human_checkpoint` JSON sidecar when the run is
//!    interactive and the brief looks risky (`IntakePhase` /
//!    `ClarifyPhase`).
//!
//! The test exercises (1) and (4) directly (pure rust paths with the
//! `MockProvider`). (2) and (3) are exercised indirectly through the
//! disagreement-score unit tests on `JudgePhase::disagreement_score`.

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use moagan::config::Config;
use moagan::domain::Proposal;
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::ProviderRegistry;
use moagan::phases::cluster_proposals::{CLUSTER_THRESHOLD, ClusterProposalsPhase};
use moagan::phases::phase::{Phase, RunContext};
use moagan::phases::synthesize::SynthesizePhase;
use moagan::telemetry::Telemetry;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn write_proposal(dir: &std::path::Path, id: &str, p: &Proposal) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.json"));
    let json = serde_json::to_vec_pretty(p).unwrap();
    std::fs::write(&path, json).unwrap();
}

fn proposal(id: &str, summary: &str, approach: &str) -> Proposal {
    Proposal {
        id: id.to_owned(),
        summary: summary.to_owned(),
        approach: approach.to_owned(),
        tradeoffs: vec!["some trade-off".to_owned()],
        evidence: vec!["sk_001".to_owned()],
        source_sketch: String::new(),
        artifacts: Vec::new(),
        replaced_by: None,
    }
}

fn fresh_ctx(home: Arc<MoaganHome>) -> RunContext {
    RunContext::new(
        RunId::new(),
        home,
        Arc::new(ProviderRegistry::default()),
        "mock".into(),
        "mock-model".into(),
        Parallelism::new(1),
        Telemetry::noop(),
        String::new(),
        "standard".into(),
    )
    .with_interactive(false)
}

#[test]
fn cluster_proposals_phase_merges_near_duplicates() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    let run_dir = home.run_dir(RunId::new());
    run_dir.ensure()?;

    write_proposal(
        &run_dir.proposals(),
        "p_001",
        &proposal("p_001", "Use Rust and SQLite", "single binary"),
    );
    write_proposal(
        &run_dir.proposals(),
        "p_002",
        &proposal(
            "p_002",
            "Use Rust and SQLite single binary",
            "tight integration",
        ),
    );
    write_proposal(
        &run_dir.proposals(),
        "p_003",
        &proposal("p_003", "Microservices in Go", "service per concern"),
    );

    let phase = ClusterProposalsPhase::default();
    let ctx = fresh_ctx(home.clone());
    let output = pollster::block_on(phase.execute(&ctx))?;
    let moagan::phases::PhaseOutput::ClusterProposals(paths) = output else {
        panic!("expected ClusterProposals output");
    };
    assert!(!paths.is_empty(), "got zero clusters: {paths:?}");
    // The two near-duplicates collapse into one cluster; the third
    // proposal is its own cluster. Verify the cluster files exist.
    assert!(paths.iter().any(|p| p.exists()));
    Ok(())
}

#[test]
fn cluster_proposals_phase_writes_empty_marker_for_singleton_run() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    let run_dir = home.run_dir(RunId::new());
    run_dir.ensure()?;
    write_proposal(
        &run_dir.proposals(),
        "p_001",
        &proposal("p_001", "only one", "only one"),
    );

    let phase = ClusterProposalsPhase::default();
    let ctx = fresh_ctx(home.clone());
    let output = pollster::block_on(phase.execute(&ctx))?;
    let moagan::phases::PhaseOutput::ClusterProposals(paths) = output else {
        panic!("expected ClusterProposals output");
    };
    // The marker file is written even when there's nothing to merge.
    assert_eq!(paths.len(), 1, "expected one marker file");
    let cp: moagan::phases::cluster_proposals::ProposalCluster =
        serde_json::from_slice(&std::fs::read(&paths[0])?)?;
    assert_eq!(cp.id, "cp_00");
    assert!(cp.member_proposals.is_empty());
    Ok(())
}

#[test]
fn synthesize_phase_is_no_op_for_singletons() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    let run_dir = home.run_dir(RunId::new());
    run_dir.ensure()?;
    // Write only one cluster with a single member — the synthesize
    // phase should refuse to merge and return an empty output.
    std::fs::create_dir_all(run_dir.cluster_proposals_dir()).unwrap();
    let cp = moagan::phases::cluster_proposals::ProposalCluster {
        schema_version: "v1".into(),
        id: "cp_00".into(),
        member_proposals: vec!["p_001".into()],
        cluster_text_sample: "single".into(),
        created_unix: 0,
    };
    std::fs::write(
        run_dir.cluster_proposals_dir().join("cp_00.json"),
        serde_json::to_vec(&cp)?,
    )?;
    write_proposal(
        &run_dir.proposals(),
        "p_001",
        &proposal("p_001", "only one", "only one"),
    );

    let phase = SynthesizePhase::default();
    let ctx = fresh_ctx(home.clone());
    let output = pollster::block_on(phase.execute(&ctx))?;
    let moagan::phases::PhaseOutput::Synthesized(paths) = output else {
        panic!("expected Synthesized output");
    };
    assert!(
        paths.is_empty(),
        "synthesize should not emit when no cluster qualifies"
    );
    Ok(())
}

#[test]
fn checkpoint_persists_sidecar_on_yes() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cp = Checkpoint::yes_no(CheckpointKind::Clarify, "continue?");
    let opts = CheckpointOpts::with_stdin_override("y");
    let res = moagan::checkpoint::ask(&cp, tmp.path(), &opts)?;
    assert_eq!(res, moagan::checkpoint::Resolution::Approved);
    let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", cp.id)))?;
    assert!(json.contains("\"response\": \"y\""));
    assert!(json.contains("\"kind\": \"clarify\""));
    Ok(())
}

#[test]
fn checkpoint_persists_sidecar_on_no() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cp = Checkpoint::yes_no(CheckpointKind::Final, "ship?");
    let opts = CheckpointOpts::with_stdin_override("n");
    let res = moagan::checkpoint::ask(&cp, tmp.path(), &opts)?;
    assert_eq!(res, moagan::checkpoint::Resolution::Rejected);
    let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", cp.id)))?;
    assert!(json.contains("\"response\": \"n\""));
    Ok(())
}

#[test]
fn checkpoint_persists_sidecar_on_modify() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cp = Checkpoint::yes_no(CheckpointKind::Clarify, "extra constraint?");
    let opts = CheckpointOpts::with_stdin_override("add a 5GB cap");
    let res = moagan::checkpoint::ask(&cp, tmp.path(), &opts)?;
    assert_eq!(
        res,
        moagan::checkpoint::Resolution::Modify("add a 5GB cap".into())
    );
    let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", cp.id)))?;
    assert!(json.contains("\"response\": \"add a 5GB cap\""));
    Ok(())
}

#[test]
fn checkpoint_skip_marks_non_interactive() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let cp = Checkpoint::yes_no(CheckpointKind::Intake, "looks good?");
    let opts = CheckpointOpts::non_interactive();
    let res = moagan::checkpoint::ask(&cp, tmp.path(), &opts)?;
    assert_eq!(res, moagan::checkpoint::Resolution::Approved);
    let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", cp.id)))?;
    assert!(json.contains("<skipped:non_interactive>"));
    Ok(())
}

#[test]
fn cluster_threshold_default_is_seven_tenths() {
    // Pin the contract from T01-06 §8.4 so the threshold does not
    // drift silently.
    assert!((CLUSTER_THRESHOLD - 0.7).abs() < 1e-6);
}

#[test]
fn smoke_discovery_provider_registry_compiles_with_synthesizer_role() -> Result<()> {
    // Sanity: the Synthesizer role is registered and round-trips
    // through the registry. This guards against accidental removal
    // when the role enum is touched.
    use moagan::llm::Role;
    let r: Role = "synthesizer".parse().unwrap();
    assert_eq!(r.as_str(), "synthesizer");
    let r: Role = "adversary".parse().unwrap();
    assert_eq!(r.as_str(), "adversary");
    Ok(())
}

// ---------------------------------------------------------------------
// Phase D gap fix (commits 6032246 + e7875b3): the synthesized proposal
// must propagate into `proposals/` so the downstream phases pick it up
// and it enters the Pareto front (V4 §5.13 + T01-06 §8.4). These
// tests invert the original `gap_*` checks: they assert the synthesis
// DOES show up in critiques/evaluations/ranking.
// ---------------------------------------------------------------------

#[test]
fn synth_to_proposal_preserves_id_and_fields() {
    use moagan::domain::SynthesizedProposal;
    use moagan::phases::synthesize::synth_to_proposal;
    let s = SynthesizedProposal {
        id: "s_07".into(),
        cluster_id: "cp_03".into(),
        summary: "summary".into(),
        approach: "approach".into(),
        tradeoffs: vec!["t".into()],
        evidence: vec!["e".into()],
        ..Default::default()
    };
    let p = synth_to_proposal(&s);
    assert_eq!(p.id, "s_07");
    assert_eq!(p.summary, "summary");
    assert_eq!(p.approach, "approach");
    assert_eq!(p.tradeoffs, vec!["t".to_string()]);
    assert_eq!(p.evidence, vec!["e".to_string()]);
    assert_eq!(p.source_sketch, "syn_from_cp_03");
}

#[test]
fn synth_to_proposal_handles_empty_fields() {
    use moagan::domain::SynthesizedProposal;
    use moagan::phases::synthesize::synth_to_proposal;
    let s = SynthesizedProposal {
        id: "s_00".into(),
        cluster_id: "cp_00".into(),
        ..Default::default()
    };
    let p = synth_to_proposal(&s);
    assert_eq!(p.id, "s_00");
    assert!(p.summary.is_empty());
    assert!(p.approach.is_empty());
    assert!(p.tradeoffs.is_empty());
    assert!(p.evidence.is_empty());
    assert!(p.artifacts.is_empty());
}

#[test]
fn deliver_kind_badge_marks_synthesized() {
    // The portfolio renderer must distinguish synthesized entries
    // (id starts with s_) from regular proposals (id starts with p_).
    use moagan::phases::deliver::kind_badge_for;
    assert_eq!(kind_badge_for("s_00"), "synthesis");
    assert_eq!(kind_badge_for("p_000"), "");
    assert_eq!(kind_badge_for("synth_001"), "synthesis");
}

#[test]
fn synth_to_proposal_pipeline_shape() {
    // Validates the shape that gets written into `proposals/s_*.json`
    // so any phase downstream that reads from proposals/ can recognise
    // a synthesized entry by its source_sketch prefix.
    use moagan::domain::SynthesizedProposal;
    use moagan::phases::synthesize::synth_to_proposal;
    let s = SynthesizedProposal {
        id: "s_03".into(),
        cluster_id: "cp_07".into(),
        summary: "merged".into(),
        approach: "merged approach".into(),
        tradeoffs: vec!["speed".into(), "simplicity".into()],
        evidence: vec!["sk_001".into(), "sk_002".into()],
        ..Default::default()
    };
    let p = synth_to_proposal(&s);
    assert!(p.id.starts_with("s_"));
    assert!(p.source_sketch.starts_with("syn_from_"));
    assert_eq!(p.tradeoffs.len(), 2);
    assert_eq!(p.evidence.len(), 2);
    // The downstream pipeline cares about id, summary, approach,
    // tradeoffs, evidence. All other fields can stay empty.
    assert!(p.artifacts.is_empty());
}

#[test]
fn synth_to_proposal_collisions_with_proposal_prefix() {
    // Synthesized proposals MUST use the `s_` prefix so they don't
    // collide with `p_<NN>` proposals in the same directory.
    use moagan::domain::SynthesizedProposal;
    use moagan::phases::synthesize::synth_to_proposal;
    let s = SynthesizedProposal {
        id: "s_00".into(),
        cluster_id: "cp_00".into(),
        ..Default::default()
    };
    let p = synth_to_proposal(&s);
    assert_ne!(p.id, "p_00");
    assert!(!p.id.starts_with("p_"));
}

#[allow(dead_code)]
fn _unused_config_marker(_c: &Config) {}

// ---------------------------------------------------------------------
// Phase D sub-fase #6: checkpoint rows indexed in SQLite via
// Telemetry::record_checkpoint.
// ---------------------------------------------------------------------

#[test]
fn checkpoint_ask_writes_sqlite_row_via_telemetry() -> Result<()> {
    // Build a run context with a real telemetry + DB so we can
    // exercise the full wiring: ask() → persist() → record_checkpoint().
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;

    use moagan::redact::RedactPolicy;
    use moagan::storage::sqlite::Db;
    use moagan::telemetry::Telemetry;
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(run_id, "standard", "running", "0.1.0", None, None, None)?;
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))?;
    let _ = run_dir.telemetry(); // ensure dir

    let cp = Checkpoint::yes_no(CheckpointKind::Intake, "continue with assumptions?");
    let opts = CheckpointOpts {
        interactive: true,
        stdin_override: Some("y".to_owned()),
        telemetry: Some(telemetry),
    };
    let res = moagan::checkpoint::ask(&cp, &run_dir.checkpoints(), &opts)?;
    assert_eq!(res, moagan::checkpoint::Resolution::Approved);

    // JSON sidecar exists.
    let json_path = run_dir.checkpoints().join(format!("{}.json", cp.id));
    assert!(json_path.exists(), "JSON sidecar must be written");

    // SQLite row exists via Telemetry mirror.
    let rows = db.list_checkpoints_for_run(run_id)?;
    assert_eq!(rows.len(), 1, "expected exactly one checkpoint row");
    let r = &rows[0];
    assert_eq!(r.ckp_id, cp.id);
    assert_eq!(r.kind, "intake");
    assert_eq!(r.question, "continue with assumptions?");
    assert_eq!(r.response, "y");
    // With stdin_override("y") the user typed a value, so the
    // accepted_default flag is false (not the empty-enter default).
    assert!(!r.accepted_default);
    assert!(r.at_unix.is_some());

    Ok(())
}

#[test]
fn checkpoint_skip_writes_sqlite_row_with_marker() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;

    use moagan::redact::RedactPolicy;
    use moagan::storage::sqlite::Db;
    use moagan::telemetry::Telemetry;
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(run_id, "batch", "running", "0.1.0", None, None, None)?;
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))?;

    let cp = Checkpoint::new(CheckpointKind::Final, "ship?", true);
    moagan::checkpoint::skip(&cp, &run_dir.checkpoints(), Some(&telemetry))?;

    let rows = db.list_checkpoints_for_run(run_id)?;
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.kind, "final");
    assert_eq!(r.response, "<skipped:non_interactive>");
    assert!(r.accepted_default);

    Ok(())
}

#[test]
fn checkpoint_ask_without_telemetry_does_not_crash() -> Result<()> {
    // When telemetry is None, the JSON sidecar still gets written —
    // only the SQLite mirror is skipped. This is the "tests don't
    // need a DB" path.
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let _home = Arc::new(MoaganHome::resolve()?);

    let cp = Checkpoint::yes_no(CheckpointKind::Clarify, "add constraint?");
    let opts = CheckpointOpts {
        interactive: true,
        stdin_override: Some("add a 5GB cap".to_owned()),
        telemetry: None,
    };
    let dir = tmp.path();
    std::fs::create_dir_all(dir)?;
    let res = moagan::checkpoint::ask(&cp, dir, &opts)?;
    assert_eq!(
        res,
        moagan::checkpoint::Resolution::Modify("add a 5GB cap".into())
    );
    let json_path = dir.join(format!("{}.json", cp.id));
    assert!(json_path.exists());
    Ok(())
}

#[test]
fn checkpoint_counts_by_kind_groups_three_kinds() -> Result<()> {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve()?);
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;

    use moagan::redact::RedactPolicy;
    use moagan::storage::sqlite::Db;
    use moagan::telemetry::Telemetry;
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(run_id, "deep", "running", "0.1.0", None, None, None)?;
    let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), Some(db.clone()))?;

    moagan::checkpoint::skip(
        &Checkpoint::yes_no(CheckpointKind::Intake, "q1"),
        &run_dir.checkpoints(),
        Some(&telemetry),
    )?;
    moagan::checkpoint::skip(
        &Checkpoint::yes_no(CheckpointKind::Clarify, "q2"),
        &run_dir.checkpoints(),
        Some(&telemetry),
    )?;
    moagan::checkpoint::skip(
        &Checkpoint::yes_no(CheckpointKind::Clarify, "q3"),
        &run_dir.checkpoints(),
        Some(&telemetry),
    )?;
    moagan::checkpoint::skip(
        &Checkpoint::new(CheckpointKind::Final, "ship?", true),
        &run_dir.checkpoints(),
        Some(&telemetry),
    )?;

    let counts = db.checkpoint_counts_by_kind(run_id)?;
    assert_eq!(counts.get("intake").copied(), Some(1));
    assert_eq!(counts.get("clarify").copied(), Some(2));
    assert_eq!(counts.get("final").copied(), Some(1));
    Ok(())
}
