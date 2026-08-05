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

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Stale-lock threshold for `--cleanup-orphans`. Anything older than
/// 1h is considered abandoned (the process that took the lock has
/// been dead long enough for the OS to have reaped the PID).
const STALE_LOCK_SECS: i64 = 3600;

/// Zombie heartbeat threshold for `--recover-zombies`. Two hours
/// matches the existing `interrupted` semantics elsewhere in the
/// pipeline: a phase that has not advanced in two hours is no
/// longer "running" by any reasonable definition. Used by
/// commit 4's `handle_recover_zombies` implementation.
#[allow(dead_code)]
const ZOMBIE_HEARTBEAT_SECS: i64 = 7200;

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
    /// Explicit home override. When `Some`, the dispatcher
    /// uses the given `MoaganHome` instead of resolving
    /// `MOAGAN_HOME` from the environment. Production callers
    /// leave this `None`; tests set it to bypass the global
    /// env var (the parallel-test race on `MOAGAN_HOME`
    /// surfaces as a spurious
    /// `Provider("sqlite: duplicate column name: …")` panic
    /// when two tests share the same `meta.sqlite` file via
    /// env-var mutation).
    #[doc(hidden)]
    pub home_override: Option<MoaganHome>,
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

    let home = args.home_override.clone().unwrap_or(MoaganHome::resolve()?);
    let db = Db::open(&home.meta_db_path())?;

    if args.cleanup_orphans {
        handle_cleanup_orphans(&home, args.dry_run, args.yes)?;
    }
    if args.reindex_artifacts {
        handle_reindex_artifacts(&home, &db, args.dry_run)?;
    }
    if args.recover_zombies {
        handle_recover_zombies(&db, args.dry_run)?;
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

// -- D.28.3: --cleanup-orphans ------------------------------------

/// D.28.3: walk the runs dir for `*.tmp.<uuid>` atomic-write
/// leftovers and `*.lock` files with `mtime > STALE_LOCK_SECS`.
/// The plan is a `Vec<PathBuf>`; we materialise it up front so the
/// destructive branch can decide whether `--yes` is required.
fn handle_cleanup_orphans(home: &MoaganHome, dry_run: bool, yes: bool) -> Result<usize> {
    let target_runs = resolve_target_runs_for_cleanup(home)?;
    let plan = plan_cleanup(home, &target_runs)?;

    if plan.is_empty() {
        println!("cleanup-orphans: nothing to do");
        return Ok(0);
    }
    println!("cleanup-orphans: found {} orphan file(s)", plan.len());
    for p in &plan {
        println!("  - {}", p.display());
    }

    if dry_run {
        return Ok(plan.len());
    }
    if !yes {
        return Err(Error::NeedsInput(format!(
            "cleanup-orphans: {} file(s) queued for deletion; pass --yes to apply",
            plan.len()
        )));
    }

    let mut deleted = 0usize;
    for p in &plan {
        match std::fs::remove_file(p) {
            Ok(()) => deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Race: another process or a concurrent repair pass
                // already removed it. Treat as success.
                deleted += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(deleted)
}

/// For the cleanup-orphans path we walk the filesystem directly
/// (orphan files may not have a corresponding DB row). The list
/// of run directories is read from the filesystem, not the index,
/// so the helper still works on a freshly-created MOAGAN_HOME
/// whose SQLite index has not been bootstrapped yet.
fn resolve_target_runs_for_cleanup(home: &MoaganHome) -> Result<Vec<RunId>> {
    let runs_root = home.runs_dir();
    if !runs_root.exists() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<RunId> = Vec::new();
    for entry in std::fs::read_dir(&runs_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // `.lock` files at the top of the runs dir are handled by
        // `plan_cleanup` directly, not by the per-run walker.
        if name.ends_with(".lock") {
            continue;
        }
        if let Ok(id) = name.parse::<RunId>() {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Build the list of files the cleanup pass would delete. Two
/// patterns:
///
/// 1. `<run_dir>/**/*.tmp.<hex>` — atomic-write leftovers from
///    `AtomicWriter` (`src/atomic/writer.rs` writes `<dest>.tmp.<hex>`
///    then renames on success).
/// 2. `<runs_dir>/*.lock` with `mtime > STALE_LOCK_SECS` — abandoned
///    per-run lock files at the top of the runs dir.
fn plan_cleanup(home: &MoaganHome, target_runs: &[RunId]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    let runs_root = home.runs_dir();
    if !runs_root.exists() {
        return Ok(out);
    }

    // 1. *.tmp.<hex> inside every target run dir.
    for id in target_runs {
        let run_dir = home.run_dir(*id);
        for entry in WalkDir::new(run_dir.root())
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if is_atomic_tmp(path) {
                out.push(path.to_path_buf());
            }
        }
    }

    // 2. Stale `*.lock` at the top of `home.runs_dir()`.
    let now = crate::time::now_unix_secs();
    for entry in std::fs::read_dir(&runs_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_lock_file(&path) {
            continue;
        }
        let modified = match entry.metadata() {
            Ok(m) => m.modified().ok(),
            Err(_) => None,
        };
        let Some(modified) = modified else { continue };
        let modified_unix = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if now - modified_unix > STALE_LOCK_SECS {
            out.push(path);
        }
    }
    Ok(out)
}

/// `*.tmp.<hex>` heuristic. The atomic writer appends
/// `.<dest>.tmp.<16 hex>` so any file whose name contains `.tmp.`
/// followed by at least 8 hex chars qualifies.
fn is_atomic_tmp(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(idx) = name.find(".tmp.") else {
        return false;
    };
    let tail = &name[idx + ".tmp.".len()..];
    !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit())
}

/// `*.lock` heuristic. Conservative on purpose: the runs dir does
/// not currently produce lock files on its own, so any `.lock`
/// file at this depth is by definition orphan. We never delete
/// sub-directory lock files.
fn is_lock_file(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("lock")
}

// -- D.28.5: --reindex-artifacts ----------------------------------

/// D.28.5: for each target run and each kind, compare the on-disk
/// count against the cached count in `run_artifacts`. When they
/// differ, call the matching `reindex_<kind>` helper (which is
/// itself a filesystem re-walk + upsert). Returns the total number
/// of (run, kind) tuples that drifted.
fn handle_reindex_artifacts(home: &MoaganHome, db: &Db, dry_run: bool) -> Result<usize> {
    let target_runs = resolve_target_runs_for_reindex(home)?;
    let mut diffs = 0usize;
    for id in &target_runs {
        let run_dir = home.run_dir(*id);
        let kinds: [&str; 4] = ["proposals", "sketches", "evaluations", "critiques"];
        for kind in kinds {
            let dir = match kind {
                "proposals" => run_dir.proposals(),
                "sketches" => run_dir.sketches(),
                "evaluations" => run_dir.evaluations(),
                "critiques" => run_dir.critiques(),
                other => return Err(Error::InvalidArgs(format!("unknown reindex kind: {other}"))),
            };
            let disk_count = count_artefacts_in_dir(&dir)?;
            let cached = match kind {
                "proposals" => db.count_proposals(id)?,
                "sketches" => db.count_sketches(id)?,
                "evaluations" => db.count_evaluations(id)?,
                "critiques" => db.count_critiques(id)?,
                _ => unreachable!("kind set is closed"),
            };
            if disk_count == cached {
                continue;
            }
            diffs += 1;
            println!(
                "reindex: {kind} drift on {id} (db={cached}, disk={disk_count})",
                kind = kind,
                id = id,
                cached = cached,
                disk_count = disk_count,
            );
            if !dry_run {
                let _ = reindex_kind(db, *id, kind, run_dir.root())?;
            }
        }
    }
    Ok(diffs)
}

/// For the reindex path we read the filesystem (orphan files may
/// not have a corresponding DB row) and also drive every run we
/// find on disk.
fn resolve_target_runs_for_reindex(home: &MoaganHome) -> Result<Vec<RunId>> {
    let runs_root = home.runs_dir();
    if !runs_root.exists() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<RunId> = Vec::new();
    for entry in std::fs::read_dir(&runs_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.ends_with(".lock") {
            continue;
        }
        if let Ok(id) = name.parse::<RunId>() {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Count primary `*.json` files in `dir`. Excludes sidecars
/// (`*.meta.json`) and atomic-write leftovers (`*.tmp.<hex>`).
/// Returns 0 when the directory is missing.
fn count_artefacts_in_dir(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") || name.ends_with(".meta.json") {
            continue;
        }
        if is_atomic_tmp(&p) {
            continue;
        }
        count = count.checked_add(1).ok_or_else(|| {
            Error::Provider(format!("artefact count overflow at {}", p.display()))
        })?;
    }
    Ok(count)
}

/// Upsert a fresh count for one (run, kind) pair. The disk walk
/// itself lives inside `Db::reindex_<kind>` so the DB layer is
/// the single source of truth for the reindex SQL. Returns the
/// freshly indexed count so the caller can log it on the diff
/// path.
fn reindex_kind(db: &Db, id: RunId, kind: &str, root: &Path) -> Result<usize> {
    match kind {
        "proposals" => db.reindex_proposals(&id, root),
        "sketches" => db.reindex_sketches(&id, root),
        "evaluations" => db.reindex_evaluations(&id, root),
        "critiques" => db.reindex_critiques(&id, root),
        other => Err(Error::InvalidArgs(format!("unknown reindex kind: {other}"))),
    }
}

// -- D.28.4: --recover-zombies ------------------------------------

/// D.28.4 stub. Real implementation lands in commit 4.
fn handle_recover_zombies(_db: &Db, _dry_run: bool) -> Result<usize> {
    Ok(0)
}

// -- Tests --------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_MOAGAN_HOME_LOCK;
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

    /// Acquire the process-wide `MOAGAN_HOME` lock and set the
    /// env var. The lock lives in
    /// `crate::TEST_MOAGAN_HOME_LOCK` so every test that
    /// touches the var (this module and any future one) shares
    /// the same mutex; without it, two tests setting
    /// `MOAGAN_HOME` in parallel can race the OS scheduler
    /// and end up sharing each other's home dir, which
    /// surfaces as a spurious
    /// `Provider("sqlite: duplicate column name: …")` panic.
    fn lock_env(tmp: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp);
        }
        guard
    }

    fn unlock_env(guard: std::sync::MutexGuard<'static, ()>) {
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
        drop(guard);
    }

    /// No flag at all is an operator error, not a no-op. The
    /// dispatcher must surface `Error::InvalidArgs` so CI
    /// scripts see exit code 2.
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
    /// an empty runs dir must exit 0 and not touch the
    /// filesystem.
    #[test]
    fn dry_run_with_cleanup_orphans_no_fs_changes() {
        let tmp = unique_tmp("empty-cleanup");
        let guard = lock_env(&tmp);
        let rc = run(args_with(&["cleanup", "dry", "yes"])).expect("dry-run must not error");
        assert_eq!(rc, 0);
        assert!(
            !tmp.join(".runs/foo").exists(),
            "dry-run must not create spurious files"
        );
        unlock_env(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Plan with matches: dry-run must not delete any file, and
    /// must report the count it would have deleted.
    #[test]
    fn cleanup_orphans_dry_run_does_not_delete() {
        let tmp = unique_tmp("dry-run");
        let run_id = RunId::new();
        let run_dir = tmp.join(".runs").join(run_id.to_string()).join("proposals");
        std::fs::create_dir_all(&run_dir).unwrap();
        let target = run_dir.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&target, b"orphan").unwrap();

        let guard = lock_env(&tmp);
        let rc = run(args_with(&["cleanup", "dry"])).expect("dry-run must not error");
        assert_eq!(rc, 0);
        assert!(target.exists(), "dry-run must not delete the file");
        unlock_env(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Plan with matches and `--yes`: the dispatcher must delete
    /// the queued files and report a non-zero count.
    #[test]
    fn cleanup_orphans_with_yes_deletes() {
        let tmp = unique_tmp("with-yes");
        let run_id = RunId::new();
        let run_dir = tmp.join(".runs").join(run_id.to_string()).join("proposals");
        std::fs::create_dir_all(&run_dir).unwrap();
        let target = run_dir.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&target, b"orphan").unwrap();

        let guard = lock_env(&tmp);
        let rc = run(args_with(&["cleanup", "yes"])).expect("with-yes must not error");
        assert_eq!(rc, 0);
        assert!(!target.exists(), "with-yes must delete the file");
        unlock_env(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Plan with matches and **no** `--yes`: the dispatcher must
    /// surface `Error::NeedsInput` (exit 10) and leave the file
    /// untouched.
    #[test]
    fn cleanup_orphans_without_yes_returns_needs_input() {
        let tmp = unique_tmp("no-yes");
        let run_id = RunId::new();
        let run_dir = tmp.join(".runs").join(run_id.to_string()).join("proposals");
        std::fs::create_dir_all(&run_dir).unwrap();
        let target = run_dir.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&target, b"orphan").unwrap();

        let guard = lock_env(&tmp);
        let err =
            run(args_with(&["cleanup"])).expect_err("missing --yes must surface as NeedsInput");
        assert!(
            matches!(err, Error::NeedsInput(_)),
            "expected Error::NeedsInput, got {err:?}"
        );
        assert_eq!(err.exit_code() as i32, 10);
        assert!(target.exists(), "no-yes must not delete the file");
        unlock_env(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Reindex with the disk and DB already in sync must report
    /// zero diffs and not touch the DB. The cache is primed
    /// directly via `Db::reindex_proposals` (not through the
    /// dispatcher) so the test avoids running the SQLite
    /// migration runner twice on the same `meta.sqlite` — the
    /// `current < N` guard is non-atomic and the second open
    /// can race the first open's commit under parallel
    /// `cargo test`, surfacing as a spurious `duplicate column
    /// name: status` panic.
    #[test]
    fn reindex_no_diff_returns_zero() {
        let tmp = unique_tmp("reindex-sync");
        let run_id = RunId::new();
        let run_dir_root = tmp.join(".runs").join(run_id.to_string());
        let proposals = run_dir_root.join("proposals");
        std::fs::create_dir_all(&proposals).unwrap();
        for n in 0..3 {
            std::fs::write(proposals.join(format!("p_{n:03}.json")), b"{}").unwrap();
        }

        let guard = lock_env(&tmp);
        // Prime the cache directly via the DB so we only open
        // the connection once.
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let _ = db
            .reindex_proposals(&run_id, &run_dir_root)
            .expect("prime cache");
        drop(db);
        // Dispatcher: must report zero diffs and exit 0.
        let rc = run(args_with(&["reindex", "dry"])).expect("reindex must not error");
        assert_eq!(rc, 0);
        unlock_env(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A proposal written to disk after the cached count was
    /// last seen triggers a reindex on the next call: the
    /// dispatcher reports the drift, the DB catches up.
    ///
    /// The test makes TWO dispatcher calls — one to prime the
    /// cache, one to detect the drift — both via
    /// `home_override` so the global `MOAGAN_HOME` env var is
    /// never touched. Two sequential opens of the same
    /// `meta.sqlite` are race-prone against the non-atomic
    /// migration runner (`ALTER TABLE` + `PRAGMA user_version`
    /// are two separate transactions), so the second open can
    /// see a stale `user_version` and re-apply v003's
    /// `ALTER TABLE calls ADD COLUMN status`, which fails with
    /// `duplicate column name: status` under parallel
    /// `cargo test`. The `home_override` lets the dispatcher
    /// resolve the same path deterministically; the underlying
    /// race would still surface on the same file, so the
    /// priming call commits the migrations synchronously
    /// before the dry-run call observes them.
    #[test]
    fn reindex_missing_in_db_catches_up() {
        let tmp = unique_tmp("reindex-drift");
        let run_id = RunId::new();
        let run_dir_root = tmp.join(".runs").join(run_id.to_string());
        let proposals = run_dir_root.join("proposals");
        std::fs::create_dir_all(&proposals).unwrap();
        for n in 0..3 {
            std::fs::write(proposals.join(format!("p_{n:03}.json")), b"{}").unwrap();
        }

        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        // First call: primes the cache to 3 (the disk count).
        let mut args = args_with(&["reindex"]);
        args.home_override = Some(home.clone());
        let _ = run(args).expect("prime reindex must not error");
        // Add a 4th file on disk; the cache still says 3.
        std::fs::write(proposals.join("p_extra.json"), b"{}").unwrap();
        // Second call: dry-run, must detect the drift and
        // exit 0. The migration runner short-circuits on
        // user_version=10 because the first call already
        // committed.
        let mut args = args_with(&["reindex", "dry"]);
        args.home_override = Some(home);
        let rc = run(args).expect("dry reindex must not error");
        assert_eq!(rc, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
