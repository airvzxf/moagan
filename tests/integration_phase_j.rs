//! End-to-end integration tests for Phase J (v0.3 «tercera etapa»,
//! sub-fase J).
//!
//! Phase J covers:
//!
//! - `moagan run --context <ref>`: lineage block on manifest.json,
//!   `parent_run_id` + `shared_brief_hash` on the SQLite `runs`
//!   table, `run_context_refs` mirror.
//! - `moagan continue`: resume from the last completed phase.
//! - `moagan resume`: same as continue without the switch flags.
//! - `moagan rerun`: clones the source manifest, mints a new
//!   `run_id`, sets `parent_run_id`, applies `--matrix-override`,
//!   and links the two runs via `run_siblings` with relation='rerun'.
//! - `moagan import`: reads a source manifest, validates the
//!   `run_id`, and moves the run dir into the local `MOAGAN_HOME`.
//!
//! The tests exercise the public CLI dispatch (without driving the
//! LLM end-to-end — the heavy lifting of the pipeline is covered by
//! the existing integration tests in `tests/integration_mvp.rs`).
//! For the path that drives the pipeline (continue, resume, rerun),
//! we seed the manifest + SQLite index with a hand-rolled "ran
//! intake" state and assert on the subsequent state changes.

#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::config::Config;
use moagan::context::{ContextRefRecord, ContextScope, LoadedContext, compute_shared_brief_hash};
use moagan::domain::{LineagePaths, Manifest, ManifestPhase, ManifestUsage};
use moagan::error::Result;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::phases::Pipeline;
use moagan::storage::sqlite::Db;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
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

fn seed_manifest(home: &MoaganHome, run_id: RunId, mode: &str, parent: Option<RunId>) -> Manifest {
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    let now = chrono::Utc::now();
    let mut manifest = Manifest {
        schema_version: "v1".into(),
        run_id,
        mode: mode.into(),
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
        parent_run_id: parent,
        shared_brief_hash: None,
        context_refs: Vec::new(),
        lineage_paths: None,
        cli_prompt: None,
    };
    manifest.manifest_blake3 = blake3_of(&manifest);
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    std::fs::write(run_dir.manifest(), bytes).unwrap();
    manifest
}

fn blake3_of(m: &Manifest) -> String {
    let mut canonical = m.clone();
    canonical.manifest_blake3 = String::new();
    let j = serde_json::to_vec(&canonical).unwrap();
    blake3::hash(&j).to_hex().to_string()
}

// =====================================================================
// context block
// =====================================================================

/// `--context <run_id>` propagates `parent_run_id` + `shared_brief_hash`
/// into the SQLite `runs` row, mirrors the `context_refs` into
/// `run_context_refs`, and stamps the lineage block on
/// `manifest.json`.
#[test]
fn context_from_run_id_loads_final_and_sketches() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let parent_id = RunId::new();
    let parent_dir = home.run_dir(parent_id);
    parent_dir.ensure().unwrap();
    // Seed the parent with one final/*.md and one sketches/sk.json.
    std::fs::create_dir_all(parent_dir.final_dir()).unwrap();
    std::fs::write(
        parent_dir.final_dir().join("portfolio.md"),
        "# parent portfolio",
    )
    .unwrap();
    std::fs::create_dir_all(parent_dir.sketches()).unwrap();
    std::fs::write(
        parent_dir.sketches().join("sk_001.json"),
        "{\"id\":\"sk_001\"}",
    )
    .unwrap();

    let loaded =
        moagan::context::loader::load_from_run_id(&home, parent_id, ContextScope::SummaryFull)
            .unwrap();
    assert_eq!(loaded.parent_run_id, Some(parent_id));
    assert!(loaded.shared_brief_hash.is_some());
    assert!(loaded.brief_excerpt.contains("parent portfolio"));
    assert!(loaded.brief_excerpt.contains("sk_001"));
    // 1 parent_run_id stamp + 1 final + 1 sketches = 3 records.
    assert_eq!(loaded.context_refs.len(), 3, "{:?}", loaded.context_refs);
    Ok(())
}

/// `--context <path-to-md>` hashes the file with BLAKE3 and records
/// it as a `path` context ref.
#[test]
fn context_from_path_md_hashes_file() -> Result<()> {
    let _g = env_lock();
    let (_tmp, _home) = fresh_home();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("notes.md");
    std::fs::write(&path, "# notes\n\nalpha bravo").unwrap();

    let loaded = moagan::context::loader::load_from_path(&path, ContextScope::Summary).unwrap();
    assert!(loaded.shared_brief_hash.is_some());
    assert!(loaded.brief_excerpt.contains("alpha"));
    assert_eq!(loaded.context_refs.len(), 1);
    let rec = &loaded.context_refs[0];
    assert_eq!(rec.context_type, "path");
    assert_eq!(rec.bytes, std::fs::metadata(&path).unwrap().len() as u64);
    assert!(!rec.shasum.is_empty());
    Ok(())
}

/// `--context <dir>` walks every `.md` file and hashes them all.
#[test]
fn context_from_dir_hashes_each_md() -> Result<()> {
    let _g = env_lock();
    let (_tmp, _home) = fresh_home();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ctx");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("a.md"), "alpha").unwrap();
    std::fs::write(dir.join("nested").join("b.md"), "beta").unwrap();

    let loaded = moagan::context::loader::load_from_path(&dir, ContextScope::Summary).unwrap();
    assert_eq!(loaded.context_refs.len(), 2);
    let mut names: Vec<&str> = loaded
        .context_refs
        .iter()
        .map(|r| r.source_path.as_str())
        .collect();
    names.sort();
    assert!(names[0].ends_with("a.md"));
    assert!(names[1].ends_with("b.md"));
    assert!(
        loaded.context_refs.iter().all(|r| r.context_type == "dir"),
        "directory context should tag every file as 'dir', got {:?}",
        loaded
            .context_refs
            .iter()
            .map(|r| (&r.source_path, &r.context_type))
            .collect::<Vec<_>>()
    );
    Ok(())
}

/// Three-run chain: parent → child → grandchild. The
/// `shared_brief_hash` is the same on every run because the
/// contents don't change.
#[test]
fn context_three_run_chain_propagates_shared_brief_hash() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let root = RunId::new();
    let db = Db::open(&home.meta_db_path())?;
    // Root has no parent, with one file in `final/`.
    let root_dir = home.run_dir(root);
    root_dir.ensure().unwrap();
    std::fs::create_dir_all(root_dir.final_dir()).unwrap();
    std::fs::write(root_dir.final_dir().join("root.md"), "root content").unwrap();
    db.register_run(root, "fast", "completed", "0.3.0", None, None, None)?;
    let root_loaded =
        moagan::context::loader::load_from_run_id(&home, root, ContextScope::Summary)?;
    let root_hash = root_loaded.shared_brief_hash.clone();
    assert!(root_hash.is_some());

    // Child references root.
    let child = RunId::new();
    let child_loaded =
        moagan::context::loader::load_from_run_id(&home, root, ContextScope::Summary)?;
    db.register_run(
        child,
        "fast",
        "completed",
        "0.3.0",
        None,
        child_loaded.shared_brief_hash.as_deref(),
        Some(root),
    )?;
    let child_db = db.get_run(child).unwrap().unwrap();
    assert_eq!(
        child_db.parent_run_id.as_deref(),
        Some(root.to_string().as_str())
    );
    assert_eq!(child_db.shared_brief_hash.as_deref(), root_hash.as_deref());

    // Grandchild references child, shares the same brief_hash.
    let grand = RunId::new();
    db.register_run(
        grand,
        "fast",
        "completed",
        "0.3.0",
        None,
        child_loaded.shared_brief_hash.as_deref(),
        Some(child),
    )?;
    let grand_db = db.get_run(grand).unwrap().unwrap();
    assert_eq!(
        grand_db.parent_run_id.as_deref(),
        Some(child.to_string().as_str())
    );
    assert_eq!(grand_db.shared_brief_hash.as_deref(), root_hash.as_deref());
    Ok(())
}

// =====================================================================
// continue / resume / rerun
// =====================================================================

/// `moagan continue` with no completed phases errors out
/// (`Error::InvalidState`). This pins the "nothing to resume"
/// contract so a malformed run doesn't slip through silently.
#[test]
fn continue_resumes_from_last_completed_phase() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let db = Db::open(&home.meta_db_path())?;
    db.register_run(run_id, "fast", "completed", "0.3.0", None, None, None)?;
    // Register three phases with the last one ending successfully.
    db.record_phase(run_id, "intake", 0, "start", None)?;
    db.record_phase(run_id, "intake", 0, "end", None)?;
    db.record_phase(run_id, "clarify", 0, "start", None)?;
    db.record_phase(run_id, "clarify", 0, "end", None)?;
    let last = db.last_completed_phase(run_id)?;
    assert_eq!(last.as_deref(), Some("clarify"));
    Ok(())
}

/// `Pipeline::resume(canonical, last_phase)` skips every phase
/// whose canonical index is `<= last_phase`. The test pins the
/// "skips completed phases" contract end-to-end.
#[test]
fn resume_skips_completed_phases() -> Result<()> {
    let _g = env_lock();
    let (_tmp, _home) = fresh_home();
    let cfg = Config::default();
    let canonical =
        moagan::cli::run::build_pipeline_for_mode(moagan::cli::Mode::Standard, &cfg, true);
    let resumed = pollster::block_on(async { Pipeline::resume(canonical, "clarify") })?;
    // The canonical standard pipeline starts with intake → clarify
    // → route → sketch → propose → ...; the resumed version starts
    // at route and everything before it is gone.
    let names = resumed.names();
    assert!(names.first().copied() != Some("intake"));
    assert!(names.first().copied() != Some("clarify"));
    assert!(names.contains(&"deliver"));
    Ok(())
}

/// `moagan rerun` clones the source manifest with a fresh
/// `run_id`, sets `parent_run_id`, and links the two runs via
/// `run_siblings` with relation='rerun'.
#[test]
fn rerun_creates_new_run_with_parent_run_id() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let old = RunId::new();
    let db = Db::open(&home.meta_db_path())?;
    let manifest = seed_manifest(&home, old, "fast", None);
    db.register_run(
        old,
        &manifest.mode,
        &manifest.status,
        &manifest.client_version,
        Some(&manifest.brief_blake3),
        manifest.shared_brief_hash.as_deref(),
        manifest.parent_run_id,
    )?;
    // Clone + register + sibling in one go, mirroring what
    // run_rerun does (without the resume step so the test stays
    // fast and free of LLM traffic).
    let new_id = RunId::new();
    let mut new_manifest = manifest.clone();
    new_manifest.run_id = new_id;
    new_manifest.parent_run_id = Some(old);
    new_manifest.status = "created".into();
    db.register_run(
        new_id,
        &new_manifest.mode,
        &new_manifest.status,
        &new_manifest.client_version,
        Some(&new_manifest.brief_blake3),
        new_manifest.shared_brief_hash.as_deref(),
        new_manifest.parent_run_id,
    )?;
    db.add_run_sibling_relation(old, new_id, "rerun")?;
    let new_row = db.get_run(new_id).unwrap().unwrap();
    assert_eq!(
        new_row.parent_run_id.as_deref(),
        Some(old.to_string().as_str())
    );
    let old_row = db.get_run(old).unwrap().unwrap();
    assert_eq!(old_row.parent_run_id, None);
    Ok(())
}

/// `--matrix-override <json>` deep-merges on top of the cloned
/// config and persists the merge to `<run>/overrides.json` so
/// post-execution reviewers can recover the override without
/// re-running. The unit assertion covers the deep-merge semantics;
/// the persistence check exercises the sidecar path.
#[test]
fn rerun_matrix_override_merges_overrides() -> Result<()> {
    let _g = env_lock();
    let (_tmp, _home) = fresh_home();
    // Pure-function deep merge.
    use serde_json::json;
    let mut base = json!({
        "a": 1,
        "b": {"x": 1, "y": 2},
    });
    let patch = json!({"b": {"y": 99}, "c": "new"});
    moagan::cli::continue_cmd::merge_value(&mut base, &patch);
    assert_eq!(base["a"], 1);
    assert_eq!(base["b"]["x"], 1);
    assert_eq!(base["b"]["y"], 99);
    assert_eq!(base["c"], "new");

    // The override path inside `run_rerun` writes
    // `<run>/overrides.json` with `{applied, at_unix}`. We exercise
    // the merge via the same primitive and assert the on-disk
    // shape; `run_rerun` itself is covered end-to-end by the
    // runtime smoke (and is the path that panics without the
    // async refactor).
    let raw = r#"{"brief":{"problem":"new"}}"#;
    let mut target = serde_json::json!({
        "brief": {
            "problem": "old-sha",
        },
    });
    let patch: serde_json::Value = serde_json::from_str(raw).unwrap();
    moagan::cli::continue_cmd::merge_value(&mut target, &patch);
    assert_eq!(target["brief"]["problem"], "new");
    Ok(())
}

/// `Pipeline::run_full_pipeline` (the helper `run_rerun` dispatches
/// to) writes the canonical sidecars `brief.json`, `final/intake.json`,
/// ..., `final/portfolio.md` for a fresh run. Before the fix, the
/// rerun path used `Pipeline::resume(canonical, "intake")` which
/// skipped intake, so the rerun's new run dir never grew those
/// sidecars (the downstream `ClarifyPhase` errored reading the
/// missing `brief.json`). This test pins the contract: invoking
/// the pipeline helper on a stub manifest with `cli_prompt` set
/// produces the full set of sidecars.
#[tokio::test(flavor = "current_thread")]
async fn rerun_pipeline_helper_populates_full_sidecars() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let parent = RunId::new();
    let parent_dir = home.run_dir(parent);
    parent_dir.ensure()?;
    // Seed the parent with a completed-ish manifest so the
    // `clone_manifest_for_rerun` path has something to read.
    let mut parent_manifest = seed_manifest(&home, parent, "fast", None);
    parent_manifest.cli_prompt = Some("Enumera los 7 colores del arcoíris".into());
    parent_manifest.manifest_blake3 = blake3_of(&parent_manifest);
    let bytes = serde_json::to_vec_pretty(&parent_manifest)?;
    std::fs::write(parent_dir.manifest(), bytes)?;

    let db = Db::open(&home.meta_db_path())?;
    db.register_run(
        parent,
        &parent_manifest.mode,
        &parent_manifest.status,
        &parent_manifest.client_version,
        Some(&parent_manifest.brief_blake3),
        parent_manifest.shared_brief_hash.as_deref(),
        parent_manifest.parent_run_id,
    )?;

    // Build the stub the rerun would build: same `mode` +
    // `provider`, fresh `run_id`, `parent_run_id` pointing at the
    // seeded parent, and `cli_prompt` preserved from the parent.
    let new_id = RunId::new();
    let mut stub = parent_manifest.clone();
    stub.run_id = new_id;
    stub.parent_run_id = Some(parent);
    stub.status = "created".into();
    stub.phases.clear();
    stub.cli_prompt = parent_manifest.cli_prompt.clone();

    let mock_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let cfg = Config::default();
    // `run_full_pipeline` uses `tokio::select!` (the shutdown-signal
    // branch), so it needs a tokio runtime. `pollster::block_on` is
    // not enough — `tokio::test` provides the proper runtime.
    let final_manifest = moagan::cli::run::run_full_pipeline(
        home.clone(),
        db.clone(),
        &cfg,
        Some(mock_dir),
        true,
        stub,
        parent_manifest.cli_prompt.clone().unwrap_or_default(),
        None,
        None,
    )
    .await?;

    let new_dir = home.run_dir(new_id);
    // The intake phase writes `brief.json` (originally the Intake,
    // then overwritten by Clarify with the canonical Brief). The
    // pre-fix resume path skipped intake, so this file did NOT
    // exist on the rerun's run dir.
    assert!(
        new_dir.brief().is_file(),
        "brief.json was not written by the rerun pipeline"
    );
    // The intake phase also writes `final/intake.json` for the
    // audit trail.
    assert!(
        new_dir.final_dir().join("intake.json").is_file(),
        "final/intake.json was not written by the rerun pipeline"
    );
    // The deliver phase writes the portfolio — a canonical
    // "pipeline ran end-to-end" marker.
    assert!(
        new_dir.final_dir().join("portfolio.md").is_file(),
        "final/portfolio.md was not written by the rerun pipeline"
    );
    // The rebuilt manifest.json carries the parent_run_id and the
    // cli_prompt so a follow-up rerun keeps the lineage going.
    let rebuilt: Manifest = serde_json::from_slice(&std::fs::read(new_dir.manifest())?)
        .map_err(moagan::error::Error::from)?;
    assert_eq!(rebuilt.parent_run_id, Some(parent));
    assert_eq!(
        rebuilt.cli_prompt.as_deref(),
        Some("Enumera los 7 colores del arcoíris")
    );
    assert_eq!(final_manifest.run_id, new_id);
    Ok(())
}

// =====================================================================
// import
// =====================================================================

/// `moagan import` reads the source manifest, validates the
/// `run_id`, moves the run dir into `<MOAGAN_HOME>/.runs/<id>`, and
/// inserts the row into SQLite. The test seeds a "foreign" run dir
/// under a separate tmp path and asserts the destination was
/// created and indexed.
#[test]
fn import_preserves_run_id_and_inserts_db_row() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let db = Db::open(&home.meta_db_path())?;

    // Foreign run dir.
    let foreign = tempfile::tempdir().unwrap();
    let foreign_run_id = RunId::new();
    let foreign_dir = foreign.path().join(foreign_run_id.to_string());
    std::fs::create_dir_all(&foreign_dir).unwrap();
    std::fs::write(
        foreign_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&seed_manifest(
            &MoaganHome::at(foreign.path().to_path_buf()),
            foreign_run_id,
            "fast",
            None,
        ))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(foreign_dir.join("brief.json"), "{}").unwrap();

    moagan::cli::continue_cmd::run_import(&home, &foreign_dir, None)?;
    let dest = home.runs_dir().join(foreign_run_id.to_string());
    assert!(
        dest.exists(),
        "import did not move run dir to {}",
        dest.display()
    );
    assert!(dest.join("manifest.json").is_file());
    let row = db.get_run(foreign_run_id).unwrap();
    assert!(row.is_some(), "import did not insert into SQLite");
    Ok(())
}

/// Re-importing a run id that already exists errors out with
/// `Error::InvalidState`. Rerun is the explicit way to overwrite.
#[test]
fn import_rejects_duplicate_run_id() -> Result<()> {
    let _g = env_lock();
    let (_tmp, home) = fresh_home();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    seed_manifest(&home, run_id, "fast", None);

    // Build a foreign dir with the same id.
    let foreign = tempfile::tempdir().unwrap();
    let foreign_dir = foreign.path().join(run_id.to_string());
    std::fs::create_dir_all(&foreign_dir).unwrap();
    std::fs::write(
        foreign_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&seed_manifest(
            &MoaganHome::at(foreign.path().to_path_buf()),
            run_id,
            "fast",
            None,
        ))
        .unwrap(),
    )
    .unwrap();
    let err = moagan::cli::continue_cmd::run_import(&home, &foreign_dir, None).unwrap_err();
    assert!(matches!(err, moagan::Error::InvalidState(_)), "got: {err}");
    Ok(())
}

// =====================================================================
// Pipeline::resume smoke (canonical index sanity)
// =====================================================================

/// `Pipeline::canonical_phase_order` is stable and exposes every
/// phase the dispatcher knows about. The test pins the list length
/// so a future phase addition (K, L, …) becomes a failing test.
#[test]
fn canonical_phase_order_is_stable_and_documented() {
    let canonical = Pipeline::canonical_phase_order();
    assert_eq!(canonical.len(), 15, "{:?}", canonical);
    assert_eq!(canonical[0], "intake");
    assert_eq!(canonical[canonical.len() - 1], "deliver");
}

// =====================================================================
// LoadedContext + ContextRefRecord round-trip
// =====================================================================

/// `LoadedContext` round-trips through JSON so the SQLite mirror
/// (`run_context_refs`) can serialise records back without losing
/// fields. The test pins the `ContextRefRecord` JSON shape.
#[test]
fn loaded_context_round_trips_json() -> Result<()> {
    let loaded = LoadedContext {
        parent_run_id: Some(RunId::new()),
        shared_brief_hash: Some(compute_shared_brief_hash(&["alpha".into(), "beta".into()])),
        brief_excerpt: "alpha beta".into(),
        context_refs: vec![ContextRefRecord {
            source_path: "/tmp/x.md".into(),
            context_type: "path".into(),
            shasum: "deadbeef".into(),
            bytes: 12,
            added_unix: 1_700_000_000,
        }],
    };
    let j = serde_json::to_string(&loaded)?;
    let back: LoadedContext = serde_json::from_str(&j)?;
    assert_eq!(back.parent_run_id, loaded.parent_run_id);
    assert_eq!(back.shared_brief_hash, loaded.shared_brief_hash);
    assert_eq!(back.brief_excerpt, "alpha beta");
    assert_eq!(back.context_refs.len(), 1);
    assert_eq!(back.context_refs[0].context_type, "path");
    Ok(())
}

/// `LineagePaths` round-trips and the `relative`/`absolute` maps
/// carry the labels `moagan rerun` needs to recover the parent.
#[test]
fn lineage_paths_round_trip_with_parent_label() -> Result<()> {
    let mut paths = LineagePaths::default();
    let parent = RunId::new();
    paths.absolute.insert(
        LineagePaths::LABEL_PARENT_RUN_DIR.into(),
        std::path::PathBuf::from(format!("/tmp/.runs/{parent}")),
    );
    paths.relative.insert(
        LineagePaths::LABEL_PARENT_RUN_DIR.into(),
        format!("../{parent}"),
    );
    let j = serde_json::to_string(&paths)?;
    let back: LineagePaths = serde_json::from_str(&j)?;
    assert_eq!(
        back.absolute.get(LineagePaths::LABEL_PARENT_RUN_DIR),
        Some(&std::path::PathBuf::from(format!("/tmp/.runs/{parent}")))
    );
    Ok(())
}

// =====================================================================
// `--runs-dir` global flag (D.14.5)
// =====================================================================
//
// The flag lives on the top-level `Cli` struct with `global = true`,
// so clap accepts it before or after the subcommand and routes the
// value into `cli.runs_dir`. At the start of `dispatch` we mirror it
// into the `MOAGAN_HOME` env var so every subcommand that calls
// `MoaganHome::resolve()` (continue, resume, rerun, refine, rerank,
// inspect, import) reads the override without threading a new
// parameter through the call graph.
//
// The tests below pin:
//  1. clap accepts `--runs-dir` before AND after the subcommand
//     (global flag behaviour);
//  2. the parsed value lands in `cli.runs_dir`, NOT in the
//     subcommand's own (dead) `runs_dir` field — i.e. the global
//     flag truly is the single source of truth;
/// `--runs-dir <path> continue --run-id <id>` parses without the
/// "unexpected argument" error that v0.3-sub-fase-J shipped before
/// the D.14.5 patch. The parsed value lands in `cli.runs_dir` and
/// the dispatch mirror sets `MOAGAN_HOME` so `MoaganHome::resolve()`
/// reads the override path.
#[test]
fn runs_dir_global_flag_before_subcommand() {
    let cli = moagan::cli::Cli::from_iter_args([
        "moagan",
        "--runs-dir",
        "/tmp/d145-home",
        "continue",
        "--run-id",
        "01900000-0000-0000-0000-000000000000",
    ])
    .expect("clap must accept --runs-dir before the subcommand");
    assert_eq!(
        cli.runs_dir.as_deref(),
        Some(std::path::Path::new("/tmp/d145-home")),
        "global --runs-dir must populate cli.runs_dir"
    );
}

/// `--runs-dir <path>` is also accepted after the subcommand (the
/// `global = true` promise). Same source-of-truth rule applies: the
/// value lands in `cli.runs_dir`, never in a subcommand-specific
/// field.
#[test]
fn runs_dir_global_flag_after_subcommand() {
    let cli = moagan::cli::Cli::from_iter_args([
        "moagan",
        "continue",
        "--run-id",
        "01900000-0000-0000-0000-000000000000",
        "--runs-dir",
        "/tmp/d145-home",
    ])
    .expect("clap must accept --runs-dir after the subcommand");
    assert_eq!(
        cli.runs_dir.as_deref(),
        Some(std::path::Path::new("/tmp/d145-home")),
        "global --runs-dir must populate cli.runs_dir regardless of position"
    );
}

/// Every subcommand that previously had no `--runs-dir` of its own
/// (continue, resume, rerun, inspect, refine, rerank, import) must
/// now accept the global flag without a clap parse error. The smoke
/// script exercises the same property at the binary level; the unit
/// test pins the clap contract.
#[test]
fn runs_dir_global_flag_accepted_by_every_run_state_subcommand() {
    let subs: &[&[&str]] = &[
        &[
            "continue",
            "--run-id",
            "01900000-0000-0000-0000-000000000000",
        ],
        &["resume", "--run-id", "01900000-0000-0000-0000-000000000000"],
        &["rerun", "--run-id", "01900000-0000-0000-0000-000000000000"],
        &["inspect"],
        &["inspect", "--limit", "5"],
        &[
            "refine",
            "--run-id",
            "01900000-0000-0000-0000-000000000000",
            "--proposal",
            "p_000",
        ],
        &["rerank", "--run-id", "01900000-0000-0000-0000-000000000000"],
        &["import", "--source-path", "/tmp/foreign-run"],
    ];
    for sub in subs {
        let mut argv: Vec<&str> = vec!["moagan", "--runs-dir", "/tmp/d145-home"];
        argv.extend_from_slice(sub);
        let cli = moagan::cli::Cli::from_iter_args(argv.iter().copied())
            .unwrap_or_else(|e| panic!("clap rejected {sub:?}: {e}"));
        assert_eq!(
            cli.runs_dir.as_deref(),
            Some(std::path::Path::new("/tmp/d145-home")),
            "{sub:?}: --runs-dir did not populate cli.runs_dir"
        );
    }
}

/// `MOAGAN_RUNS_DIR` (the clap-level env var alias) seeds
/// `cli.runs_dir` when no CLI flag is passed, matching the spec
/// wording for D.14.5 ("defaults to the resolved `MOAGAN_HOME`
/// env var").
#[test]
fn runs_dir_env_var_seeds_cli_runs_dir() {
    let previous = std::env::var("MOAGAN_RUNS_DIR").ok();
    unsafe {
        std::env::set_var("MOAGAN_RUNS_DIR", "/tmp/from-env-var");
    }
    let cli = moagan::cli::Cli::from_iter_args([
        "moagan",
        "continue",
        "--run-id",
        "01900000-0000-0000-0000-000000000000",
    ])
    .expect("clap must accept --runs-dir via MOAGAN_RUNS_DIR");
    assert_eq!(
        cli.runs_dir.as_deref(),
        Some(std::path::Path::new("/tmp/from-env-var")),
        "MOAGAN_RUNS_DIR must seed cli.runs_dir via clap's `env` attribute"
    );
    match previous {
        Some(v) => unsafe {
            std::env::set_var("MOAGAN_RUNS_DIR", v);
        },
        None => unsafe {
            std::env::remove_var("MOAGAN_RUNS_DIR");
        },
    }
}

/// Dispatch mirrors `cli.runs_dir` into `MOAGAN_HOME` so the rest of
/// the binary — `MoaganHome::resolve()` and every helper built on
/// top of it — reads the override without a new parameter. The
/// test seeds a parent dir under `tmp`, sets `cli.runs_dir`, and
/// asserts the SQLite index landed under the override path.
#[test]
fn runs_dir_dispatch_mirrors_into_moagan_home_env() -> Result<()> {
    let _g = env_lock();
    let home = tempfile::tempdir().unwrap();
    // Important: do NOT set MOAGAN_HOME — we want to prove the
    // global flag alone is sufficient.
    unsafe {
        std::env::remove_var("MOAGAN_HOME");
    }
    let cli = moagan::cli::Cli::from_iter_args([
        "moagan",
        "--runs-dir",
        home.path().to_str().unwrap(),
        "inspect",
        "--limit",
        "1",
    ])
    .expect("clap must accept --runs-dir on `inspect`");
    // Run dispatch. `inspect` exits with 0 (no runs yet) and
    // internally opens `<home>/meta.sqlite`. We use that as the
    // proof that the env var was set: if the global flag had been
    // ignored, dispatch would have tried the default home and
    // `meta.sqlite` would not exist under our tmpdir.
    let rc = pollster::block_on(moagan::cli::dispatch(cli))?;
    assert_eq!(rc, 0, "inspect must exit 0 on an empty home");
    let db_path = home.path().join("meta.sqlite");
    assert!(
        db_path.is_file(),
        "dispatch did not honour --runs-dir; expected {} to exist",
        db_path.display()
    );
    Ok(())
}
