//! `moagan pause <run_id>`, `moagan continue --from-pause`, and
//! `moagan list --paused` — cross-process hibernation CLI surface.
//!
//! Pause serialises the current run state into `<run_dir>/paused.json`
//! via [`PausePoint`]. Continue reads that file, filters the canonical
//! pipeline through [`Pipeline::resume`] using the persisted
//! `paused_at_phase`, and cleans up the pause artefacts on success.
//! List enumerates every run directory that currently carries a
//! `paused.json`.
//!
//! The lockfile (`paused.lock`, TTL 5 min) prevents two pauses from
//! racing on the same run; the TTL also bounds how long a crashed
//! pause can keep a run wedged before a fresh pause can take over.
//!
//! PR #131 (Sesión C) shipped the CLI surface with a hard-coded
//! `paused_at_phase = "synthesize"` and a hard-coded `completed_phases`
//! list. PR D3 (K.2) replaces the hard-code with the live SQLite
//! index: `paused_at_phase` is derived from
//! [`crate::discovery::resume::derive_paused_at_phase`], and
//! `completed_phases` is derived from
//! [`crate::discovery::resume::derive_completed_phases`]. Operators
//! can still override both via `--phase` and `--completed`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::discovery::pause::PausePoint;
use crate::discovery::resume;
use crate::error::{Error, IoError, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::phases::Pipeline;
use crate::storage::sqlite::Db;

#[cfg(test)]
use crate::domain::Manifest;

/// Name of the lockfile that guards against two pauses racing on
/// the same run. Sits next to `paused.json` inside `<run_dir>/`.
const LOCK_FILENAME: &str = "paused.lock";

/// Default lockfile TTL (seconds). A pause lock older than this is
/// treated as stale and silently replaced; this bounds how long a
/// crashed `moagan pause` can keep a run wedged before a fresh
/// pause can take over.
const LOCK_TTL_SECS_DEFAULT: u64 = 300;

/// Inputs for [`run_pause`].
#[derive(Debug, Clone)]
pub struct PauseArgs {
    /// Run id to pause.
    pub run_id: RunId,
    /// Override the `paused_at_phase` field on the persisted
    /// `paused.json`. Defaults to the live DB's
    /// `last_completed_phase`, or
    /// [`resume::DEFAULT_PAUSED_AT_PHASE`] when the run is not in
    /// the index.
    pub phase: Option<String>,
    /// Override the `completed_phases` list on the persisted
    /// `paused.json`. Defaults to the live DB's
    /// `list_completed_phases(run_id)`, or
    /// [`resume::DEFAULT_COMPLETED_PHASES`] when the run is not in
    /// the index.
    pub completed: Option<Vec<String>>,
}

/// Inputs for [`run_list`].
#[derive(Debug, Clone)]
pub struct ListArgs {}

/// `moagan pause <run_id> [--phase <name>] [--completed <csv>]`
///
/// Writes a [`PausePoint`] for the run and stamps a lockfile with a
/// TTL so a second pause within the window is rejected.
///
/// The phase boundary and completed-phases list come from the live
/// SQLite index when the run is registered (PR D3, K.2); operators
/// can override either with `--phase` and `--completed`. A run
/// whose index row is missing (paused before `db.register_run(...)`
/// had a chance to commit, or imported from a wiped meta.sqlite)
/// falls back to [`resume::DEFAULT_PAUSED_AT_PHASE`] and
/// [`resume::DEFAULT_COMPLETED_PHASES`] so the pause still produces
/// a readable `paused.json`.
pub fn run_pause(home: &MoaganHome, args: PauseArgs) -> Result<i32> {
    debug!(run_id = %args.run_id, "pause::run_pause: enter");
    let run_dir = home.run_dir(args.run_id);
    if !run_dir.root().exists() {
        warn!(run_id = %args.run_id, "pause: run not on disk");
        return Err(Error::InvalidArgs(format!(
            "run {} not found at {}",
            args.run_id,
            run_dir.root().display()
        )));
    }
    acquire_lock(run_dir.root(), LOCK_TTL_SECS_DEFAULT)?;

    let (paused_at_phase, completed_phases) = resolve_pause_state(home, &args)?;
    let pp = PausePoint::new(
        args.run_id,
        paused_at_phase.clone(),
        completed_phases,
        serde_json::json!({"resumable": true}),
        format!("paused at {}", crate::time::now_unix_secs()),
    );
    pp.save(run_dir.root())?;
    info!(
        run_id = %args.run_id,
        paused_at_phase = %paused_at_phase,
        completed_phases = pp.completed_phases.len(),
        "pause: saved"
    );
    println!(
        "paused run {} at phase '{}' ({} completed phases)",
        args.run_id,
        paused_at_phase,
        pp.completed_phases.len()
    );
    Ok(0)
}

/// Resolve the `(paused_at_phase, completed_phases)` pair the pause
/// will persist. Operator overrides win; otherwise the live DB is
/// consulted; otherwise the legacy default list is used.
///
/// The DB is opened lazily so `pause` on a run whose index has not
/// been committed (no `meta.sqlite` row yet) still succeeds — the
/// open failure falls through to the default branch.
fn resolve_pause_state(home: &MoaganHome, args: &PauseArgs) -> Result<(String, Vec<String>)> {
    let db = Db::open(&home.meta_db_path()).ok();
    let registered = db
        .as_ref()
        .and_then(|d| resume::run_is_registered(d, args.run_id).ok())
        .unwrap_or(false);

    let paused_at_phase = match args.phase.clone() {
        Some(p) => p,
        None => match db.as_ref() {
            Some(d) if registered => resume::derive_paused_at_phase(d, args.run_id)?,
            _ => resume::DEFAULT_PAUSED_AT_PHASE.to_string(),
        },
    };

    let completed_phases = match args.completed.clone() {
        Some(c) => c,
        None => match db.as_ref() {
            Some(d) if registered => {
                let derived = resume::derive_completed_phases(d, args.run_id)?;
                if derived.is_empty() {
                    // Live DB registered the run but no phase ended
                    // yet — e.g. an imported run with an empty
                    // history. Fall back to the default list rather
                    // than persist an empty `completed_phases`,
                    // which would make `continue --from-pause`
                    // re-run the whole pipeline from intake.
                    resume::DEFAULT_COMPLETED_PHASES
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                } else {
                    derived
                }
            }
            _ => resume::DEFAULT_COMPLETED_PHASES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        },
    };

    Ok((paused_at_phase, completed_phases))
}

/// `moagan continue --from-pause` — load the persisted [`PausePoint`],
/// filter the canonical pipeline via [`Pipeline::resume`], run the
/// remaining phases, and clean up `paused.json` + `paused.lock` on
/// success. On failure the pause artefacts are kept so the operator
/// can inspect or retry.
pub async fn run_continue_from_pause(home: &MoaganHome, run_id: RunId) -> Result<i32> {
    debug!(run_id = %run_id, "pause::run_continue_from_pause: enter");
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        warn!(run_id = %run_id, "pause::continue: run not on disk");
        return Err(Error::InvalidArgs(format!(
            "run {run_id} not found at {}",
            run_dir.root().display()
        )));
    }
    let pp = PausePoint::load(run_dir.root())?.ok_or_else(|| {
        warn!(run_id = %run_id, "pause::continue: no paused.json");
        Error::InvalidArgs(format!(
            "no paused.json for run {run_id}; nothing to resume"
        ))
    })?;

    println!(
        "resume plan for run {}: paused at phase '{}', {} completed phases, summary: {}",
        run_id,
        pp.paused_at_phase,
        pp.completed_phases.len(),
        pp.summary
    );
    tracing::info!(
        run_id = %run_id,
        paused_at_phase = %pp.paused_at_phase,
        completed_phases = ?pp.completed_phases,
        "continue --from-pause: resuming pipeline from paused state"
    );

    let outcome = resume_paused_run(home, run_id, &pp).await;
    finalize_resume(run_dir.root(), outcome.is_ok())?;
    outcome.map(|_| 0)
}

/// Build the resumed pipeline (filtered via [`Pipeline::resume`])
/// and run the remaining phases through the canonical `resume_pipeline`
/// helper. The helper takes care of provider / telemetry /
/// parallelism wiring; the pause layer just hands it the manifest
/// and the persisted `paused_at_phase`.
async fn resume_paused_run(home: &MoaganHome, run_id: RunId, pp: &PausePoint) -> Result<()> {
    let manifest = super::continue_cmd::load_manifest(home, run_id)?;
    let mode = super::continue_cmd::parse_mode(&manifest.mode)?;
    let cfg = Config::load().unwrap_or_default();
    let canonical = super::continue_cmd::build_canonical_for_resume(&cfg, mode);
    let resumed = Pipeline::resume(canonical, &pp.paused_at_phase)?;
    if resumed.is_empty() {
        tracing::info!(
            run_id = %run_id,
            paused_at_phase = %pp.paused_at_phase,
            "continue --from-pause: nothing left to do after paused phase"
        );
        return Ok(());
    }
    super::continue_cmd::resume_pipeline(home, &manifest, &pp.paused_at_phase, None, true).await
}

/// Decide whether the pause artefacts should be removed and act on
/// it. On success both `paused.json` and `paused.lock` are deleted
/// so the next `moagan pause` on the same run starts clean. On
/// failure the artefacts are kept so the operator can inspect the
/// state without re-deriving it.
///
/// Errors during cleanup are surfaced but do not mask the resume
/// outcome: a successful resume that fails to delete the lockfile
/// returns `Ok(0)` plus a warning, while a failed resume keeps
/// the artefacts regardless.
fn finalize_resume(run_dir: &Path, success: bool) -> Result<()> {
    if !success {
        tracing::info!(
            run_dir = %run_dir.display(),
            "resume failed; keeping paused.json + paused.lock for inspection"
        );
        return Ok(());
    }
    PausePoint::delete(run_dir)?;
    let lock_path = run_dir.join(LOCK_FILENAME);
    if lock_path.exists() {
        std::fs::remove_file(&lock_path).map_err(|e| {
            Error::Io(IoError::Raw(std::io::Error::other(format!(
                "failed to remove {}: {e}",
                lock_path.display()
            ))))
        })?;
    }
    Ok(())
}

/// Compute the resumed pipeline (filter applied, never executed) for
/// the pause artefacts on disk. Pure function over the filesystem:
/// reads `paused.json` + `manifest.json`, returns a [`Pipeline`] that
/// has been filtered via [`Pipeline::resume`]. Test-only entry
/// point that exercises the same code path as
/// [`run_continue_from_pause`] without spinning up the provider
/// registry.
#[cfg(test)]
pub(crate) fn planned_resumed_pipeline(home: &MoaganHome, run_id: RunId) -> Result<Pipeline> {
    let run_dir = home.run_dir(run_id);
    let pp = PausePoint::load(run_dir.root())?
        .ok_or_else(|| Error::InvalidArgs(format!("no paused.json for run {run_id}")))?;
    let manifest = super::continue_cmd::load_manifest(home, run_id)?;
    let mode = super::continue_cmd::parse_mode(&manifest.mode)?;
    let cfg = Config::load().unwrap_or_default();
    let canonical = super::continue_cmd::build_canonical_for_resume(&cfg, mode);
    Pipeline::resume(canonical, &pp.paused_at_phase)
}

/// `moagan list --paused` — enumerate every run directory under
/// `<home>/.runs/` that currently carries a `paused.json`. The
/// returned code is always 0; the operator checks stdout instead.
pub fn run_list(home: &MoaganHome, _args: ListArgs) -> Result<i32> {
    debug!("pause::run_list: enter");
    let runs = home.runs_dir();
    let mut found = 0;
    if let Ok(entries) = std::fs::read_dir(&runs) {
        for e in entries.flatten() {
            let p = e.path();
            if p.join("paused.json").exists() {
                let run_id = p.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                println!("  {run_id}");
                found += 1;
            }
        }
    }
    debug!(found, "pause::run_list: done");
    if found == 0 {
        println!("(no paused runs)");
    }
    Ok(0)
}

/// Acquire the lockfile at `<run_dir>/paused.lock`. Errors when a
/// fresh lock (younger than `ttl_secs`) is already present; overwrites
/// stale locks so a crashed previous pause cannot wedge the run
/// forever.
fn acquire_lock(run_dir: &Path, ttl_secs: u64) -> Result<()> {
    let lock_path: PathBuf = run_dir.join(LOCK_FILENAME);
    if lock_path.exists()
        && let Ok(meta) = lock_path.metadata()
        && let Ok(modified) = meta.modified()
    {
        let age = modified.elapsed().unwrap_or(Duration::from_secs(u64::MAX));
        if age < Duration::from_secs(ttl_secs) {
            warn!(
                lock_path = %lock_path.display(),
                age_secs = age.as_secs(),
                ttl_secs,
                "pause: lockfile held"
            );
            return Err(Error::InvalidArgs(format!(
                "paused.lock held (age {}s, ttl {}s); retry later",
                age.as_secs(),
                ttl_secs
            )));
        }
    }
    std::fs::write(&lock_path, b"locked").map_err(|e| {
        Error::Io(IoError::Write {
            path: lock_path.clone(),
            source: e,
        })
    })?;
    trace!("pause: lock acquired");
    Ok(())
}

/// Test-only helper: register a run in the SQLite index with the
/// given completed phases. Mirrors the order the pipeline writes
/// them (start + end per phase) so the resume path picks them up
/// identically to a live run.
#[cfg(test)]
fn seed_db_completed_phases(db: &Db, run_id: RunId, mode: &str, phases: &[&str]) {
    db.register_run(run_id, mode, "running", "0.4.0", None, None, None)
        .unwrap();
    for phase in phases {
        db.record_phase(run_id, phase, 0, "start", None).unwrap();
        db.record_phase(run_id, phase, 0, "end", None).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// `run_pause` writes `paused.json` and `paused.lock` for a
    /// registered run. When the DB has a recorded completed-phase
    /// history the persisted list MUST mirror it instead of the
    /// legacy hard-coded constant.
    #[test]
    fn pause_uses_db_completed_phases_when_run_active() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id).root()).unwrap();

        // Seed the DB with a partial history that differs from the
        // legacy hard-coded list (no `sketch` / `propose` / `gate`).
        let db = Db::open(&home.meta_db_path()).unwrap();
        seed_db_completed_phases(&db, run_id, "standard", &["intake", "clarify"]);

        let code = run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap();
        assert_eq!(code, 0);

        let loaded = PausePoint::load(home.run_dir(run_id).root())
            .unwrap()
            .expect("paused.json must round-trip");
        assert_eq!(loaded.paused_at_phase, "clarify");
        // `list_completed_phases` returns rows in
        // `started_unix DESC, phase ASC` order; `clarify` was
        // recorded after `intake` so it lands first.
        assert_eq!(
            loaded.completed_phases,
            vec!["clarify".to_string(), "intake".to_string()]
        );
    }

    /// `run_pause` falls back to the legacy defaults when the run
    /// is not in the SQLite index. This covers paused-before-commit
    /// and the import-from-clean-DB cases.
    #[test]
    fn pause_falls_back_to_default_when_run_not_in_db() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id).root()).unwrap();

        let code = run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap();
        assert_eq!(code, 0);

        let loaded = PausePoint::load(home.run_dir(run_id).root())
            .unwrap()
            .expect("paused.json must round-trip");
        assert_eq!(loaded.paused_at_phase, "synthesize");
        let expected: Vec<String> = resume::DEFAULT_COMPLETED_PHASES
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(loaded.completed_phases, expected);
    }

    /// Operator overrides on `--phase` and `--completed` win over
    /// both the live DB and the legacy fallback. Useful when the
    /// operator pauses a run that has already drifted from the
    /// SQLite index (e.g. `meta.sqlite` was rebuilt manually).
    #[test]
    fn pause_respects_explicit_phase_and_completed_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id).root()).unwrap();

        let code = run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: Some("deliver".to_string()),
                completed: Some(vec!["intake".to_string(), "rank".to_string()]),
            },
        )
        .unwrap();
        assert_eq!(code, 0);

        let loaded = PausePoint::load(home.run_dir(run_id).root())
            .unwrap()
            .expect("paused.json must round-trip");
        assert_eq!(loaded.paused_at_phase, "deliver");
        assert_eq!(
            loaded.completed_phases,
            vec!["intake".to_string(), "rank".to_string()]
        );
    }

    /// `run_continue_from_pause` reads `paused.json`, filters the
    /// canonical pipeline via `Pipeline::resume`, and the resumed
    /// pipeline contains only the phases strictly after the
    /// persisted `paused_at_phase`. The provider / telemetry
    /// machinery is bypassed in this test by calling
    /// [`planned_resumed_pipeline`] directly, which exercises the
    /// same code path with stubbed manifest + paused.json.
    #[test]
    fn resume_skips_completed_phases_in_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        let run_root = home.run_dir(run_id).root().to_path_buf();
        std::fs::create_dir_all(&run_root).unwrap();

        // Drop a manifest so `load_manifest` succeeds. The
        // `mode = "fast"` selects a real (small) canonical
        // pipeline; the actual phase names are read off the
        // `Pipeline::canonical_phase_order()` static list.
        let manifest = Manifest {
            run_id,
            mode: "fast".to_string(),
            ..Default::default()
        };
        std::fs::write(
            run_root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        PausePoint::new(
            run_id,
            "intake".to_string(),
            vec!["intake".to_string()],
            serde_json::json!({}),
            "paused after intake".to_string(),
        )
        .save(&run_root)
        .unwrap();

        let resumed = planned_resumed_pipeline(&home, run_id).unwrap();
        let names: Vec<&str> = resumed.names();
        assert!(
            names.contains(&"deliver"),
            "resumed pipeline must include the post-intake phases; got {names:?}"
        );
        assert!(
            !names.contains(&"intake"),
            "resumed pipeline must drop the paused phase; got {names:?}"
        );
    }

    /// `finalize_resume` deletes both `paused.json` and `paused.lock`
    /// when the resume reports success.
    #[test]
    fn resume_deletes_paused_json_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        PausePoint::new(
            RunId::new(),
            "intake".to_string(),
            vec!["intake".to_string()],
            serde_json::json!({}),
            "to be deleted".to_string(),
        )
        .save(run_dir)
        .unwrap();
        std::fs::write(run_dir.join(LOCK_FILENAME), b"locked").unwrap();

        assert!(run_dir.join("paused.json").exists());
        assert!(run_dir.join(LOCK_FILENAME).exists());

        finalize_resume(run_dir, true).unwrap();

        assert!(
            !run_dir.join("paused.json").exists(),
            "paused.json must be gone after a successful resume"
        );
        assert!(
            !run_dir.join(LOCK_FILENAME).exists(),
            "paused.lock must be gone after a successful resume"
        );
    }

    /// `finalize_resume` keeps both `paused.json` and `paused.lock`
    /// when the resume reports failure. The operator can inspect or
    /// retry the run without losing the persisted plan.
    #[test]
    fn resume_keeps_paused_json_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        PausePoint::new(
            RunId::new(),
            "intake".to_string(),
            vec!["intake".to_string()],
            serde_json::json!({}),
            "kept on failure".to_string(),
        )
        .save(run_dir)
        .unwrap();
        std::fs::write(run_dir.join(LOCK_FILENAME), b"locked").unwrap();

        finalize_resume(run_dir, false).unwrap();

        assert!(
            run_dir.join("paused.json").exists(),
            "paused.json must remain after a failed resume"
        );
        assert!(
            run_dir.join(LOCK_FILENAME).exists(),
            "paused.lock must remain after a failed resume"
        );
    }

    /// `run_pause` rejects a run id whose directory does not exist.
    /// This protects against silent typos in the CLI argument from
    /// creating a paused.json on an unintended path.
    #[test]
    fn pause_returns_invalid_args_for_missing_run() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();

        let err = run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap_err();
        match err {
            Error::InvalidArgs(msg) => {
                assert!(msg.contains("not found"), "got: {msg}");
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
        assert!(!home.run_dir(run_id).root().join("paused.json").exists());
    }

    /// `run_list` enumerates every run that carries `paused.json`.
    /// The function returns 0 and writes one line per match; the
    /// run ids present in the printed output must be a superset of
    /// the ones we actually paused (the directory may carry other
    /// runs we did not pause).
    #[test]
    fn pause_list_shows_all_paused_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id_1 = RunId::new();
        let run_id_2 = RunId::new();
        let run_id_unpaused = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id_1).root()).unwrap();
        std::fs::create_dir_all(home.run_dir(run_id_2).root()).unwrap();
        std::fs::create_dir_all(home.run_dir(run_id_unpaused).root()).unwrap();
        run_pause(
            &home,
            PauseArgs {
                run_id: run_id_1,
                phase: None,
                completed: None,
            },
        )
        .unwrap();
        run_pause(
            &home,
            PauseArgs {
                run_id: run_id_2,
                phase: None,
                completed: None,
            },
        )
        .unwrap();

        let code = run_list(&home, ListArgs {}).unwrap();
        assert_eq!(code, 0);
        assert!(home.run_dir(run_id_1).root().join("paused.json").exists());
        assert!(home.run_dir(run_id_2).root().join("paused.json").exists());
        assert!(
            !home
                .run_dir(run_id_unpaused)
                .root()
                .join("paused.json")
                .exists()
        );
    }

    /// `run_list` returns 0 and prints the empty marker when no run
    /// carries a `paused.json`. A missing `<home>/.runs/` directory
    /// must be tolerated (treated as "no paused runs").
    #[test]
    fn pause_list_returns_empty_when_no_paused_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let code = run_list(&home, ListArgs {}).unwrap();
        assert_eq!(code, 0);
    }

    /// Two pauses within the TTL must collide: the second call sees
    /// a fresh `paused.lock` and errors with an `InvalidArgs`
    /// message that mentions the TTL. The first `paused.json` stays
    /// intact (we never overwrite the file while the lock is held).
    #[test]
    fn pause_lockfile_prevents_double_pause_within_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id).root()).unwrap();

        run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap();
        let original =
            std::fs::read_to_string(home.run_dir(run_id).root().join("paused.json")).unwrap();

        let err = run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap_err();
        match err {
            Error::InvalidArgs(msg) => {
                assert!(msg.contains("paused.lock held"), "got: {msg}");
                assert!(msg.contains("ttl"), "got: {msg}");
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }

        let after =
            std::fs::read_to_string(home.run_dir(run_id).root().join("paused.json")).unwrap();
        assert_eq!(
            original, after,
            "paused.json must not be touched while the lock is held"
        );
    }

    /// After the TTL elapses a fresh pause must succeed. We rewrite
    /// the lockfile's mtime to "TTL + slack seconds ago" so the test
    /// runs in milliseconds instead of waiting for the real TTL.
    #[test]
    fn pause_lockfile_expires_after_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id).root()).unwrap();

        run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap();

        let lock_path = home.run_dir(run_id).root().join(LOCK_FILENAME);
        let aged = SystemTime::now()
            .checked_sub(Duration::from_secs(LOCK_TTL_SECS_DEFAULT + 60))
            .expect("system time must support subtraction");
        std::fs::File::open(&lock_path)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        let code = run_pause(
            &home,
            PauseArgs {
                run_id,
                phase: None,
                completed: None,
            },
        )
        .unwrap();
        assert_eq!(code, 0, "stale lock must be replaced, not rejected");
        assert!(lock_path.exists());
    }
}
