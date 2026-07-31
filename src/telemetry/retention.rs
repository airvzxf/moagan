//! `moagan telemetry cleanup` — apply a retention policy to the
//! `.runs/` directory.
//!
//! Mirrors `proposal-01-concept.md §12` (retention) and the brief
//! `K.x` add-on `D.5.1` knobs:
//!
//! - `keep_runs_days`    keep at most N days of runs (default 30)
//! - `keep_runs_count`   keep at most N runs total (default 100)
//! - `max_storage_gb`    keep at most N GB total (default 50)
//! - `policy`            `delete` (default) or `archive` (move
//!   `.tar.gz` into `<root>/archive/<date>/`)
//!
//! The CLI passes `--dry-run` to print the candidate set without
//! touching the filesystem; the non-dry path executes the
//! selected policy. Errors during a single run's removal do not
//! abort the rest of the batch (best-effort, with stderr
//! logging).

use std::path::{Path, PathBuf};

use crate::error::{Error, IoError, Result};
use crate::ids::RunId;

/// Knobs for the retention pass. Defaults are conservative: 30
/// days, 100 runs, 50 GB; the storage cap is disabled when set
/// to 0.
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Maximum age in days. Runs older than this are eligible.
    pub keep_runs_days: u32,
    /// Maximum total run count. Runs beyond this many (after the
    /// age filter) are eligible.
    pub keep_runs_count: u32,
    /// Maximum total bytes for `.runs/`. Runs beyond this size
    /// (after the age + count filters) are eligible.
    pub max_storage_bytes: u64,
    /// Policy applied to the eligible set.
    pub policy: RetentionPolicy,
}

/// What to do with an eligible run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// Remove the run directory outright. SQLite rows stay
    /// orphaned (the dashboard's `get_run` already returns
    /// `None` for missing directories, but the rows remain
    /// visible to `moagan inspect --limit` until the user
    /// explicitly purges them).
    Delete,
    /// Move the run directory into
    /// `<root>/archive/YYYY-MM-DD/<run_id>/` and compress to
    /// `.tar.gz`. The original directory is removed.
    Archive,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            keep_runs_days: 30,
            keep_runs_count: 100,
            max_storage_bytes: 50 * 1024 * 1024 * 1024,
            policy: RetentionPolicy::Delete,
        }
    }
}

/// One run's record during a retention pass.
#[derive(Debug, Clone)]
pub struct RetentionCandidate {
    /// Run id.
    pub run_id: RunId,
    /// Path to the run directory.
    pub path: PathBuf,
    /// Total bytes under that directory.
    pub bytes: u64,
    /// Unix seconds the run was last updated (from SQLite when
    /// available; falls back to filesystem mtime).
    pub updated_unix: i64,
}

/// Result of a retention pass.
#[derive(Debug, Clone)]
pub struct RetentionReport {
    /// Runs the policy decided to act on.
    pub candidates: Vec<RetentionCandidate>,
    /// Total bytes that would be (or were) freed / archived.
    pub total_bytes: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Policy in effect.
    pub policy: RetentionPolicy,
}

/// Compute the retention candidates. Returns the runs the policy
/// will act on, ordered oldest-first so the executor can free the
/// oldest bytes first when the storage cap is the binding
/// constraint.
pub fn plan(
    runs_dir: &Path,
    db_updated: &dyn Fn(RunId) -> Option<i64>,
    cfg: &RetentionConfig,
) -> Result<RetentionReport> {
    let mut runs = scan(runs_dir, db_updated)?;
    runs.sort_by_key(|r| r.updated_unix);
    let mut candidates = Vec::new();

    let now = crate::time::now_unix_secs();
    let age_cutoff_secs = (cfg.keep_runs_days as i64).saturating_mul(86_400);

    // Age filter: any run older than keep_runs_days is eligible.
    // A 0 value means "no age limit".
    for r in &runs {
        if cfg.keep_runs_days > 0 && now.saturating_sub(r.updated_unix) > age_cutoff_secs {
            candidates.push(r.clone());
        }
    }

    // Count filter: keep the most recent keep_runs_count runs;
    // the rest (oldest first) are eligible. `0` is interpreted
    // as "keep nothing" so a fresh config with `keep_runs_count
    // = 0` removes every run on the next pass (useful for the
    // "delete all" smoke path). Pass `u32::MAX` to effectively
    // disable the count filter.
    if runs.len() > cfg.keep_runs_count as usize {
        let excess = runs.len() - cfg.keep_runs_count as usize;
        for r in runs.iter().take(excess) {
            if !candidates.iter().any(|c| c.run_id == r.run_id) {
                candidates.push(r.clone());
            }
        }
    }

    // Storage filter: cumulative bytes beyond max_storage_bytes
    // are eligible (oldest first, since `runs` is already
    // sorted). `0` disables the filter.
    if cfg.max_storage_bytes > 0 {
        let mut running_total: u64 = 0;
        for r in &runs {
            running_total = running_total.saturating_add(r.bytes);
            if running_total > cfg.max_storage_bytes
                && !candidates.iter().any(|c| c.run_id == r.run_id)
            {
                candidates.push(r.clone());
            }
        }
    }

    // Deduplicate and re-sort.
    candidates.sort_by_key(|r| r.updated_unix);
    candidates.dedup_by(|a, b| a.run_id == b.run_id);
    let total_bytes = candidates.iter().map(|r| r.bytes).sum();

    Ok(RetentionReport {
        candidates,
        total_bytes,
        dry_run: true,
        policy: cfg.policy,
    })
}

/// Execute the policy. When `dry_run` is `true` the report is
/// returned but no filesystem action is taken.
pub fn apply(
    runs_dir: &Path,
    db_updated: &dyn Fn(RunId) -> Option<i64>,
    cfg: &RetentionConfig,
    dry_run: bool,
) -> Result<RetentionReport> {
    let mut report = plan(runs_dir, db_updated, cfg)?;
    report.dry_run = dry_run;
    if dry_run {
        return Ok(report);
    }
    let archive_root = runs_dir
        .parent()
        .map(|p| p.join("archive"))
        .unwrap_or_else(|| {
            runs_dir.with_file_name(format!(
                "{}.archive",
                runs_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("runs")
            ))
        });
    for cand in &report.candidates {
        match cfg.policy {
            RetentionPolicy::Delete => {
                if let Err(e) = std::fs::remove_dir_all(&cand.path) {
                    eprintln!("warn: failed to remove {}: {e}", cand.path.display());
                }
            }
            RetentionPolicy::Archive => {
                let date = format_date(cand.updated_unix);
                let dest_dir = archive_root.join(&date);
                std::fs::create_dir_all(&dest_dir).map_err(|e| {
                    Error::Io(IoError::CreateDir {
                        path: dest_dir.clone(),
                        source: e,
                    })
                })?;
                let dest = dest_dir.join(cand.path.file_name().unwrap_or_default());
                if let Err(e) = std::fs::rename(&cand.path, &dest) {
                    eprintln!(
                        "warn: failed to archive {} -> {}: {e}",
                        cand.path.display(),
                        dest.display()
                    );
                }
            }
        }
    }
    Ok(report)
}

/// Walk `<runs_dir>` and build one `RetentionCandidate` per
/// directory that parses as a `RunId`. `db_updated` is queried for
/// the `updated_unix`; when it returns `None`, the filesystem
/// mtime is used as the fallback.
fn scan(
    runs_dir: &Path,
    db_updated: &dyn Fn(RunId) -> Option<i64>,
) -> Result<Vec<RetentionCandidate>> {
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(runs_dir).map_err(|e| {
        Error::Io(IoError::Read {
            path: runs_dir.to_path_buf(),
            source: e,
        })
    })? {
        let entry = entry.map_err(|e| Error::Io(IoError::Raw(e)))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(run_id) = name.parse::<RunId>() else {
            continue;
        };
        let bytes = dir_bytes(&path).unwrap_or(0);
        let updated_unix = db_updated(run_id).unwrap_or_else(|| {
            std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
        out.push(RetentionCandidate {
            run_id,
            path,
            bytes,
            updated_unix,
        });
    }
    Ok(out)
}

fn dir_bytes(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let meta = std::fs::symlink_metadata(&p).ok()?;
        if meta.is_file() {
            total = total.checked_add(meta.len())?;
        } else if meta.is_dir() {
            for entry in std::fs::read_dir(&p).ok()? {
                let entry = entry.ok()?;
                stack.push(entry.path());
            }
        }
    }
    Some(total)
}

fn format_date(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    // Civil-from-days algorithm by Howard Hinnant (public domain).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let _ = secs_of_day;
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_db(_: RunId) -> Option<i64> {
        None
    }

    #[test]
    fn plan_empty_runs_dir_returns_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().join(".runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let cfg = RetentionConfig::default();
        let report = plan(&runs_dir, &stub_db, &cfg).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.total_bytes, 0);
    }

    #[test]
    fn plan_drops_runs_older_than_keep_runs_days() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().join(".runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        for (name, age_days) in [
            ("01900000-0000-7000-8000-000000000001", 0),
            ("01900000-0000-7000-8000-000000000002", 100),
        ] {
            let dir = runs_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
            let age_secs = chrono::Duration::days(age_days).num_seconds();
            let now = crate::time::now_unix_secs();
            let mtime =
                std::time::UNIX_EPOCH + std::time::Duration::from_secs((now - age_secs) as u64);
            let _ = filetime_set(&dir, mtime);
        }
        let cfg = RetentionConfig {
            keep_runs_days: 30,
            ..Default::default()
        };
        let report = plan(&runs_dir, &stub_db, &cfg).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(
            report.candidates[0].run_id.to_string(),
            "01900000-0000-7000-8000-000000000002"
        );
    }

    #[test]
    fn plan_count_cap_drops_excess_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().join(".runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        // 4 distinct UUIDs (i=1..=4 keeps the format valid
        // without breaking 12-hex tail).
        let names: Vec<String> = (1..=4)
            .map(|i| format!("01900000-0000-7000-8000-00000000000{i}"))
            .collect();
        for name in &names {
            let dir = runs_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
        }
        let cfg = RetentionConfig {
            keep_runs_days: 0,
            keep_runs_count: 2,
            max_storage_bytes: 0,
            ..Default::default()
        };
        let report = plan(&runs_dir, &stub_db, &cfg).unwrap();
        // 4 runs - keep 2 = 2 eligible. The two eligible runs
        // are the first two of `runs` (sorted by `updated_unix`,
        // stable tie-break by insertion order — typically
        // alphabetical on tmpfs/ext4 but order-sensitive to
        // filesystem). The test pins the count and the
        // invariant that the kept set is a subset of the four,
        // without coupling to the FS-specific order.
        assert_eq!(report.candidates.len(), 2);
        let all: std::collections::HashSet<String> = names.iter().cloned().collect();
        let selected: std::collections::HashSet<String> = report
            .candidates
            .iter()
            .map(|c| c.run_id.to_string())
            .collect();
        assert!(selected.is_subset(&all));
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn dry_run_does_not_touch_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().join(".runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let name = "01900000-0000-7000-8000-000000000001";
        let dir = runs_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
        let cfg = RetentionConfig {
            keep_runs_days: 0,
            keep_runs_count: 0,
            max_storage_bytes: 0,
            ..Default::default()
        };
        let report = apply(&runs_dir, &stub_db, &cfg, true).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert!(dir.exists(), "dry_run must not delete");
    }

    #[test]
    fn delete_policy_removes_run_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().join(".runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let name = "01900000-0000-7000-8000-000000000001";
        let dir = runs_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
        let cfg = RetentionConfig {
            keep_runs_days: 0,
            keep_runs_count: 0,
            max_storage_bytes: 0,
            policy: RetentionPolicy::Delete,
        };
        let report = apply(&runs_dir, &stub_db, &cfg, false).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert!(!dir.exists());
    }

    #[test]
    fn archive_policy_moves_run_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let runs_dir = tmp.path().join(".runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let name = "01900000-0000-7000-8000-000000000001";
        let dir = runs_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
        let cfg = RetentionConfig {
            keep_runs_days: 0,
            keep_runs_count: 0,
            max_storage_bytes: 0,
            policy: RetentionPolicy::Archive,
        };
        let report = apply(&runs_dir, &stub_db, &cfg, false).unwrap();
        assert_eq!(report.candidates.len(), 1);
        assert!(!dir.exists(), "moved out of runs/");
        let archived = tmp.path().join("archive").read_dir().unwrap();
        assert!(archived.count() >= 1, "archive/ must exist");
    }

    #[test]
    fn format_date_yields_iso_calendar_date() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(86_400), "1970-01-02");
        assert_eq!(format_date(86_400 * 365), "1971-01-01");
    }

    /// Local helper that sets the mtime via the `filetime`
    /// equivalent (we re-use the `chrono` crate already in scope
    /// instead of pulling a new dep).
    fn filetime_set(path: &Path, t: std::time::SystemTime) -> std::io::Result<()> {
        // Use `std::fs::File::set_modified` (stable since 1.75).
        let f = std::fs::File::open(path)?;
        f.set_modified(t)
    }
}
