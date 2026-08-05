//! `moagan repair` — Track D.2 (specs D.14.3 + D.28.1/3/4/5).
//!
//! Reconciles the filesystem (canonical) against the SQLite index
//! without ever modifying artefact content. The three orthogonal
//! operations are gated by their own flags so an operator can pick
//! exactly the safety level they need:
//!
//! - `--cleanup-orphans` (D.28.3): removes `*.tmp.<uuid>` atomic
//!   write leftovers inside `home.runs_dir()/<id>/` and stale
//!   `*.lock` files at the top of `home.runs_dir()`. Destructive,
//!   so `--yes` is required when there is at least one match.
//! - `--reindex-artifacts` (D.28.5): reconciles filesystem vs the
//!   `run_artifacts` table for the four primary artefact kinds
//!   (`proposals`, `sketches`, `evaluations`, `critiques`).
//! - `--recover-zombies` (D.28.4): marks runs whose
//!   `status = 'running'` and `updated_unix < now - 7200s` as
//!   `interrupted` and emits an outbox event per recovery.
//!
//! Common knobs:
//! - `--run <id>`: scope every operation to a single run. Defaults
//!   to all known runs (DB list, ordered by recency).
//! - `--dry-run`: print the plan without touching disk or SQLite.
//! - `--yes`: confirm destructive operations when there is a
//!   non-empty plan.
//!
//! Exit codes follow the existing CLI contract (T01-06 §12.3 +
//! D.14.3):
//!   0 — operation ran (or was a no-op).
//!   2 — `Error::InvalidArgs` (no flag passed, malformed run id).
//!  10 — `Error::NeedsInput` (destructive plan, `--yes` missing).

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// CLI arguments for `moagan repair`. The dispatcher requires at
/// least one of the operation flags; all other flags tweak the
/// behaviour of the chosen operations.
#[derive(Debug, Clone, Default)]
pub struct RepairArgs {
    /// D.28.3: clean `*.tmp.<uuid>` + stale `*.lock` files.
    pub cleanup_orphans: bool,
    /// D.28.5: reconcile the per-kind artefact count cache.
    pub reindex_artifacts: bool,
    /// D.28.4: mark `running`-but-stale runs as `interrupted`.
    pub recover_zombies: bool,
    /// Required to actually apply destructive changes; the
    /// dispatcher returns `Error::NeedsInput` (exit 10) when a
    /// non-empty destructive plan runs without this flag.
    pub yes: bool,
    /// Optional single-run scope. `None` means "every run known
    /// to the SQLite index".
    pub run: Option<RunId>,
    /// Print the plan instead of applying it. Combined with
    /// `--yes` to express "preview, then apply" intent; without
    /// `--yes` the dry-run is a no-op for the destructive paths.
    pub dry_run: bool,
}

/// Top-level dispatch. Returns the process exit code so the central
/// CLI dispatcher can map `Error` variants onto `ExitCode` (T01-06
/// §12.3).
///
/// The function never silently no-ops an empty plan: when no
/// operation flag is passed it returns `Error::InvalidArgs` so CI
/// scripts can detect the "forgot to add a flag" failure mode.
pub fn run(args: RepairArgs) -> Result<i32> {
    if !args.cleanup_orphans && !args.reindex_artifacts && !args.recover_zombies {
        return Err(Error::InvalidArgs(
            "moagan repair requires at least one of \
             --cleanup-orphans, --reindex-artifacts, --recover-zombies"
                .into(),
        ));
    }

    // Resolve the home + DB up front so the operation handlers
    // never have to repeat the work. The handlers themselves
    // take a `&MoaganHome` and `&Db` for symmetry with the rest
    // of the CLI surface.
    let _home = MoaganHome::resolve()?;
    let _db = Db::open(&_home.meta_db_path())?;

    // Stubs — commit 2/3/4 swap each branch for its real handler.
    if args.cleanup_orphans {
        handle_cleanup_orphans(&_home, args.dry_run, args.yes)?;
    }
    if args.reindex_artifacts {
        handle_reindex_artifacts(&_home, &_db, args.dry_run)?;
    }
    if args.recover_zombies {
        handle_recover_zombies(&_db, args.dry_run)?;
    }

    println!(
        "repair ({}): cleanup={} reindex={} zombies={}",
        if args.dry_run { "dry-run" } else { "applied" },
        args.cleanup_orphans,
        args.reindex_artifacts,
        args.recover_zombies,
    );
    Ok(0)
}

/// D.28.3 stub. Real implementation lands in commit 2.
fn handle_cleanup_orphans(_home: &MoaganHome, _dry_run: bool, _yes: bool) -> Result<usize> {
    Ok(0)
}

/// D.28.5 stub. Real implementation lands in commit 3.
fn handle_reindex_artifacts(_home: &MoaganHome, _db: &Db, _dry_run: bool) -> Result<usize> {
    Ok(0)
}

/// D.28.4 stub. Real implementation lands in commit 4.
fn handle_recover_zombies(_db: &Db, _dry_run: bool) -> Result<usize> {
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_tmp(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("moagan-repair-{pid}-{n}-{label}"));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir
    }

    fn args_with(flags: &[&str]) -> RepairArgs {
        let mut a = RepairArgs::default();
        for f in flags {
            match *f {
                "cleanup" => a.cleanup_orphans = true,
                "reindex" => a.reindex_artifacts = true,
                "zombies" => a.recover_zombies = true,
                "yes" => a.yes = true,
                "dry" => a.dry_run = true,
                other => panic!("unknown flag: {other}"),
            }
        }
        a
    }

    /// No flag at all is an operator error, not a no-op. The
    /// dispatcher must surface `Error::InvalidArgs` so CI scripts
    /// see exit code 2.
    #[test]
    fn no_flags_returns_invalid_args() {
        let args = args_with(&[]);
        let err = run(args).expect_err("no flags must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "expected Error::InvalidArgs, got {err:?}"
        );
        assert_eq!(err.exit_code() as i32, 2);
    }

    /// `moagan repair --cleanup-orphans --dry-run --yes` against
    /// an empty runs dir must exit 0. The stub returns Ok(0)
    /// without touching the filesystem; the test confirms the
    /// glue wires through without panicking and that the
    /// resulting `MOAGAN_HOME` is left untouched.
    #[test]
    fn dry_run_with_cleanup_orphans_no_fs_changes() {
        let tmp = unique_tmp("empty-cleanup");
        unsafe {
            std::env::set_var("MOAGAN_HOME", &tmp);
        }
        let args = args_with(&["cleanup", "dry", "yes"]);
        let rc = run(args).expect("dry-run must not error");
        assert_eq!(rc, 0);
        assert!(
            !tmp.join(".runs/foo").exists(),
            "dry-run must not create spurious files"
        );
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
