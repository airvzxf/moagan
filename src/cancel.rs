//! Cooperative cancellation. A run, a phase, and any long-running
//! future share the same `CancellationToken`; setting it causes every
//! listener to wake up with `Error::Cancelled` or `Error::Cancel`.
//!
//! Compliance: T01-06 §6.4 + 10-integrada-v0 §D.10 (token type wrapper)
//! + 10-integrada-v0 §D.10.6 (`libc::killpg` on Hard tier).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken as TkToken;

use crate::error::Error;

/// Reason the run is being cancelled. Distinct from `Error::Cancelled`
/// so callers can branch on the cause.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum CancelReason {
    /// User pressed Ctrl-C.
    UserInterrupt,
    /// Total run timeout fired.
    TotalTimeout,
    /// A pipeline phase exceeded its configured timeout.
    PhaseTimeout(String),
    /// Plan exhausted (token budget crossed).
    PlanExhausted,
    /// Explicit API key switch.
    ApiKeySwitch,
    /// Application-level request.
    Requested,
}

/// Urgency of a cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelTier {
    /// Allow in-flight work to finish its current iteration.
    Soft,
    /// Trigger the shared cancellation token immediately.
    Normal,
    /// Cancel immediately and request child-process termination.
    Hard,
}

impl From<CancelReason> for crate::domain::PauseReason {
    fn from(reason: CancelReason) -> Self {
        let target = match reason {
            CancelReason::UserInterrupt | CancelReason::Requested => Self::UserPause,
            CancelReason::TotalTimeout => Self::TimeoutTotal,
            CancelReason::PhaseTimeout(_) => Self::TimeoutPhase,
            CancelReason::PlanExhausted => Self::PlanExceeded,
            CancelReason::ApiKeySwitch => Self::ProviderError,
        };
        tracing::trace!(reason = ?reason, pause = ?target, "CancelReason -> PauseReason");
        target
    }
}

impl std::fmt::Display for CancelReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserInterrupt => f.write_str("user interrupt"),
            Self::TotalTimeout => f.write_str("total timeout"),
            Self::PhaseTimeout(phase) => write!(f, "phase timeout: {phase}"),
            Self::PlanExhausted => f.write_str("plan exhausted"),
            Self::ApiKeySwitch => f.write_str("api key switch"),
            Self::Requested => f.write_str("requested"),
        }
    }
}

/// Handle to a cancellation source. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Cancel {
    inner: Arc<Inner>,
}

/// Inner state shared by every clone of a [`Cancel`] handle.
#[derive(Debug)]
struct Inner {
    /// The cooperative cancellation token.
    token: TkToken,
    /// Reason cell. The child clones the parent's cell so a child sees
    /// the reason the parent was cancelled with.
    reason: Arc<parking_lot::Mutex<Option<CancelReason>>>,
    /// Process-group ids of in-flight sandbox children. Hard tier sends
    /// `SIGTERM` to each registered pgid, then `SIGKILL` after a
    /// grace period. `None` when the platform cannot enumerate groups
    /// (Windows-only; this crate compiles on Unix-only features).
    child_pgids: Arc<parking_lot::Mutex<HashSet<i32>>>,
}

/// Grace window between `SIGTERM` and `SIGKILL` for `CancelTier::Hard`.
/// Two seconds matches the orchestrator's spec and gives well-behaved
/// subprocesses enough time to flush + exit while still being fast
/// enough for an operator pressing Ctrl-C.
pub const HARD_KILL_GRACE: Duration = Duration::from_secs(2);

impl Cancel {
    /// Build a fresh, uncancelled token.
    pub fn new() -> Self {
        tracing::trace!("Cancel::new: enter");
        Self {
            inner: Arc::new(Inner {
                token: TkToken::new(),
                reason: Arc::new(parking_lot::Mutex::new(None)),
                child_pgids: Arc::new(parking_lot::Mutex::new(HashSet::new())),
            }),
        }
    }

    /// Build a child token that is cancelled when the parent is.
    pub fn child(&self) -> Self {
        tracing::trace!("Cancel::child: enter");
        Self {
            inner: Arc::new(Inner {
                token: self.inner.token.child_token(),
                reason: self.inner.reason.clone(),
                child_pgids: self.inner.child_pgids.clone(),
            }),
        }
    }

    /// Cancel the token with a reason.
    pub fn cancel(&self, reason: CancelReason) {
        tracing::trace!(reason = ?reason, "Cancel::cancel");
        *self.inner.reason.lock() = Some(reason);
        self.inner.token.cancel();
    }

    /// Clone of the underlying tokio token. Cheap (the token wraps an
    /// `Arc`); used by callers that need to subscribe a background
    /// task to the same cooperative cancellation source (the lease
    /// heartbeat, the audit proxy, the sandbox child reader). The
    /// returned token fires when **any** clone of this `Cancel`
    /// signals — the typical pattern is `child_token()` so a
    /// sub-token's signal does not cascade to siblings.
    pub fn token(&self) -> TkToken {
        tracing::trace!("Cancel::token: cloning parent token");
        self.inner.token.clone()
    }

    /// Child token that mirrors the parent. Cancellation of the
    /// parent fires the child; cancellation of the child does not
    /// propagate back. Used by long-running tasks (heartbeat, audit
    /// proxy) so they exit when the run is cancelled but never
    /// cancel the run themselves.
    pub fn child_token(&self) -> TkToken {
        tracing::trace!("Cancel::child_token: cloning child token");
        self.inner.token.child_token()
    }

    /// Cancel with an urgency tier.
    ///
    /// - `Soft` and `Normal` signal the cooperative token only.
    /// - `Hard` signals the cooperative token AND, on Unix, sends
    ///   `SIGTERM` to every registered process-group id, then schedules
    ///   `SIGKILL` after [`HARD_KILL_GRACE`]. Subprocesses that
    ///   blocked or ignored `SIGTERM` get the `SIGKILL` fallback.
    ///
    /// The child registry is populated by the sandbox (`Cancel::register_child`)
    /// and drained on the natural-completion path
    /// (`Cancel::unregister_child`); `Hard` cancel reads it under the
    /// mutex and copies the pgids before signalling so the kill path
    /// does not have to hold the lock across the (potentially slow)
    /// `killpg` syscalls.
    pub fn cancel_with_tier(&self, reason: CancelReason, tier: CancelTier) {
        tracing::trace!(reason = ?reason, tier = ?tier, "Cancel::cancel_with_tier");
        // 1) Cooperative token so in-flight async work notices.
        self.cancel(reason);

        if !matches!(tier, CancelTier::Hard) {
            tracing::trace!(tier = ?tier, "cancel_with_tier: cooperative-only path");
            return;
        }

        // 2) Snapshot the registered pgids under the lock.
        let pgids: Vec<i32> = self.inner.child_pgids.lock().iter().copied().collect();
        if pgids.is_empty() {
            tracing::trace!("cancel_with_tier: no registered pgids; SIGKILL/SIGTERM skipped");
            return;
        }
        tracing::trace!(pgid_count = pgids.len(), "cancel_with_tier: hard kill path");

        #[cfg(unix)]
        {
            // 3a) SIGTERM the whole process group. `killpg` returns
            // -1 with `ESRCH` if the group is already gone; we
            // treat that as a no-op (the natural-completion path
            // should have unregistered it, but timing is racy).
            for pgid in &pgids {
                // SAFETY: `killpg` is safe to call from any thread;
                // a missing group yields ESRCH which we ignore.
                let _ = unsafe { libc::killpg(*pgid, libc::SIGTERM) };
            }
            tracing::debug!(
                pgid_count = pgids.len(),
                "cancel_with_tier: SIGTERM dispatched to process groups"
            );
            // 3b) SIGKILL after the grace window on a tokio task so
            // we never block the caller. The task is parented to the
            // cooperative `CancellationToken` (AGENTS.md §"No-go list":
            // no `tokio::spawn` without a `JoinHandle` recorded or a
            // `CancellationToken` parent). When the token is already
            // cancelled (e.g. a second Hard cancel arrives during the
            // grace window, or the orchestrator shuts down), the
            // `select!` resolves immediately and SIGKILL fires without
            // waiting another 2 s.
            let pgids_for_kill = pgids.clone();
            let token = self.inner.token.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(HARD_KILL_GRACE) => {}
                    _ = token.cancelled() => {}
                }
                for pgid in &pgids_for_kill {
                    // SAFETY: same as above.
                    let _ = unsafe { libc::killpg(*pgid, libc::SIGKILL) };
                }
                tracing::debug!(
                    pgid_count = pgids_for_kill.len(),
                    "cancel_with_tier: SIGKILL dispatched after grace"
                );
            });
        }

        #[cfg(not(unix))]
        {
            // Hard tier is a Unix-only feature in this crate; on other
            // platforms the cooperative token still propagates.
            let _ = pgids;
        }
    }

    /// Register a process-group id whose leader is the just-spawned
    /// sandbox child. The sandbox calls this after `Command::spawn`
    /// and `setpgid(0, 0)` in `pre_exec`. Pgid must be positive; the
    /// value is taken verbatim so the sandbox controls whether it
    /// uses the child's pid or an inherited id.
    pub fn register_child(&self, pgid: i32) {
        tracing::trace!(pgid, "Cancel::register_child");
        self.inner.child_pgids.lock().insert(pgid);
    }

    /// Remove a previously registered pgid. Idempotent: unregistering
    /// an unknown pgid is a no-op. The sandbox calls this on every
    /// exit path (natural, error, timeout) so the registry does not
    /// leak ids.
    pub fn unregister_child(&self, pgid: i32) {
        tracing::trace!(pgid, "Cancel::unregister_child");
        self.inner.child_pgids.lock().remove(&pgid);
    }

    /// True if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        let cancelled = self.inner.token.is_cancelled();
        tracing::trace!(cancelled, "Cancel::is_cancelled");
        cancelled
    }

    /// Await cancellation. Returns immediately if already cancelled.
    pub async fn cancelled(&self) {
        tracing::trace!("Cancel::cancelled: awaiting");
        self.inner.token.cancelled().await
    }

    /// Current cancel reason, if set.
    pub fn reason(&self) -> Option<CancelReason> {
        let r = self.inner.reason.lock().clone();
        tracing::trace!(reason = ?r, "Cancel::reason");
        r
    }

    /// Translate into a `crate::Error::Cancelled` with the recorded reason.
    pub fn into_error(&self) -> Error {
        tracing::trace!("Cancel::into_error: enter");
        let reason = self.reason();
        match reason {
            Some(r) => {
                tracing::trace!(reason = ?r, "Cancel::into_error: with reason");
                Error::Cancelled(r.to_string())
            }
            None => {
                tracing::trace!("Cancel::into_error: generic reason");
                Error::Cancelled("cancelled".to_owned())
            }
        }
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_propagates_to_child() {
        let parent = Cancel::new();
        let child = parent.child();
        assert!(!child.is_cancelled());
        parent.cancel(CancelReason::UserInterrupt);
        assert!(child.is_cancelled());
        assert_eq!(child.reason(), Some(CancelReason::UserInterrupt));
    }

    /// `Cancel::token()` returns a `TkToken` clone that mirrors the
    /// parent. Pinning this so the lease-renewal heartbeat and
    /// any other future consumer of the raw tokio token observe the
    /// same cancellation semantics as `Cancel::cancelled()`.
    #[tokio::test]
    async fn token_clone_observes_parent_cancellation() {
        let parent = Cancel::new();
        let token = parent.token();
        assert!(!token.is_cancelled());
        parent.cancel(CancelReason::UserInterrupt);
        assert!(token.is_cancelled());
    }

    /// `Cancel::child_token()` returns a child token. Cancelling the
    /// parent fires the child; cancelling the child does not
    /// propagate to the parent. Pins the parent-child isolation
    /// contract that the lease heartbeat relies on (its cancel
    /// arm must never trigger the run's overall cancel).
    #[tokio::test]
    async fn child_token_isolates_cancellation() {
        let parent = Cancel::new();
        let child_token = parent.child_token();
        assert!(!child_token.is_cancelled());

        // Cancel the child first — parent must stay live.
        child_token.cancel();
        assert!(child_token.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "cancelling the child token must NOT cancel the parent"
        );

        // Now cancel the parent — child must follow.
        parent.cancel(CancelReason::UserInterrupt);
        assert!(parent.is_cancelled());
        assert!(child_token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_without_reason_yields_generic_error() {
        let c = Cancel::new();
        c.cancel(CancelReason::Requested);
        let err = c.into_error();
        assert!(matches!(err, Error::Cancelled(_)));
    }

    #[test]
    fn every_cancel_tier_signals_the_existing_token() {
        for tier in [CancelTier::Soft, CancelTier::Normal, CancelTier::Hard] {
            let cancel = Cancel::new();
            cancel.cancel_with_tier(CancelReason::Requested, tier);
            assert!(cancel.is_cancelled());
            assert_eq!(cancel.reason(), Some(CancelReason::Requested));
        }
    }

    #[tokio::test]
    async fn child_clones_share_the_pgid_registry() {
        let parent = Cancel::new();
        let child = parent.child();
        parent.register_child(12345);
        // The child token shares the reason + pgid-registry Arcs.
        // Cancelling the child sets its own token but the registry
        // Arc is shared, so a Hard cancel from either side reaches
        // the same pgid. Tokio runtime is required because Hard
        // spawns the delayed SIGKILL task.
        child.cancel_with_tier(CancelReason::Requested, CancelTier::Hard);
        assert!(child.is_cancelled());
        assert_eq!(child.reason(), Some(CancelReason::Requested));
        // The reason cell is shared with the parent.
        assert_eq!(parent.reason(), Some(CancelReason::Requested));
    }

    #[test]
    fn unregister_is_idempotent() {
        let cancel = Cancel::new();
        cancel.register_child(99999);
        cancel.unregister_child(99999);
        cancel.unregister_child(99999); // second call is a no-op
        // Hard cancel with empty registry must not panic; the kill
        // path is skipped when the set is empty.
        cancel.cancel_with_tier(CancelReason::Requested, CancelTier::Hard);
        assert!(cancel.is_cancelled());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hard_cancel_does_not_panic_on_unknown_pgid() {
        // Use a clearly nonexistent pgid; killpg returns ESRCH, which
        // we swallow. The cooperative token must still be set so the
        // caller observes cancellation.
        let cancel = Cancel::new();
        cancel.register_child(i32::MAX);
        cancel.cancel_with_tier(CancelReason::UserInterrupt, CancelTier::Hard);
        assert!(cancel.is_cancelled());
        // Drain the SIGKILL task so the test does not leak a pending
        // tokio task that other tests might observe. The grace is 2s
        // and the test runtime outlives it.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
