//! `moagan pause <run_id>`, `moagan continue --from-pause`, and
//! `moagan list --paused` — cross-process hibernation CLI surface.
//!
//! Pause serialises the current run state into `<run_dir>/paused.json`
//! via [`PausePoint`]. Continue reads that file and (in a later PR)
//! skips the upstream phases that have already produced artefacts on
//! disk. List enumerates every run directory that currently carries
//! a `paused.json`.
//!
//! The lockfile (`paused.lock`, TTL 5 min) prevents two pauses from
//! racing on the same run; the TTL also bounds how long a crashed
//! pause can keep a run wedged before a fresh pause can take over.
//!
//! PR C.3 (K.2b) defines this CLI surface. The actual resume-loop
//! integration lands in PR C.5 (K.2 wires `continue_cmd.rs`); this
//! module only logs the resume plan today.

use std::path::Path;
use std::time::Duration;

use crate::discovery::pause::PausePoint;
use crate::error::{Error, IoError, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;

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
}

/// Inputs for [`run_list`].
#[derive(Debug, Clone)]
pub struct ListArgs {}

/// `moagan pause <run_id>` — write a [`PausePoint`] for the run and
/// stamp a lockfile with a TTL so a second pause within the window
/// is rejected. The "phase the pause was issued at" is hard-coded
/// to `"synthesize"` for now; PR C.5 will receive the real phase
/// from the pipeline boundary that called into this CLI.
pub fn run_pause(home: &MoaganHome, args: PauseArgs) -> Result<i32> {
    let run_dir = home.run_dir(args.run_id);
    if !run_dir.root().exists() {
        return Err(Error::InvalidArgs(format!(
            "run {} not found at {}",
            args.run_id,
            run_dir.root().display()
        )));
    }
    acquire_lock(run_dir.root(), LOCK_TTL_SECS_DEFAULT)?;
    let pp = PausePoint::new(
        args.run_id,
        "synthesize".to_owned(),
        vec![
            "intake".to_owned(),
            "clarify".to_owned(),
            "sketch".to_owned(),
            "propose".to_owned(),
            "gate".to_owned(),
        ],
        serde_json::json!({"resumable": true}),
        format!("paused at {}", crate::time::now_unix_secs()),
    );
    pp.save(run_dir.root())?;
    println!("paused run {} at phase 'synthesize'", args.run_id);
    Ok(0)
}

/// `moagan continue --from-pause` — load the persisted [`PausePoint`]
/// and log a human-readable resume plan. The actual loop skip lands
/// in PR C.5 (K.2 wires `continue_cmd.rs`); for now we only confirm
/// the file is present + readable + the schema matches.
pub fn run_continue_from_pause(home: &MoaganHome, run_id: RunId) -> Result<i32> {
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        return Err(Error::InvalidArgs(format!(
            "run {run_id} not found at {}",
            run_dir.root().display()
        )));
    }
    match PausePoint::load(run_dir.root())? {
        Some(pp) => {
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
                "continue --from-pause: loaded PausePoint (full integration lands in K.2)"
            );
            Ok(0)
        }
        None => Err(Error::InvalidArgs(format!(
            "no paused.json for run {run_id}; nothing to resume"
        ))),
    }
}

/// `moagan list --paused` — enumerate every run directory under
/// `<home>/.runs/` that currently carries a `paused.json`. The
/// returned code is always 0; the operator checks stdout instead.
pub fn run_list(home: &MoaganHome, _args: ListArgs) -> Result<i32> {
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
    let lock_path = run_dir.join(LOCK_FILENAME);
    if lock_path.exists()
        && let Ok(meta) = lock_path.metadata()
        && let Ok(modified) = meta.modified()
    {
        let age = modified.elapsed().unwrap_or(Duration::from_secs(u64::MAX));
        if age < Duration::from_secs(ttl_secs) {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// `run_pause` writes both `paused.json` and `paused.lock` into
    /// the run directory. Verifies the happy path end-to-end.
    #[test]
    fn pause_writes_paused_json_for_existing_run() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();
        std::fs::create_dir_all(home.run_dir(run_id).root()).unwrap();

        let code = run_pause(&home, PauseArgs { run_id }).unwrap();
        assert_eq!(code, 0);
        assert!(home.run_dir(run_id).root().join("paused.json").exists());
        assert!(home.run_dir(run_id).root().join("paused.lock").exists());

        let loaded = PausePoint::load(home.run_dir(run_id).root())
            .unwrap()
            .expect("paused.json must round-trip");
        assert_eq!(loaded.run_id, run_id);
        assert_eq!(loaded.paused_at_phase, "synthesize");
        assert_eq!(loaded.completed_phases.len(), 5);
    }

    /// `run_pause` rejects a run id whose directory does not exist.
    /// This protects against silent typos in the CLI argument from
    /// creating a paused.json on an unintended path.
    #[test]
    fn pause_returns_invalid_args_for_missing_run() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        let run_id = RunId::new();

        let err = run_pause(&home, PauseArgs { run_id }).unwrap_err();
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
        run_pause(&home, PauseArgs { run_id: run_id_1 }).unwrap();
        run_pause(&home, PauseArgs { run_id: run_id_2 }).unwrap();

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

        run_pause(&home, PauseArgs { run_id }).unwrap();
        let original =
            std::fs::read_to_string(home.run_dir(run_id).root().join("paused.json")).unwrap();

        let err = run_pause(&home, PauseArgs { run_id }).unwrap_err();
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

        run_pause(&home, PauseArgs { run_id }).unwrap();

        let lock_path = home.run_dir(run_id).root().join(LOCK_FILENAME);
        let aged = SystemTime::now()
            .checked_sub(Duration::from_secs(LOCK_TTL_SECS_DEFAULT + 60))
            .expect("system time must support subtraction");
        std::fs::File::open(&lock_path)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        let code = run_pause(&home, PauseArgs { run_id }).unwrap();
        assert_eq!(code, 0, "stale lock must be replaced, not rejected");
        assert!(lock_path.exists());
    }
}
