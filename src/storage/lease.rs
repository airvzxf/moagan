//! Run leases with TTL and a monotonic fence.

use std::time::{Duration, Instant};

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// RAII guard for one run lease.
#[derive(Debug, Clone)]
pub struct LeaseGuard {
    /// Database owning the lease row.
    pub db: Db,
    /// Run protected by this lease.
    pub run_id: RunId,
    /// Holder identity.
    pub holder: String,
    /// Current fencing token.
    pub fence: u64,
    /// Monotonic instant at which the local lease was acquired or renewed.
    pub acquired_at: Instant,
    /// Lease time-to-live.
    pub ttl: Duration,
}

impl LeaseGuard {
    /// Acquire a lease for `run_id`.
    pub fn acquire(db: &Db, run_id: RunId, holder: &str, ttl: Duration) -> Result<Self> {
        let fence = db.renew_lease(run_id, holder, ttl, None)?;
        Ok(Self {
            db: db.clone(),
            run_id,
            holder: holder.to_string(),
            fence,
            acquired_at: Instant::now(),
            ttl,
        })
    }

    /// Renew the lease and advance its fencing token.
    pub fn renew(&mut self) -> Result<()> {
        let new_fence =
            self.db
                .renew_lease(self.run_id, &self.holder, self.ttl, Some(self.fence))?;
        self.fence = new_fence;
        self.acquired_at = Instant::now();
        Ok(())
    }

    /// Return whether the local lease TTL has elapsed.
    pub fn is_expired(&self) -> bool {
        self.acquired_at.elapsed() > self.ttl
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = self.db.release_run_lease(self.run_id, &self.holder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Db {
        let tmp = tempfile::tempdir().expect("temporary directory");
        let path = tmp.path().join("meta.sqlite");
        std::mem::forget(tmp);
        Db::open(&path).expect("database opens")
    }

    #[test]
    fn lease_acquire_succeeds_first_time() {
        let db = temp_db();
        let lease = LeaseGuard::acquire(&db, RunId::new(), "holder-a", Duration::from_secs(60))
            .expect("first lease must acquire");
        assert_eq!(lease.fence, 1);
    }

    #[test]
    fn lease_acquire_blocks_second_holder() {
        let db = temp_db();
        let run_id = RunId::new();
        let _first = LeaseGuard::acquire(&db, run_id, "holder-a", Duration::from_secs(60))
            .expect("first lease must acquire");
        let second = LeaseGuard::acquire(&db, run_id, "holder-b", Duration::from_secs(60));
        assert!(matches!(second, Err(crate::error::Error::LockHeld(_))));
    }

    #[test]
    fn lease_renew_increments_fence() {
        let db = temp_db();
        let mut lease = LeaseGuard::acquire(&db, RunId::new(), "holder-a", Duration::from_secs(60))
            .expect("lease must acquire");
        assert_eq!(lease.fence, 1);
        lease.renew().expect("lease must renew");
        assert_eq!(lease.fence, 2);
    }

    #[test]
    fn lease_renew_with_stale_fence_returns_error() {
        let db = temp_db();
        let run_id = RunId::new();
        let mut lease = LeaseGuard::acquire(&db, run_id, "holder-a", Duration::from_secs(60))
            .expect("lease must acquire");
        let current = db
            .renew_lease(run_id, "holder-a", Duration::from_secs(60), None)
            .expect("new generation must renew");
        assert_eq!(current, 2);
        let error = lease.renew().expect_err("stale fence must fail");
        assert!(matches!(error, crate::error::Error::LockHeld(_)));
    }

    #[test]
    fn lease_drop_releases_lock() {
        let db = temp_db();
        let run_id = RunId::new();
        let lease = LeaseGuard::acquire(&db, run_id, "holder-a", Duration::from_secs(60))
            .expect("lease must acquire");
        drop(lease);
        let replacement = LeaseGuard::acquire(&db, run_id, "holder-b", Duration::from_secs(60))
            .expect("drop must release the lease");
        assert_eq!(replacement.fence, 1);
    }
}
