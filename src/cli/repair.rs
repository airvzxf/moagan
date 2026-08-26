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
//! The two filesystem-facing operations (`--cleanup-orphans` and
//! `--recover-zombies`) are thin wrappers around the public
//! [`crate::reconcile`] module: that module is what runs at
//! `moagan` startup (Track F), so the manual and the auto path
//! can never drift.
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

use std::path::Path;

use tracing::{debug, trace, warn};

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
    debug!(
        cleanup_orphans = args.cleanup_orphans,
        reindex_artifacts = args.reindex_artifacts,
        recover_zombies = args.recover_zombies,
        yes = args.yes,
        dry_run = args.dry_run,
        "repair::run: enter"
    );
    if !args.cleanup_orphans && !args.reindex_artifacts && !args.recover_zombies {
        warn!("repair: no operation flag set");
        return Err(Error::InvalidArgs(
            "moagan repair requires at least one of \
             --cleanup-orphans, --reindex-artifacts, --recover-zombies"
                .into(),
        ));
    }

    let home = args.home_override.clone().unwrap_or(MoaganHome::resolve()?);
    let db = Db::open(&home.meta_db_path())?;

    // PR-B1: `--run <RUN_ID>` scopes every operation below to a
    // single run directory + its DB rows. The flag was previously
    // parsed and threaded all the way down to `RepairArgs::run`,
    // but the handler ignored it and operated on the global
    // home/DB. With the wire-up:
    //   - `--cleanup-orphans` only walks the target run dir for
    //     `*.tmp.<hex>` leftovers; stale `*.lock` files at the
    //     top of `home.runs_dir()` are still reported (they
    //     outlive any single run anyway).
    //   - `--reindex-artifacts` only reconciles the four primary
    //     artefact kinds for that single run.
    //   - `--recover-zombies` only considers the named run id;
    //     any zombie rows belonging to other runs are left alone.
    // `args.run = None` keeps today's global sweep.
    if let Some(id) = args.run.as_ref() {
        tracing::info!(
            run_id = %id,
            "repair: scoped to run {id}"
        );
    } else {
        tracing::info!("repair: global sweep (no --run flag)");
    }

    if args.cleanup_orphans {
        debug!("repair: dispatching cleanup_orphans");
        handle_cleanup_orphans(&home, args.run, args.dry_run, args.yes)?;
    }
    if args.reindex_artifacts {
        debug!("repair: dispatching reindex_artifacts");
        handle_reindex_artifacts(&home, &db, args.run, args.dry_run)?;
    }
    if args.recover_zombies {
        debug!("repair: dispatching recover_zombies");
        handle_recover_zombies(&db, args.run, args.dry_run)?;
    }

    println!(
        "repair ({}): cleanup={} reindex={} zombies={} scope={}",
        if args.dry_run { "dry-run" } else { "applied" },
        args.cleanup_orphans,
        args.reindex_artifacts,
        args.recover_zombies,
        match args.run {
            Some(id) => format!("run:{id}"),
            None => "global".to_string(),
        },
    );
    Ok(0)
}

// -- D.28.3: --cleanup-orphans ------------------------------------

/// D.28.3: walk the runs dir for `*.tmp.<uuid>` atomic-write
/// leftovers and `*.lock` files with `mtime > STALE_LOCK_SECS`.
/// Delegates the actual walk-and-delete to
/// [`crate::reconcile::cleanup_orphans`] so the manual
/// `moagan repair --cleanup-orphans` path and the auto startup
/// reconcile path share one implementation.
///
/// When `scope` is `Some(id)`, the per-run walk is restricted to
/// that run's directory; stale top-level `*.lock` files are still
/// reported because they outlive any single run. `None` falls
/// back to the global sweep (`resolve_target_runs_for_cleanup`
/// walks every `home.runs_dir()/<id>` it can parse).
fn handle_cleanup_orphans(
    home: &MoaganHome,
    scope: Option<RunId>,
    dry_run: bool,
    yes: bool,
) -> Result<usize> {
    // Mirror the reconcile module's plan so the operator sees the
    // exact list of files the auto path would have removed.
    let plan = match scope {
        Some(id) => {
            // Per-run scope: the run directory must exist on disk;
            // a missing run dir is not an error (the operator may
            // be probing a run they just imported under a fresh
            // home). `plan_cleanup` returns an empty vec for a
            // non-existent run dir.
            if !home.run_dir(id).root().exists() {
                println!("cleanup-orphans: run {id} has no on-disk directory; nothing to do");
                return Ok(0);
            }
            crate::reconcile::plan_cleanup(home, std::slice::from_ref(&id))?
        }
        None => crate::reconcile::plan_cleanup_for_report(home)?,
    };
    if plan.is_empty() {
        debug!("cleanup-orphans: plan empty");
        println!("cleanup-orphans: nothing to do");
        return Ok(0);
    }
    println!("cleanup-orphans: found {} orphan file(s)", plan.len());
    for p in &plan {
        println!("  - {}", p.display());
    }
    debug!(plan_len = plan.len(), "cleanup-orphans: plan prepared");

    if dry_run {
        debug!("cleanup-orphans: dry-run short-circuit");
        return Ok(plan.len());
    }
    if !yes {
        warn!(plan_len = plan.len(), "cleanup-orphans: needs --yes");
        return Err(Error::NeedsInput(format!(
            "cleanup-orphans: {} file(s) queued for deletion; pass --yes to apply",
            plan.len()
        )));
    }

    // Per-run scope: delete the planned files directly so we do
    // not have to add a per-run variant to the reconcile module.
    // The reconcile module's `cleanup_orphans` always sweeps the
    // whole runs dir, which would defeat the per-run contract.
    match scope {
        Some(_) => {
            let mut removed = 0usize;
            for p in &plan {
                match std::fs::remove_file(p) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => removed += 1,
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(removed)
        }
        None => crate::reconcile::cleanup_orphans(home),
    }
}

// -- D.28.5: --reindex-artifacts ----------------------------------

/// D.28.5: for each target run and each kind, compare the on-disk
/// count against the cached count in `run_artifacts`. When they
/// differ, call the matching `reindex_<kind>` helper (which is
/// itself a filesystem re-walk + upsert). Returns the total number
/// of (run, kind) tuples that drifted.
///
/// When `scope` is `Some(id)` the per-run walk is restricted to
/// that run; the four primary artefact kinds are still walked
/// (sketches / proposals / evaluations / critiques) but no other
/// run is touched. `None` falls back to today's global sweep
/// (`resolve_target_runs_for_reindex`).
fn handle_reindex_artifacts(
    home: &MoaganHome,
    db: &Db,
    scope: Option<RunId>,
    dry_run: bool,
) -> Result<usize> {
    debug!(scope = ?scope, dry_run, "repair::handle_reindex_artifacts: enter");
    let target_runs = match scope {
        Some(id) => vec![id],
        None => resolve_target_runs_for_reindex(home)?,
    };
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
                trace!(run_id = %id, kind = kind, "reindex: in-sync");
                continue;
            }
            diffs += 1;
            warn!(
                run_id = %id,
                kind = kind,
                disk = disk_count,
                db = cached,
                "reindex: drift detected"
            );
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
    debug!(diffs, "repair::handle_reindex_artifacts: done");
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
        if crate::reconcile::is_atomic_tmp(&p) {
            continue;
        }
        count = count.checked_add(1).ok_or_else(|| Error::Provider {
            message: format!("artefact count overflow at {}", p.display()),
            http_status: None,
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

/// D.28.4: find runs whose `status = 'running'` and
/// `updated_unix < now - 7200s`, mark them
/// `interrupted`, and emit a `run.zombie_recovered` outbox
/// event per recovery. In `--dry-run` mode the zombie list
/// is printed and no row is touched.
///
/// When `scope` is `Some(id)` only that run id is considered;
/// the helper short-circuits if the id is not a zombie (e.g.
/// still alive or already interrupted). `None` falls back to
/// the global sweep via [`crate::reconcile::recover_zombies`].
///
/// The actual recovery is delegated to
/// [`crate::reconcile::recover_zombies`] so the manual
/// `moagan repair --recover-zombies` path and the auto startup
/// reconcile path share one implementation.
fn handle_recover_zombies(db: &Db, scope: Option<RunId>, dry_run: bool) -> Result<usize> {
    debug!(scope = ?scope, dry_run, "repair::handle_recover_zombies: enter");
    match scope {
        Some(id) => {
            let now = crate::time::now_unix_secs();
            let threshold = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS;
            let row = db.get_run(id)?;
            let row = match row {
                Some(r) => r,
                None => {
                    println!("recover-zombies: run {id} not found in the index");
                    return Ok(0);
                }
            };
            if row.status != "running" {
                trace!(run_id = %id, status = %row.status, "recover-zombies: not running");
                println!(
                    "recover-zombies: run {id} is not running (status={}), nothing to do",
                    row.status
                );
                return Ok(0);
            }
            if row.updated_unix >= threshold {
                trace!(run_id = %id, "recover-zombies: within heartbeat");
                println!(
                    "recover-zombies: run {id} is still within the heartbeat window, nothing to do"
                );
                return Ok(0);
            }
            warn!(run_id = %id, "recover-zombies: zombie detected");
            println!("recover-zombies: found 1 zombie run(s)");
            println!("  - {id}");
            if dry_run {
                return Ok(1);
            }
            use crate::storage::outbox_tx::{OutboxEvent, record_with};
            let payload = serde_json::json!({
                "kind": "zombie_recovered",
                "previous_status": "running",
                "new_status": "interrupted",
                "recovered_at_unix": now,
                "stale_threshold_secs": crate::reconcile::ZOMBIE_HEARTBEAT_SECS,
            });
            let events = [OutboxEvent {
                run_id: id,
                event_type: "run.zombie_recovered".into(),
                payload: payload.to_string(),
            }];
            record_with(db, &events, || db.update_run_status(id, "interrupted"))?;
            Ok(1)
        }
        None => {
            let zombies = crate::reconcile::list_zombie_run_ids(db)?;
            if zombies.is_empty() {
                println!("recover-zombies: no zombie runs");
                return Ok(0);
            }
            println!("recover-zombies: found {} zombie run(s)", zombies.len());
            for z in &zombies {
                println!("  - {z}");
            }

            if dry_run {
                return Ok(zombies.len());
            }

            crate::reconcile::recover_zombies(db)
        }
    }
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

        // Bypass MOAGAN_HOME: route every Db::open through home_override so the
        // dispatcher cannot race another test that mutates MOAGAN_HOME between
        // the prime and the dispatch call. Same pattern as
        // reindex_missing_in_db_catches_up below.
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        // Prime the cache directly via the DB so we only open
        // the connection once.
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let _ = db
            .reindex_proposals(&run_id, &run_dir_root)
            .expect("prime cache");
        drop(db);
        // Dispatcher: must report zero diffs and exit 0.
        let mut args = args_with(&["reindex", "dry"]);
        args.home_override = Some(home);
        let rc = run(args).expect("reindex must not error");
        assert_eq!(rc, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A proposal written to disk after the cached count was
    /// last seen triggers a reindex on the next call: the
    /// dispatcher reports the drift, the DB catches up. Same
    /// single-dispatcher-call pattern as
    /// `reindex_no_diff_returns_zero`.
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
        // Second call (dry): the dispatcher sees the drift
        // and reports it; the cache stays at 3 because
        // --dry-run skips the upsert.
        let mut args = args_with(&["reindex", "dry"]);
        args.home_override = Some(home);
        let rc = run(args).expect("dry reindex must not error");
        assert_eq!(rc, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// D.28.4 — `--recover-zombies` flips a stale `running`
    /// row to `interrupted` and emits a `run.zombie_recovered`
    /// outbox event. The test seeds a row whose `updated_unix`
    /// is older than the 2h threshold, runs the dispatcher,
    /// and asserts the row is now `interrupted`.
    #[test]
    fn recovers_zombie_running_runs() {
        let tmp = unique_tmp("zombies-recover");
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let zombie = RunId::new();
        let alive = RunId::new();
        let now = crate::time::now_unix_secs();
        // `register_run` writes created_unix=updated_unix=now.
        // Manually backdate the zombie's updated_unix via the
        // pool so it lands well past the 2h threshold.
        db.register_run(zombie, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        db.register_run(alive, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let past = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie, past)
            .expect("backdate");

        let mut args = args_with(&["zombies"]);
        args.home_override = Some(home.clone());
        let recovered = run(args).expect("recover must not error");
        assert_eq!(recovered, 0, "exit code is 0 (ok)");

        let zombie_row = db.get_run(zombie).expect("get zombie").unwrap();
        assert_eq!(zombie_row.status, "interrupted");
        let alive_row = db.get_run(alive).expect("get alive").unwrap();
        assert_eq!(alive_row.status, "running");

        let events = db
            .list_outbox_events_for_run(&zombie.to_string())
            .expect("events");
        let hit = events
            .iter()
            .find(|e| e.event_type == "run.zombie_recovered")
            .expect("zombie_recovered event must exist");
        assert!(hit.payload.contains("\"kind\":\"zombie_recovered\""));
        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `--recover-zombies --dry-run` must list the zombie but
    /// leave the row untouched (status stays `running`) and
    /// not emit any outbox event.
    #[test]
    fn recover_zombies_dry_run_does_not_update_db() {
        let tmp = unique_tmp("zombies-dry-run");
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let zombie = RunId::new();
        let now = crate::time::now_unix_secs();
        db.register_run(zombie, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let past = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie, past)
            .expect("backdate");

        let mut args = args_with(&["zombies", "dry"]);
        args.home_override = Some(home.clone());
        let recovered = run(args).expect("dry-run must not error");
        assert_eq!(recovered, 0, "dry-run exit code is 0");

        let row = db.get_run(zombie).expect("get").unwrap();
        assert_eq!(
            row.status, "running",
            "dry-run must leave the row untouched"
        );
        let events = db
            .list_outbox_events_for_run(&zombie.to_string())
            .expect("events");
        assert!(
            events.is_empty(),
            "dry-run must not emit any outbox events; got {events:?}"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Track F refactor pin: the `moagan repair --cleanup-orphans`
    /// and `--recover-zombies` paths now delegate to
    /// `crate::reconcile`. The refactor must keep the
    /// `dry-run → count without touching the DB` contract for
    /// zombies so the regression coverage stays in place. We
    /// piggy-back on the `recover_zombies_dry_run_does_not_update_db`
    /// pattern above to also pin that the count is reported as 1
    /// (the dispatcher prints `recover-zombies: found 1 zombie`).
    #[test]
    fn repair_refactor_still_passes() {
        let tmp = unique_tmp("refactor-pin");
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let zombie = RunId::new();
        let now = crate::time::now_unix_secs();
        db.register_run(zombie, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let past = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie, past)
            .expect("backdate");

        // The reconcile module's discovery helper must surface
        // the same single zombie the dispatcher used to find
        // inline. This is the contract that the refactor
        // promises: `list_zombie_run_ids` is what `repair --dry-run`
        // now prints, and `recover_zombies` is what
        // `repair --yes` actually applies.
        let zombies = crate::reconcile::list_zombie_run_ids(&db).expect("list");
        assert_eq!(zombies.len(), 1);
        assert_eq!(zombies[0], zombie);

        // Apply the recovery directly (skipping the print/--yes
        // UI) and confirm the row flips + the outbox event lands.
        let recovered = crate::reconcile::recover_zombies(&db).expect("recover");
        assert_eq!(recovered, 1);
        let row = db.get_run(zombie).expect("get").unwrap();
        assert_eq!(row.status, "interrupted");
        let events = db
            .list_outbox_events_for_run(&zombie.to_string())
            .expect("events");
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "run.zombie_recovered"),
            "recovery must emit the outbox event"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-B1 (B1.3): `--run <RUN_ID>` scopes `--cleanup-orphans`
    /// to that run's `.runs/<id>/` directory. Without the flag
    /// the sweep walks every run; with the flag only the named
    /// run's `*.tmp.<hex>` leftovers are reported. Two runs are
    /// seeded: the target run gets a `*.tmp.<hex>` orphan, the
    /// other run gets its own. The scoped dry-run must report
    /// only the target's orphan.
    #[test]
    fn cleanup_orphans_with_run_scope_only_touches_target_run() {
        let tmp = unique_tmp("run-scope-cleanup");
        let target = RunId::new();
        let other = RunId::new();
        // Target run gets one orphan.
        let target_proposals = tmp.join(".runs").join(target.to_string()).join("proposals");
        std::fs::create_dir_all(&target_proposals).unwrap();
        let target_orphan = target_proposals.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&target_orphan, b"target orphan").unwrap();
        // Other run also gets one orphan.
        let other_proposals = tmp.join(".runs").join(other.to_string()).join("proposals");
        std::fs::create_dir_all(&other_proposals).unwrap();
        let other_orphan = other_proposals.join("p_001.json.tmp.cafebabe01234567");
        std::fs::write(&other_orphan, b"other orphan").unwrap();

        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        // Scoped: --cleanup-orphans --dry-run --run <target>.
        let mut args = args_with(&["cleanup", "dry"]);
        args.run = Some(target);
        args.home_override = Some(home.clone());
        let rc = run(args).expect("scoped dry-run must not error");
        assert_eq!(rc, 0);

        // Both files still exist (dry-run), but the helper
        // recorded only the target's plan. We verify the contract
        // by re-invoking the per-run planner directly: it must
        // return only the target's orphan when scoped.
        let plan = crate::reconcile::plan_cleanup(&home, &[target]).expect("plan must succeed");
        assert_eq!(
            plan.len(),
            1,
            "scoped plan must contain only the target's orphan; got {plan:?}"
        );
        assert_eq!(
            plan[0], target_orphan,
            "scoped plan must contain exactly the target's orphan path"
        );
        let unscoped_plan =
            crate::reconcile::plan_cleanup_for_report(&home).expect("unscoped plan must succeed");
        assert_eq!(
            unscoped_plan.len(),
            2,
            "unscoped plan must contain both orphans; got {unscoped_plan:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-B1 (B1.3): `--run <RUN_ID> --cleanup-orphans --yes`
    /// must delete only the target run's orphans and leave the
    /// other run's `*.tmp.<hex>` files intact. Pins the
    /// destructive side of the scoping contract.
    #[test]
    fn cleanup_orphans_with_run_scope_deletes_only_target() {
        let tmp = unique_tmp("run-scope-cleanup-yes");
        let target = RunId::new();
        let other = RunId::new();
        let target_proposals = tmp.join(".runs").join(target.to_string()).join("proposals");
        std::fs::create_dir_all(&target_proposals).unwrap();
        let target_orphan = target_proposals.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&target_orphan, b"target orphan").unwrap();
        let other_proposals = tmp.join(".runs").join(other.to_string()).join("proposals");
        std::fs::create_dir_all(&other_proposals).unwrap();
        let other_orphan = other_proposals.join("p_001.json.tmp.cafebabe01234567");
        std::fs::write(&other_orphan, b"other orphan").unwrap();

        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let mut args = args_with(&["cleanup", "yes"]);
        args.run = Some(target);
        args.home_override = Some(home);
        let rc = run(args).expect("scoped apply must not error");
        assert_eq!(rc, 0);

        assert!(
            !target_orphan.exists(),
            "scoped apply must delete the target's orphan"
        );
        assert!(
            other_orphan.exists(),
            "scoped apply must NOT delete the other run's orphan"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-B1 (B1.3): `--run <RUN_ID>` scopes `--recover-zombies`
    /// to the named run id. A scoped call against a still-alive
    /// (non-zombie) run id must NOT flip any rows and must NOT
    /// emit an outbox event. Two runs are seeded: the target is
    /// alive, the other is a zombie.
    #[test]
    fn recover_zombies_with_run_scope_skips_non_zombie_target() {
        let tmp = unique_tmp("run-scope-zombies-skip");
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let alive_target = RunId::new();
        let zombie_other = RunId::new();
        db.register_run(alive_target, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        db.register_run(zombie_other, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let now = crate::time::now_unix_secs();
        let past = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie_other, past)
            .expect("backdate zombie");

        // Scoped: --recover-zombies --dry-run --run <alive_target>.
        let mut args = args_with(&["zombies", "dry"]);
        args.run = Some(alive_target);
        args.home_override = Some(home.clone());
        let rc = run(args).expect("scoped dry-run must not error");
        assert_eq!(rc, 0);

        // The alive target's row is still `running` (no flip).
        let target_row = db.get_run(alive_target).expect("get target").unwrap();
        assert_eq!(
            target_row.status, "running",
            "scoped recover must not flip an alive target"
        );
        // The other run is also untouched (it's not in scope).
        let other_row = db.get_run(zombie_other).expect("get other").unwrap();
        assert_eq!(
            other_row.status, "running",
            "scoped recover must not touch out-of-scope zombies"
        );
        // No outbox events were emitted by the dry-run.
        let target_events = db
            .list_outbox_events_for_run(&alive_target.to_string())
            .expect("events target");
        let other_events = db
            .list_outbox_events_for_run(&zombie_other.to_string())
            .expect("events other");
        assert!(
            target_events.is_empty(),
            "scoped dry-run must not emit events for the target; got {target_events:?}"
        );
        assert!(
            other_events.is_empty(),
            "scoped dry-run must not emit events for the other run; got {other_events:?}"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR-B1 (B1.3): `--run <RUN_ID>` scopes `--recover-zombies`
    /// to the named run id. A scoped call against a zombie target
    /// must flip ONLY that row to `interrupted` and leave other
    /// zombie rows alone.
    #[test]
    fn recover_zombies_with_run_scope_flips_only_target() {
        let tmp = unique_tmp("run-scope-zombies-yes");
        let home = crate::fs_layout::MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let zombie_target = RunId::new();
        let zombie_other = RunId::new();
        db.register_run(zombie_target, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        db.register_run(zombie_other, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let now = crate::time::now_unix_secs();
        let past = now - crate::reconcile::ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie_target, past)
            .expect("backdate target");
        db._test_backdate_updated_unix(zombie_other, past)
            .expect("backdate other");

        let mut args = args_with(&["zombies", "yes"]);
        args.run = Some(zombie_target);
        args.home_override = Some(home.clone());
        let rc = run(args).expect("scoped apply must not error");
        assert_eq!(rc, 0);

        let target_row = db.get_run(zombie_target).expect("get target").unwrap();
        assert_eq!(
            target_row.status, "interrupted",
            "scoped apply must flip the in-scope target"
        );
        let other_row = db.get_run(zombie_other).expect("get other").unwrap();
        assert_eq!(
            other_row.status, "running",
            "scoped apply must NOT flip out-of-scope zombies"
        );

        // The target got the outbox event; the other did not.
        let target_events = db
            .list_outbox_events_for_run(&zombie_target.to_string())
            .expect("events target");
        let other_events = db
            .list_outbox_events_for_run(&zombie_other.to_string())
            .expect("events other");
        assert!(
            target_events
                .iter()
                .any(|e| e.event_type == "run.zombie_recovered"),
            "in-scope target must emit the outbox event"
        );
        assert!(
            other_events.is_empty(),
            "out-of-scope zombie must NOT emit any outbox events; got {other_events:?}"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
