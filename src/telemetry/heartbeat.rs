//! Heartbeat loop: extends a run lease until cancelled or expired.

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::storage::lease::LeaseGuard;

/// Spawn a heartbeat task that renews `lease` every `interval`.
///
/// The task exits when:
/// - the cancellation token fires (returns `Ok(ticks)`),
/// - the local lease TTL has elapsed (returns `Ok(ticks)`), or
/// - a renewal attempt fails (returns `Err`).
pub fn spawn(
    mut lease: LeaseGuard,
    interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<Result<u64>> {
    tracing::info!(
        interval_ms = interval.as_millis() as u64,
        "heartbeat::spawn: enter"
    );
    tokio::spawn(async move {
        tracing::debug!("heartbeat task: started");
        let mut interval = tokio::time::interval(interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(ticks, "heartbeat cancelled");
                    return Ok(ticks);
                }
                _ = interval.tick() => {
                    if lease.is_expired() {
                        tracing::info!(ticks, "heartbeat lease expired");
                        return Ok(ticks);
                    }
                    if let Err(e) = lease.renew() {
                        tracing::error!(error = %e, "heartbeat renew failed");
                        return Err(e);
                    }
                    ticks += 1;
                    tracing::trace!(ticks, "heartbeat renewed");
                }
            }
        }
    })
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

    /// Two renewals under a long TTL bump the fence.
    #[tokio::test(flavor = "current_thread")]
    async fn heartbeat_extends_lease_when_active() {
        let db = temp_db();
        let lease = LeaseGuard::acquire(&db, RunId::new(), "holder-a", Duration::from_secs(60))
            .expect("lease must acquire");
        let cancel = CancellationToken::new();
        let handle = spawn(lease, Duration::from_millis(10), cancel.clone());
        tokio::time::sleep(Duration::from_millis(45)).await;
        cancel.cancel();
        let ticks = handle.await.expect("join").expect("heartbeat must succeed");
        assert!(ticks >= 1, "heartbeat must have ticked at least once");
    }

    /// Cancelling the token stops the loop.
    #[tokio::test(flavor = "current_thread")]
    async fn heartbeat_stops_on_cancellation() {
        let db = temp_db();
        let lease = LeaseGuard::acquire(&db, RunId::new(), "holder-a", Duration::from_secs(60))
            .expect("lease must acquire");
        let cancel = CancellationToken::new();
        let handle = spawn(lease, Duration::from_millis(20), cancel.clone());
        tokio::time::sleep(Duration::from_millis(5)).await;
        cancel.cancel();
        let ticks = handle.await.expect("join").expect("heartbeat must succeed");
        assert!(ticks <= 2, "early cancel must limit ticks: {ticks}");
    }

    /// A short TTL causes the loop to exit once the local timer
    /// is past the lease deadline. The lease is acquired with a
    /// 20 ms TTL, then we wait 60 ms before spawning so the
    /// local `Instant` is already past the deadline; the first
    /// tick of the heartbeat observes `is_expired()` and exits
    /// without renewing the lease in the DB.
    #[tokio::test(flavor = "current_thread")]
    async fn heartbeat_returns_when_lease_expires() {
        let db = temp_db();
        let lease = LeaseGuard::acquire(&db, RunId::new(), "holder-a", Duration::from_millis(20))
            .expect("lease must acquire");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let cancel = CancellationToken::new();
        let handle = spawn(lease, Duration::from_secs(60), cancel);
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("heartbeat must finish in time")
            .expect("join must succeed");
        let ticks = result.expect("heartbeat must not error on expiry");
        assert_eq!(ticks, 0, "expired branch must return zero ticks");
    }
}
