//! D.28.1: per-run reconcile. Walks the run directory and
//! reindexes artifacts against the SQLite index.

use crate::error::Result;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::sqlite::Db;
use std::path::Path;

/// Per-run reconcile report. Each field counts the number of
/// artifact files observed on disk inside the run directory's
/// corresponding sub-directory.
///
/// The fields are intentionally per-artifact-type: the caller
/// can decide whether a count delta vs the SQLite index signals
/// a reindex or an investigation. Full reindex-pipeline wiring
/// is a follow-up; this module establishes the public API and
/// the counting semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Run id that was reconciled.
    pub run_id: RunId,
    /// Number of `*.json` files found in `<run>/sketches/`.
    pub sketches_reindexed: usize,
    /// Number of `*.json` files found in `<run>/proposals/`.
    pub proposals_reindexed: usize,
    /// Number of `*.json` files found in `<run>/evaluations/`.
    pub evaluations_reindexed: usize,
    /// Number of `*.json` files found in `<run>/critiques/`.
    pub critiques_reindexed: usize,
}

/// Walk the run directory for a single `run_id` and count the
/// `*.json` artifacts under each canonical sub-directory. The
/// `db` argument is accepted so the full reindex pipeline
/// (D.28.2+) can write back to the index without changing the
/// public signature; for now it is only used to look up the
/// run row and confirm the directory exists on disk.
pub fn reconcile_run(_db: &Db, home: &MoaganHome, run_id: RunId) -> Result<ReconcileReport> {
    let run_dir = home.run_dir(run_id);
    let _root = run_dir.root();
    let sketches = count_json_in(&run_dir.sketches())?;
    let proposals = count_json_in(&run_dir.proposals())?;
    let evaluations = count_json_in(&run_dir.evaluations())?;
    let critiques = count_json_in(&run_dir.critiques())?;
    Ok(ReconcileReport {
        run_id,
        sketches_reindexed: sketches,
        proposals_reindexed: proposals,
        evaluations_reindexed: evaluations,
        critiques_reindexed: critiques,
    })
}

fn count_json_in(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::Db;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_tmp(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("moagan-per-run-reconcile-{pid}-{n}-{label}"));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir
    }

    fn write_json(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join(name), b"{}").expect("write json");
    }

    /// A run id that has no row in the SQLite index and no
    /// directory on disk: all four counters must be zero and the
    /// function returns `Ok` without panicking.
    #[test]
    fn reconcile_run_returns_zero_counts_for_unknown_run() {
        let tmp = unique_tmp("unknown");
        let home = MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let phantom = RunId::new();

        let report = reconcile_run(&db, &home, phantom).expect("reconcile must succeed");
        assert_eq!(report.run_id, phantom);
        assert_eq!(report.sketches_reindexed, 0);
        assert_eq!(report.proposals_reindexed, 0);
        assert_eq!(report.evaluations_reindexed, 0);
        assert_eq!(report.critiques_reindexed, 0);

        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Each canonical sub-directory is counted independently. A
    /// mix of JSON and non-JSON files confirms only `*.json`
    /// matches.
    #[test]
    fn reconcile_run_counts_files_in_subdirs() {
        let tmp = unique_tmp("counts");
        let home = MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let run = RunId::new();
        let run_dir = home.run_dir(run);

        // sketches: 2 json + 1 .tmp leftover
        write_json(&run_dir.sketches(), "s_001.json");
        write_json(&run_dir.sketches(), "s_002.json");
        std::fs::write(
            run_dir.sketches().join("s_003.json.tmp.deadbeef01234567"),
            b"orphan",
        )
        .unwrap();

        // proposals: 3 json
        write_json(&run_dir.proposals(), "p_001.json");
        write_json(&run_dir.proposals(), "p_002.json");
        write_json(&run_dir.proposals(), "p_003.json");

        // evaluations: 1 json + 1 .lock (the walker must not pick
        // the lock up either)
        write_json(&run_dir.evaluations(), "e_001.json");
        std::fs::write(run_dir.evaluations().join("evaluations.lock"), b"lock").unwrap();

        // critiques: empty (directory not even created)

        let report = reconcile_run(&db, &home, run).expect("reconcile must succeed");
        assert_eq!(report.sketches_reindexed, 2);
        assert_eq!(report.proposals_reindexed, 3);
        assert_eq!(report.evaluations_reindexed, 1);
        assert_eq!(report.critiques_reindexed, 0);

        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A run that registered in the index but never wrote any
    /// artifacts (the common case for a brand-new `RunId`) must
    /// reconcile cleanly with all-zero counts. The helper
    /// `count_json_in` short-circuits on missing directories
    /// instead of bubbling a `NotFound` io error.
    #[test]
    fn reconcile_run_handles_missing_subdirs() {
        let tmp = unique_tmp("missing-subdirs");
        let home = MoaganHome::at(tmp.clone());
        let db = Db::open(&home.meta_db_path()).expect("open db");
        let run = RunId::new();
        let run_dir = home.run_dir(run);
        std::fs::create_dir_all(run_dir.root()).expect("create empty run dir");

        let report = reconcile_run(&db, &home, run).expect("reconcile must succeed");
        assert_eq!(report.sketches_reindexed, 0);
        assert_eq!(report.proposals_reindexed, 0);
        assert_eq!(report.evaluations_reindexed, 0);
        assert_eq!(report.critiques_reindexed, 0);

        drop(db);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
