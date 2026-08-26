//! Run leases with TTL and a monotonic fence.
//!
//! Two layered concepts share the `process_locks` table:
//!
//! 1. **Run lease** ([`LeaseGuard`], used by the pipeline
//!    heartbeat): keyed by `{run_id}|{holder}` (string holder),
//!    fence bumps on every renewal, holder is a free-form label
//!    like `"heartbeat"`.
//!
//! 2. **Process lease** ([`ProcessLease`], the typed API): keyed
//!    by `{run_id}|{holder_uuid}` so it does not collide with
//!    lease keys used by [`LeaseGuard`]. The fence is allocated
//!    server-side by SQL (`MAX(fence) + 1` across the whole
//!    table) so two simultaneous acquires always receive
//!    different tokens. The struct exposes both
//!    [`ProcessLease::acquired_at_unix`] (set once on acquire)
//!    and [`ProcessLease::last_heartbeat_unix`] (refreshed on
//!    every [`heartbeat_process_lock`]) so a stale holder can
//!    detect a takeover by either the fence mismatch or the
//!    timestamp moving forward without its cooperation.
//!
//! The legacy [`Db::acquire_process_lock`] /
//! [`Db::release_process_lock`] primitives in `src/storage/sqlite.rs`
//! remain available for callers that need the low-level
//! caller-supplied-fence behaviour; the typed API in this module
//! is the recommended entry point for new code (T01-06 D.1.5).

use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::error::{Error, Result};
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
        tracing::debug!(%run_id, holder, ttl_secs = ttl.as_secs(), "LeaseGuard::acquire: enter");
        let fence = db.renew_lease(run_id, holder, ttl, None)?;
        tracing::info!(%run_id, holder, fence, "LeaseGuard::acquire: ok");
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
        tracing::trace!(
            %self.run_id,
            holder = %self.holder,
            fence = self.fence,
            "LeaseGuard::renew: enter"
        );
        let new_fence =
            self.db
                .renew_lease(self.run_id, &self.holder, self.ttl, Some(self.fence))?;
        self.fence = new_fence;
        self.acquired_at = Instant::now();
        tracing::debug!(
            %self.run_id,
            holder = %self.holder,
            fence = self.fence,
            "LeaseGuard::renew: ok"
        );
        Ok(())
    }

    /// Return whether the local lease TTL has elapsed.
    pub fn is_expired(&self) -> bool {
        let expired = self.acquired_at.elapsed() > self.ttl;
        tracing::trace!(
            %self.run_id,
            holder = %self.holder,
            expired,
            "LeaseGuard::is_expired"
        );
        expired
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        tracing::debug!(
            %self.run_id,
            holder = %self.holder,
            fence = self.fence,
            "LeaseGuard::drop: releasing lease"
        );
        let _ = self.db.release_run_lease(self.run_id, &self.holder);
    }
}

// =========================================================================
// ProcessLease: typed process-lock API (T01-06 D.1.5)
// =========================================================================

/// Row shape returned by the process-lock helpers. The `process_locks`
/// table is shared with [`LeaseGuard`] (per-run lease); this struct
/// targets the cross-process / cross-rotation use case where a
/// monotonic numeric fence and a separate heartbeat timestamp
/// matter.
#[derive(Debug, Clone)]
pub struct ProcessLease {
    /// Run protected by this lease.
    pub run_id: RunId,
    /// Holder identity (process UUID).
    pub holder: Uuid,
    /// Monotonic fencing token. Validated on every
    /// [`heartbeat_process_lock`] and [`release_process_lock`].
    pub fencing_token: u64,
    /// UNIX-seconds stamp at which the lease was first acquired.
    /// Stays constant across heartbeats.
    pub acquired_at_unix: i64,
    /// UNIX-seconds stamp of the most recent
    /// [`heartbeat_process_lock`] (or the original acquire time on
    /// a fresh lease).
    pub last_heartbeat_unix: i64,
    /// UNIX-seconds stamp at which the lease expires if not
    /// heartbeated. Set once on acquire; never refreshed by
    /// heartbeat (the TTL is the abandon deadline, not a rolling
    /// window).
    pub expires_at_unix: i64,
}

fn lock_key(run_id: RunId, holder: Uuid) -> String {
    format!("{run_id}|{holder}")
}

/// Acquire a process lock for `(run_id, holder)`. Generates a
/// monotonic `u64` fencing token via `MAX(fence) + 1` across the
/// whole `process_locks` table. Fails with [`Error::LockHeld`]
/// when a non-expired row already exists for that key.
///
/// The TTL is the abandon deadline: a holder that stops calling
/// [`heartbeat_process_lock`] loses the lock `ttl_secs` after
/// the original acquire; the row stays in place until then so a
/// stale observer can still read its fence.
pub fn acquire_process_lock(
    db: &Db,
    run_id: RunId,
    holder: Uuid,
    ttl_secs: u64,
) -> Result<ProcessLease> {
    tracing::debug!(%run_id, %holder, ttl_secs, "acquire_process_lock: enter");
    let conn = db.pool().get()?;
    let key = lock_key(run_id, holder);
    let now = crate::time::now_unix_secs();
    let ttl_i64 =
        i64::try_from(ttl_secs).map_err(|_| Error::InvalidArgs("ttl_secs overflows i64".into()))?;
    let expires = now
        .checked_add(ttl_i64)
        .ok_or_else(|| Error::InvalidArgs("ttl_secs pushes expires_at past i64::MAX".into()))?;

    // Inspect the existing row, if any.
    let existing: Option<(String, i64)> = conn
        .query_row(
            "SELECT fence, expires_at_unix FROM process_locks WHERE holder = ?",
            params![&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    if let Some((_, existing_expires)) = existing
        && existing_expires > now
    {
        tracing::warn!(
            %run_id,
            %holder,
            existing_expires,
            now,
            "acquire_process_lock: existing non-expired lease blocks acquire"
        );
        return Err(Error::LockHeld(format!(
            "process lock held for run={run_id} holder={holder}"
        )));
    }

    // Allocate the next fence across the whole table. The column
    // is TEXT (kept that way for backward compatibility with the
    // legacy Db::acquire_process_lock / Db::renew_lease primitives
    // that write caller-supplied strings), so CAST to INTEGER for
    // the MAX and store the decimal string back.
    let next_fence_i64: i64 = conn.query_row(
        "SELECT COALESCE(MAX(CAST(fence AS INTEGER)), 0) + 1 FROM process_locks",
        [],
        |row| row.get(0),
    )?;
    let next_fence = u64::try_from(next_fence_i64).map_err(|_| Error::Provider {
        message: "process lock fence overflowed u64".into(),
        http_status: None,
    })?;

    conn.execute(
        "INSERT INTO process_locks \
            (holder, acquired_at_unix, expires_at_unix, fence, last_heartbeat_unix) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(holder) DO UPDATE SET \
            acquired_at_unix = excluded.acquired_at_unix, \
            expires_at_unix = excluded.expires_at_unix, \
            fence = excluded.fence, \
            last_heartbeat_unix = excluded.last_heartbeat_unix",
        params![&key, now, expires, next_fence.to_string(), now],
    )?;

    tracing::info!(%run_id, %holder, fencing_token = next_fence, expires, "acquire_process_lock: ok");
    Ok(ProcessLease {
        run_id,
        holder,
        fencing_token: next_fence,
        acquired_at_unix: now,
        last_heartbeat_unix: now,
        expires_at_unix: expires,
    })
}

/// Heartbeat a held lease: validate the fencing token and bump
/// [`ProcessLease::last_heartbeat_unix`] to `now()`. The TTL
/// (`expires_at_unix`) is **not** refreshed — the deadline was
/// fixed at acquire time. Fails with [`Error::LockHeld`] when no
/// row exists for the key, when the stored fence does not match
/// `fencing_token`, or when the lease has already expired.
pub fn heartbeat_process_lock(
    db: &Db,
    run_id: RunId,
    holder: Uuid,
    fencing_token: u64,
) -> Result<ProcessLease> {
    tracing::debug!(
        %run_id,
        %holder,
        fencing_token,
        "heartbeat_process_lock: enter"
    );
    let conn = db.pool().get()?;
    let key = lock_key(run_id, holder);
    let now = crate::time::now_unix_secs();

    let row: Option<(String, i64, i64, i64)> = conn
        .query_row(
            "SELECT fence, expires_at_unix, acquired_at_unix, last_heartbeat_unix \
             FROM process_locks WHERE holder = ?",
            params![&key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    let Some((stored_fence, stored_expires, stored_acquired, _stored_heartbeat)) = row else {
        tracing::warn!(%run_id, %holder, "heartbeat_process_lock: no row -> LockHeld");
        return Err(Error::LockHeld(format!(
            "process lock not held for run={run_id} holder={holder}"
        )));
    };

    let parsed_fence = stored_fence.parse::<u64>().map_err(|_| Error::Provider {
        message: "process lock fence is not a u64".into(),
        http_status: None,
    })?;
    if parsed_fence != fencing_token || stored_expires <= now {
        tracing::warn!(
            %run_id,
            %holder,
            stored_fence = parsed_fence,
            stored_expires,
            now,
            "heartbeat_process_lock: fence mismatch or expired -> LockHeld"
        );
        return Err(Error::LockHeld(format!(
            "process lock fence mismatch or expired for run={run_id} holder={holder}"
        )));
    }

    conn.execute(
        "UPDATE process_locks SET last_heartbeat_unix = ? WHERE holder = ?",
        params![now, &key],
    )?;

    tracing::debug!(
        %run_id,
        %holder,
        fencing_token = parsed_fence,
        "heartbeat_process_lock: ok"
    );
    Ok(ProcessLease {
        run_id,
        holder,
        fencing_token: parsed_fence,
        acquired_at_unix: stored_acquired,
        last_heartbeat_unix: now,
        expires_at_unix: stored_expires,
    })
}

/// Release a held lease after validating its fencing token.
/// Idempotent on the row-not-found path: releasing a non-existent
/// row returns `Ok(())` silently so a benign double-release does
/// not fail the caller. A token mismatch or expired lease returns
/// [`Error::LockHeld`].
pub fn release_process_lock(
    db: &Db,
    run_id: RunId,
    holder: Uuid,
    fencing_token: u64,
) -> Result<()> {
    tracing::debug!(
        %run_id,
        %holder,
        fencing_token,
        "release_process_lock: enter"
    );
    let conn = db.pool().get()?;
    let key = lock_key(run_id, holder);
    let now = crate::time::now_unix_secs();

    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT fence, expires_at_unix FROM process_locks WHERE holder = ?",
            params![&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let Some((stored_fence, stored_expires)) = row else {
        // Already released — treat as success.
        tracing::debug!(%run_id, %holder, "release_process_lock: row gone, idempotent ok");
        return Ok(());
    };

    let parsed_fence = stored_fence.parse::<u64>().map_err(|_| Error::Provider {
        message: "process lock fence is not a u64".into(),
        http_status: None,
    })?;
    if parsed_fence != fencing_token || stored_expires <= now {
        tracing::warn!(
            %run_id,
            %holder,
            stored_fence = parsed_fence,
            stored_expires,
            now,
            "release_process_lock: fence mismatch or expired -> LockHeld"
        );
        return Err(Error::LockHeld(format!(
            "process lock fence mismatch or expired for run={run_id} holder={holder}"
        )));
    }

    conn.execute("DELETE FROM process_locks WHERE holder = ?", params![&key])?;
    tracing::info!(%run_id, %holder, fencing_token = parsed_fence, "release_process_lock: ok");
    Ok(())
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

    // -----------------------------------------------------------------
    // ProcessLease typed-API tests (T01-06 D.1.5)
    // -----------------------------------------------------------------

    /// A fresh acquire returns a lease with a strictly positive
    /// monotonic fencing token.
    #[test]
    fn process_lease_acquire_returns_positive_fence() {
        let db = temp_db();
        let run_id = RunId::new();
        let holder = Uuid::new_v4();
        let lease = acquire_process_lock(&db, run_id, holder, 60).expect("acquire succeeds");
        assert!(
            lease.fencing_token > 0,
            "fence must be > 0, got {}",
            lease.fencing_token
        );
        assert_eq!(lease.run_id, run_id);
        assert_eq!(lease.holder, holder);
        assert_eq!(lease.acquired_at_unix, lease.last_heartbeat_unix);
        assert!(lease.expires_at_unix > lease.acquired_at_unix);
    }

    /// A duplicate acquire on the same `(run_id, holder)` while
    /// the existing lease is still within its TTL fails with
    /// [`Error::LockHeld`].
    #[test]
    fn process_lease_duplicate_blocks_while_heartbeat_fresh() {
        let db = temp_db();
        let run_id = RunId::new();
        let holder = Uuid::new_v4();
        let _first = acquire_process_lock(&db, run_id, holder, 60).expect("first acquire");
        let second = acquire_process_lock(&db, run_id, holder, 60);
        assert!(
            matches!(second, Err(Error::LockHeld(_))),
            "duplicate acquire must fail with LockHeld, got {second:?}"
        );
    }

    /// An acquire on the same `(run_id, holder)` whose previous
    /// lease has expired (we backdate `expires_at_unix` past
    /// `now()`) succeeds and returns a fence strictly greater than
    /// the previous one.
    #[test]
    fn process_lease_acquire_after_ttl_expiry_succeeds_with_higher_fence() {
        let db = temp_db();
        let run_id = RunId::new();
        let holder = Uuid::new_v4();
        let first = acquire_process_lock(&db, run_id, holder, 60).expect("first acquire");
        let first_fence = first.fencing_token;

        // Backdate the row's expires_at_unix so the next acquire
        // sees the lock as abandoned. The DB exposes this via the
        // raw pool for tests; production code never writes
        // negative TTLs.
        {
            let conn = db.pool().get().expect("pool");
            conn.execute(
                "UPDATE process_locks SET expires_at_unix = ? WHERE holder = ?",
                params![
                    crate::time::now_unix_secs() - 1,
                    &format!("{run_id}|{holder}")
                ],
            )
            .expect("backdate");
        }

        let second = acquire_process_lock(&db, run_id, holder, 60).expect("second acquire");
        assert!(
            second.fencing_token > first_fence,
            "second fence {} must exceed first fence {}",
            second.fencing_token,
            first_fence
        );
        assert_eq!(second.acquired_at_unix, second.last_heartbeat_unix);
    }

    /// `release_process_lock` rejects an invalid fencing token
    /// with [`Error::LockHeld`]. A subsequent release with the
    /// correct token succeeds (and is idempotent on a third call).
    #[test]
    fn process_lease_release_with_wrong_fence_fails() {
        let db = temp_db();
        let run_id = RunId::new();
        let holder = Uuid::new_v4();
        let lease = acquire_process_lock(&db, run_id, holder, 60).expect("acquire");

        let bad = release_process_lock(&db, run_id, holder, lease.fencing_token + 99);
        assert!(
            matches!(bad, Err(Error::LockHeld(_))),
            "release with wrong fence must fail with LockHeld, got {bad:?}"
        );

        release_process_lock(&db, run_id, holder, lease.fencing_token)
            .expect("release with correct fence succeeds");
        // Idempotent: a second release is Ok(()) even though the
        // row is gone.
        release_process_lock(&db, run_id, holder, lease.fencing_token)
            .expect("second release is idempotent");
    }

    /// `heartbeat_process_lock` rejects a stale fencing token with
    /// [`Error::LockHeld`]; a heartbeat with the correct token
    /// advances [`ProcessLease::last_heartbeat_unix`] but leaves
    /// [`ProcessLease::expires_at_unix`] untouched.
    #[test]
    fn process_lease_heartbeat_with_wrong_fence_fails() {
        let db = temp_db();
        let run_id = RunId::new();
        let holder = Uuid::new_v4();
        let lease = acquire_process_lock(&db, run_id, holder, 60).expect("acquire");
        let original_expires = lease.expires_at_unix;

        let bad = heartbeat_process_lock(&db, run_id, holder, lease.fencing_token + 7);
        assert!(
            matches!(bad, Err(Error::LockHeld(_))),
            "heartbeat with wrong fence must fail with LockHeld, got {bad:?}"
        );

        let renewed =
            heartbeat_process_lock(&db, run_id, holder, lease.fencing_token).expect("heartbeat");
        assert_eq!(renewed.fencing_token, lease.fencing_token);
        assert_eq!(renewed.acquired_at_unix, lease.acquired_at_unix);
        assert!(renewed.last_heartbeat_unix >= lease.last_heartbeat_unix);
        assert_eq!(
            renewed.expires_at_unix, original_expires,
            "expires_at_unix must NOT be refreshed by heartbeat"
        );
    }
}
