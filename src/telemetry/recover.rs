//! Zombie recovery: clean up runs whose leases have expired
//! past a configurable threshold.

use std::time::Duration;

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Find runs whose lease expired more than `threshold` ago and
/// flip their status to `interrupted`, recording the reason
/// inside the run's warning stream.
pub fn recover_zombies(db: &Db, threshold: Duration) -> Result<Vec<RunId>> {
    let candidates = db.find_zombie_runs(threshold.as_secs())?;
    let mut recovered = Vec::new();
    for run_id in candidates {
        db.mark_run_interrupted(run_id, "lease expired (zombie)")?;
        recovered.push(run_id);
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ids::RunId;
    use crate::storage::sqlite::Db;

    fn temp_db() -> Db {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("meta.sqlite");
        std::mem::forget(tmp);
        Db::open(&path).expect("database opens")
    }

    fn acquire_with_ttl(db: &Db, holder: &str, ttl_secs: u64) -> RunId {
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)
            .expect("register");
        db.renew_lease(run_id, holder, Duration::from_secs(ttl_secs), None)
            .expect("lease");
        run_id
    }

    /// A backdated lease above the threshold is recovered and
    /// the run is marked `interrupted`.
    #[test]
    fn recover_zombies_marks_stale_runs_as_interrupted() {
        let db = temp_db();
        let run_id = acquire_with_ttl(&db, "holder-a", 60);
        let now = crate::time::now_unix_secs();
        db._test_backdate_run_lease(run_id, "holder-a", now - 600)
            .expect("backdate");

        let recovered = recover_zombies(&db, Duration::from_secs(120)).expect("recover");

        assert_eq!(recovered, vec![run_id]);
        let row = db.get_run(run_id).expect("get").expect("row exists");
        assert_eq!(row.status, "interrupted");
    }

    /// A fresh lease is below the threshold and stays untouched.
    #[test]
    fn recover_zombies_skips_recent_runs() {
        let db = temp_db();
        let run_id = acquire_with_ttl(&db, "holder-a", 600);
        let now = crate::time::now_unix_secs();
        db._test_backdate_run_lease(run_id, "holder-a", now - 30)
            .expect("backdate within threshold");

        let recovered = recover_zombies(&db, Duration::from_secs(120)).expect("recover");

        assert!(recovered.is_empty());
        let row = db.get_run(run_id).expect("get").expect("row exists");
        assert_eq!(row.status, "running");
    }

    /// No leases in the table at all means nothing to recover.
    #[test]
    fn recover_zombies_returns_empty_when_no_stale_runs() {
        let db = temp_db();
        let recovered = recover_zombies(&db, Duration::from_secs(60)).expect("recover");
        assert!(recovered.is_empty());
    }
}
