//! Cooperative cancellation. A run, a phase, and any long-running
//! future share the same `CancellationToken`; setting it causes every
//! listener to wake up with `Error::Cancelled` or `Error::Cancel`.
//!
//! Compliance: T01-06 §6.4 + 10-integrada-v0 §D.10 (token type wrapper).

use std::sync::Arc;

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
        match reason {
            CancelReason::UserInterrupt | CancelReason::Requested => Self::UserPause,
            CancelReason::TotalTimeout => Self::TimeoutTotal,
            CancelReason::PhaseTimeout(_) => Self::TimeoutPhase,
            CancelReason::PlanExhausted => Self::PlanExceeded,
            CancelReason::ApiKeySwitch => Self::ProviderError,
        }
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
    inner: Arc<TkToken>,
    /// Shared reason cell. The child clones the parent's cell so a
    /// child sees the reason the parent was cancelled with.
    reason: Arc<parking_lot::Mutex<Option<CancelReason>>>,
}

impl Cancel {
    /// Build a fresh, uncancelled token.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TkToken::new()),
            reason: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// Build a child token that is cancelled when the parent is.
    pub fn child(&self) -> Self {
        Self {
            inner: Arc::new(self.inner.child_token()),
            reason: self.reason.clone(),
        }
    }

    /// Cancel the token with a reason.
    pub fn cancel(&self, reason: CancelReason) {
        *self.reason.lock() = Some(reason);
        self.inner.cancel();
    }

    /// Cancel with an urgency tier.
    ///
    /// The current cancellation handle has no child-process registry, so every
    /// tier signals the existing cooperative token. Hard process termination
    /// remains the responsibility of the sandbox process owner.
    pub fn cancel_with_tier(&self, reason: CancelReason, _tier: CancelTier) {
        self.cancel(reason);
    }

    /// True if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Await cancellation. Returns immediately if already cancelled.
    pub async fn cancelled(&self) {
        self.inner.cancelled().await
    }

    /// Current cancel reason, if set.
    pub fn reason(&self) -> Option<CancelReason> {
        self.reason.lock().clone()
    }

    /// Translate into a `crate::Error::Cancelled` with the recorded reason.
    pub fn into_error(&self) -> Error {
        let reason = self.reason();
        match reason {
            Some(r) => Error::Cancelled(r.to_string()),
            None => Error::Cancelled("cancelled".to_owned()),
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
}
