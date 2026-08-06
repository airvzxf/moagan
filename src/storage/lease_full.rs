//! Full-lease wrapper over the `process_locks` table (D.1.5 wire).
//!
//! `FullLease` exposes a typed API for run-scoped leases backed by
//! the `process_locks` table introduced in schema v008. The fence is
//! stored as an [`AtomicU64`] so concurrent readers (recovery
//! sweeps, monitoring) can observe the latest fencing token without
//! blocking. The lease is RAII-managed: dropping the guard releases
//! the row.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Typed lease wrapping the `process_locks` table.
pub struct FullLease {
    /// Database handle backing this lease.
    pub db: Db,
    /// Run protected by this lease.
    pub run_id: RunId,
    /// Holder identity (also the row key in `process_locks`).
    pub holder: String,
    /// Monotonic fencing token, atomically bumped on each renew.
    pub fence: AtomicU64,
    /// Lease TTL in seconds, captured at acquire time.
    pub ttl_secs: u64,
    /// Unix timestamp at which the lease was acquired.
    pub acquired_unix: i64,
}

impl FullLease {
    /// Acquire a lease for `run_id` on behalf of `holder` with the
    /// given TTL. Returns [`Error::LockHeld`] if another holder
    /// already owns the lock and the TTL has not elapsed.
    pub fn acquire(db: &Db, run_id: RunId, holder: &str, ttl_secs: u64) -> Result<Self> {
        let initial_fence: u64 = 1;
        let acquired = db.acquire_process_lock(holder, ttl_secs, &initial_fence.to_string())?;
        if !acquired {
            return Err(Error::LockHeld(holder.to_string()));
        }
        Ok(Self {
            db: db.clone(),
            run_id,
            holder: holder.to_string(),
            fence: AtomicU64::new(initial_fence),
            ttl_secs,
            acquired_unix: crate::time::now_unix_secs(),
        })
    }

    /// Renew the lease: extend the TTL to `new_ttl_secs` and bump
    /// the fencing token. On success the new fence is reflected in
    /// the local [`AtomicU64`].
    pub fn renew(&self, new_ttl_secs: u64) -> Result<()> {
        let next_fence = self
            .fence
            .fetch_add(1, Ordering::SeqCst)
            .checked_add(1)
            .ok_or_else(|| Error::Provider("FullLease: fence overflow".into()))?;
        let acquired =
            self.db
                .acquire_process_lock(&self.holder, new_ttl_secs, &next_fence.to_string())?;
        if !acquired {
            self.fence.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::LockHeld(self.holder.clone()));
        }
        Ok(())
    }

    /// Release the lease by deleting the row. Safe to call multiple
    /// times: when the row is already gone the underlying helper
    /// returns `Ok(false)` and this method returns `Ok(())`.
    pub fn release(&self) -> Result<()> {
        let _ = self.db.release_process_lock(&self.holder)?;
        Ok(())
    }

    /// Read the current fencing token.
    pub fn current_fence(&self) -> u64 {
        self.fence.load(Ordering::SeqCst)
    }
}

impl Drop for FullLease {
    fn drop(&mut self) {
        let _ = self.db.release_process_lock(&self.holder);
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
    fn full_lease_acquire_succeeds_for_first_holder() {
        let db = temp_db();
        let lease = FullLease::acquire(&db, RunId::new(), "holder-a", 60)
            .expect("first lease must acquire");
        assert_eq!(lease.current_fence(), 1);
        assert_eq!(lease.holder, "holder-a");
        assert_eq!(lease.ttl_secs, 60);
    }

    #[test]
    fn full_lease_blocks_second_holder_within_ttl() {
        let db = temp_db();
        let run_id = RunId::new();
        let _first =
            FullLease::acquire(&db, run_id, "holder-a", 60).expect("first lease must acquire");
        let second = FullLease::acquire(&db, run_id, "holder-b", 60);
        assert!(matches!(second, Err(Error::LockHeld(_))));
    }

    #[test]
    fn full_lease_renew_extends_ttl() {
        let db = temp_db();
        let lease =
            FullLease::acquire(&db, RunId::new(), "holder-a", 60).expect("lease must acquire");
        assert_eq!(lease.current_fence(), 1);
        lease.renew(120).expect("renew must succeed");
        assert_eq!(lease.current_fence(), 2);
        assert_eq!(lease.ttl_secs, 60);
    }

    #[test]
    fn full_lease_release_frees_lock() {
        let db = temp_db();
        let run_id = RunId::new();
        let lease = FullLease::acquire(&db, run_id, "holder-a", 60).expect("lease must acquire");
        lease.release().expect("release must succeed");
        let replacement =
            FullLease::acquire(&db, run_id, "holder-b", 60).expect("release must free the lock");
        assert_eq!(replacement.current_fence(), 1);
    }
}
