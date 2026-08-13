//! `src/reconcile/mod.rs` — startup reconcile (Track F, specs D.28.3 + D.28.4).
//!
//! Reconciles the canonical filesystem (`home.runs_dir()`) against the
//! SQLite index at the top of every `moagan run` / `moagan continue` /
//! `moagan discover` invocation. The same primitives are exposed to
//! `moagan repair --cleanup-orphans` and `--recover-zombies` so the two
//! surfaces never diverge.
//!
//! The two operations are gated by a single `Config::startup_reconcile`
//! flag (default `true`); the env var `MOAGAN_STARTUP_RECONCILE=false`
//! disables both for an operator who wants a pure dispatcher entry
//! point (e.g. inside a test harness that already pre-cleaned the
//! runs dir).
//!
//! `startup_reconcile` is intentionally non-interactive: the operator
//! is not around to confirm destructive changes. `--yes`-style
//! confirmation only applies to the explicit `moagan repair` subcommand.
//!
//! Recovery semantics:
//! - `cleanup_orphans`: every `*.tmp.<uuid>` atomic-write leftover
//!   inside `home.runs_dir()/<id>/` is removed. Stale top-level
//!   `*.lock` files older than `STALE_LOCK_SECS` are removed as well.
//!   See [`cleanup_orphans`] for the matching reindex-safe variant.
//! - `recover_zombies`: every run whose `status = 'running'` and
//!   `updated_unix < now - ZOMBIE_HEARTBEAT_SECS` is flipped to
//!   `interrupted` and a `run.zombie_recovered` outbox event is
//!   emitted. See [`recover_zombies`].
//!
//! The combined report ([`StartupReconcileReport`]) is logged via
//! `tracing` so the operator can see the count after every startup.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::config::Config;
use crate::error::Result;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::outbox_tx::{OutboxEvent, record_with};
use crate::storage::sqlite::Db;

/// Stale-lock threshold for `cleanup_orphans`. Anything older than
/// 1 h is considered abandoned (the process that took the lock has
/// been dead long enough for the OS to have reaped the PID).
pub const STALE_LOCK_SECS: i64 = 3600;

/// Zombie heartbeat threshold for `recover_zombies`. Two hours
/// matches the existing `interrupted` semantics elsewhere in the
/// pipeline: a phase that has not advanced in two hours is no
/// longer "running" by any reasonable definition.
pub const ZOMBIE_HEARTBEAT_SECS: i64 = 7200;

/// Aggregate report returned by [`startup_reconcile`]. Logged via
/// `tracing::info!` at the top of every dispatcher entry so the
/// operator can see how many orphans / zombies the boot pass
/// touched. Counters are independent: a single startup can flip
/// several zombies and remove several orphans in the same pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReconcileReport {
    /// Files removed by `cleanup_orphans`. Includes both `*.tmp.<hex>`
    /// atomic-write leftovers and stale top-level `*.lock` files.
    pub orphans_removed: usize,
    /// Runs flipped from `running` to `interrupted` by
    /// `recover_zombies`. Each flip is accompanied by a
    /// `run.zombie_recovered` outbox event.
    pub zombies_recovered: usize,
}

/// Run the startup reconcile pass. Equivalent to invoking
/// `cleanup_orphans` followed by `recover_zombies` against the
/// resolved `home` and `db`. The function never aborts on a
/// recoverable failure: a single missing file or a stale row
/// surfaces as an error returned by the inner helpers, and
/// `startup_reconcile` bubbles it up via `?` so the caller (the
/// central dispatcher) can decide whether to proceed.
///
/// `Config::startup_reconcile` is honoured at the dispatcher
/// layer, not here: the caller is responsible for skipping the
/// call when the operator opted out.
pub fn startup_reconcile(
    home: &MoaganHome,
    db: &Db,
    _cfg: &Config,
) -> Result<StartupReconcileReport> {
    let orphans_removed = cleanup_orphans(home)?;
    let zombies_recovered = recover_zombies(db)?;
    Ok(StartupReconcileReport {
        orphans_removed,
        zombies_recovered,
    })
}

/// D.28.3 — walk the runs dir for `*.tmp.<uuid>` atomic-write
/// leftovers and `*.lock` files with `mtime > STALE_LOCK_SECS`.
/// Returns the number of files actually removed.
///
/// This is the non-interactive variant used at startup: every
/// orphan that the walk discovers is removed without prompting.
/// The interactive variant (with `--dry-run` / `--yes`) lives in
/// `src/cli/repair.rs` and delegates here after confirming the
/// operator intent.
pub fn cleanup_orphans(home: &MoaganHome) -> Result<usize> {
    let target_runs = resolve_target_runs_for_cleanup(home)?;
    let plan = plan_cleanup(home, &target_runs)?;
    if plan.is_empty() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for p in &plan {
        match std::fs::remove_file(p) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Race: another process or a concurrent repair pass
                // already removed it. Treat as success.
                removed += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(removed)
}

/// For the cleanup-orphans path we walk the filesystem directly
/// (orphan files may not have a corresponding DB row). The list
/// of run directories is read from the filesystem, not the index,
/// so the helper still works on a freshly-created `MOAGAN_HOME`
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
pub fn plan_cleanup(home: &MoaganHome, target_runs: &[RunId]) -> Result<Vec<PathBuf>> {
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
pub fn is_atomic_tmp(path: &Path) -> bool {
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

/// Public wrapper that calls the private planner. The CLI repair
/// dispatcher uses this to render the destructive plan to the
/// operator before asking for `--yes`. The startup reconcile
/// path does not need this; it just calls [`cleanup_orphans`]
/// directly.
pub fn plan_cleanup_for_report(home: &MoaganHome) -> Result<Vec<PathBuf>> {
    let target_runs = resolve_target_runs_for_cleanup(home)?;
    plan_cleanup(home, &target_runs)
}

/// D.28.4 — find runs whose `status = 'running'` and
/// `updated_unix < now - ZOMBIE_HEARTBEAT_SECS`, mark them
/// `interrupted`, and emit a `run.zombie_recovered` outbox
/// event per recovery. Returns the number of recoveries.
pub fn recover_zombies(db: &Db) -> Result<usize> {
    let zombies = list_zombie_run_ids(db)?;
    if zombies.is_empty() {
        return Ok(0);
    }
    let now = crate::time::now_unix_secs();
    for z in &zombies {
        let payload = serde_json::json!({
            "kind": "zombie_recovered",
            "previous_status": "running",
            "new_status": "interrupted",
            "recovered_at_unix": now,
            "stale_threshold_secs": ZOMBIE_HEARTBEAT_SECS,
        });
        let events = [OutboxEvent {
            run_id: *z,
            event_type: "run.zombie_recovered".into(),
            payload: payload.to_string(),
        }];
        record_with(db, &events, || db.update_run_status(*z, "interrupted"))?;
    }
    Ok(zombies.len())
}

/// Walk the runs table and return the set of stale `running`
/// rows that the recovery step would flip. Used by the CLI
/// `moagan repair --recover-zombies` dispatcher so it can
/// print the plan in `--dry-run` mode without touching the DB.
pub fn list_zombie_run_ids(db: &Db) -> Result<Vec<RunId>> {
    let now = crate::time::now_unix_secs();
    let threshold = now - ZOMBIE_HEARTBEAT_SECS;
    let rows = db.list_runs(u32::MAX)?;
    let mut zombies: Vec<RunId> = Vec::new();
    for row in rows {
        let Ok(id) = row.run_id.parse::<RunId>() else {
            continue;
        };
        if row.status != "running" {
            continue;
        }
        if row.updated_unix >= threshold {
            continue;
        }
        zombies.push(id);
    }
    Ok(zombies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_MOAGAN_HOME_LOCK;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_tmp(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("moagan-reconcile-{pid}-{n}-{label}"));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir
    }

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

    /// No orphans on disk: `cleanup_orphans` returns 0 and never
    /// touches anything.
    #[test]
    fn cleanup_orphans_no_targets_is_zero() {
        let tmp = unique_tmp("clean-empty");
        let home = MoaganHome::at(tmp.clone());
        let removed = cleanup_orphans(&home).expect("cleanup must succeed");
        assert_eq!(removed, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Atomic-write leftover: `cleanup_orphans` removes it and
    /// reports the count.
    #[test]
    fn cleanup_orphans_removes_atomic_tmp() {
        let tmp = unique_tmp("clean-tmp");
        let run_id = RunId::new();
        let proposals = tmp.join(".runs").join(run_id.to_string()).join("proposals");
        std::fs::create_dir_all(&proposals).unwrap();
        let orphan = proposals.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&orphan, b"orphan").unwrap();

        let home = MoaganHome::at(tmp.clone());
        let removed = cleanup_orphans(&home).expect("cleanup must succeed");
        assert_eq!(removed, 1, "exactly one orphan must be removed");
        assert!(!orphan.exists(), "orphan must be gone");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `recover_zombies` flips a stale `running` row to
    /// `interrupted` and emits the matching outbox event.
    #[test]
    fn recover_zombies_flips_stale_running() {
        let tmp = unique_tmp("recover-zombies");
        let home = MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let zombie = RunId::new();
        let alive = RunId::new();
        let now = crate::time::now_unix_secs();
        db.register_run(zombie, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        db.register_run(alive, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let past = now - ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie, past)
            .expect("backdate");

        let recovered = recover_zombies(&db).expect("recover must succeed");
        assert_eq!(recovered, 1, "exactly one zombie must be recovered");

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

    /// Combined pass: a fresh `home` with both a tmp orphan and a
    /// stale running run reports the totals on
    /// `StartupReconcileReport`.
    #[test]
    fn reconcile_startup_cleans_orphans_and_zombies() {
        let tmp = unique_tmp("reconcile-startup");
        let guard = lock_env(&tmp);
        let home = MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");

        // Orphan: *.tmp.<hex> inside a fake run dir.
        let orphan_run = RunId::new();
        let proposals = tmp
            .join(".runs")
            .join(orphan_run.to_string())
            .join("proposals");
        std::fs::create_dir_all(&proposals).unwrap();
        let orphan = proposals.join("p_001.json.tmp.deadbeef01234567");
        std::fs::write(&orphan, b"orphan").unwrap();

        // Zombie: stale `running` row.
        let zombie = RunId::new();
        db.register_run(zombie, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let now = crate::time::now_unix_secs();
        let past = now - ZOMBIE_HEARTBEAT_SECS - 600;
        db._test_backdate_updated_unix(zombie, past)
            .expect("backdate");

        let cfg = Config::default();
        let report = startup_reconcile(&home, &db, &cfg).expect("reconcile must succeed");
        assert_eq!(report.orphans_removed, 1);
        assert_eq!(report.zombies_recovered, 1);
        assert!(!orphan.exists(), "orphan must be gone");

        let zombie_row = db.get_run(zombie).expect("get zombie").unwrap();
        assert_eq!(zombie_row.status, "interrupted");

        drop(db);
        unlock_env(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `Config::startup_reconcile = false` short-circuits the
    /// dispatcher — the operator opted out and the boot pass
    /// must not run. This is the dispatcher-level gate; here we
    /// only assert that `Config` exposes the flag with the
    /// documented default (`true`). The `MOAGAN_STARTUP_RECONCILE`
    /// env-override path is pinned in `config.rs` tests.
    #[test]
    fn reconcile_startup_respects_disabled_config() {
        let cfg = Config::default();
        assert!(
            cfg.startup_reconcile,
            "default Config::startup_reconcile must be true"
        );
        let mut cfg = cfg;
        cfg.startup_reconcile = false;
        assert!(!cfg.startup_reconcile);
    }
}
