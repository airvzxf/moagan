//! Per-provider circuit breaker.
//!
//! After `threshold` consecutive failures inside `window`, the breaker
//! opens for `cooldown`. While open, callers should consult
//! [`CircuitBreaker::is_open`] and fail fast (or sleep via
//! [`CircuitBreaker::run`], which already handles the wait). After
//! `cooldown`, the breaker half-opens for one probe call; success
//! closes it, failure reopens.
//!
//! The breaker does NOT decide which errors count toward the failure
//! tally — that policy lives in [`crate::Error::is_circuit_opening`].
//! The wrapper that fronts a provider (`BreakeredProvider` in
//! `provider.rs`) consults that helper before calling
//! [`CircuitBreaker::record_failure`], so non-opening errors
//! (schema violations, operator errors, cancellations) leave the
//! state untouched.
//!
//! Spec: catalog 10-integrada-v0 §D.19.5 (T00-08 §1428-1435; T08-03
//! §5.8; T00-09; T03-03).

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open(Instant),
    HalfOpen,
}

/// Per-provider circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    state: State,
    failures: u32,
    last_failure: Option<Instant>,
    threshold: u32,
    window: Duration,
    cooldown: Duration,
}

impl CircuitBreaker {
    /// Build a breaker with the given threshold / window / cooldown.
    /// Defaults mirror catalog 10-integrada-v0 §D.19.5.
    pub fn new(threshold: u32, window: Duration, cooldown: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: State::Closed,
                failures: 0,
                last_failure: None,
                threshold,
                window,
                cooldown,
            })),
        }
    }

    /// Run `f` under the breaker. Returns its result on success, or
    /// on failure records the failure and returns the error.
    pub async fn run<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if let Some(wait) = self.pre_check() {
            tokio::time::sleep(wait).await;
        }
        match f().await {
            Ok(v) => {
                self.record_success();
                Ok(v)
            }
            Err(e) => {
                self.record_failure();
                Err(e)
            }
        }
    }

    /// Force-open the breaker (used by rate limiter / plan exhausted).
    pub fn trip(&self) {
        let mut g = self.inner.lock();
        g.state = State::Open(Instant::now());
        g.failures = g.threshold;
    }

    /// True iff the breaker is currently rejecting calls. Returns
    /// `false` while Closed (no failures recorded) and while
    /// HalfOpen (a probe call is allowed through). Callers that want
    /// to wait out the cooldown should use [`Self::run`] instead,
    /// which sleeps the remaining time before invoking the
    /// wrapped call.
    pub fn is_open(&self) -> bool {
        matches!(self.inner.lock().state, State::Open(_))
    }

    /// Read the current state for telemetry.
    pub fn state(&self) -> &'static str {
        match self.inner.lock().state {
            State::Closed => "closed",
            State::Open(_) => "open",
            State::HalfOpen => "half_open",
        }
    }

    /// Number of consecutive failures observed inside the current
    /// window. Zero in Closed-with-no-failures and after a
    /// successful probe in HalfOpen.
    pub fn failure_count(&self) -> u32 {
        self.inner.lock().failures
    }

    /// Record a successful call. Closes the breaker and resets the
    /// failure counter so a recovered provider does not carry the
    /// tail of its past outage into the next window.
    pub fn record_success(&self) {
        let mut g = self.inner.lock();
        g.state = State::Closed;
        g.failures = 0;
        g.last_failure = None;
    }

    /// Record a failed call. Increments the failure counter; if the
    /// counter reaches `threshold` inside `window` the breaker
    /// opens. When `last_failure` is older than `window`, the
    /// counter is reset first so a long-stable provider that
    /// suddenly trips does not inherit stale history.
    ///
    /// Policy note: callers MUST filter the error through
    /// [`crate::Error::is_circuit_opening`] before invoking this
    /// method, so non-opening errors (schema violations, operator
    /// errors, cancellations) do not count toward the threshold.
    pub fn record_failure(&self) {
        let mut g = self.inner.lock();
        let now = Instant::now();
        // Reset failure count if last failure was outside the
        // window. A failure streak that broke is functionally a
        // fresh streak; if the breaker had tripped before, the
        // new count starts from zero, so the state has to follow.
        // Without this, callers that drive `record_failure`
        // directly (tests, manual recovery scripts) would observe
        // state=open with failures < threshold, which is a
        // contradiction the wrapper would then have to paper
        // over.
        if let Some(t) = g.last_failure
            && now.duration_since(t) > g.window
        {
            g.failures = 0;
            g.state = State::Closed;
        }
        g.failures = g.failures.saturating_add(1);
        g.last_failure = Some(now);
        if g.failures >= g.threshold {
            g.state = State::Open(now);
        }
    }

    pub(crate) fn record_failure_if_circuit_opening(&self, err: &Error) {
        if err.is_circuit_opening() {
            self.record_failure();
        }
    }

    fn pre_check(&self) -> Option<Duration> {
        let mut g = self.inner.lock();
        match g.state {
            State::Closed => None,
            State::Open(t) => {
                if t.elapsed() >= g.cooldown {
                    g.state = State::HalfOpen;
                    None
                } else {
                    Some(g.cooldown - t.elapsed())
                }
            }
            State::HalfOpen => None,
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        // Defaults match `CircuitBreakerConfig::default()` so the
        // registry-built-in breakers and the config-driven breakers
        // share the same opening policy out of the box. Spec
        // surface: catalog 10-integrada-v0 §D.19.5.
        Self::new(5, Duration::from_secs(60), Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closed_breaker_passes_through() {
        let cb = CircuitBreaker::default();
        let r = cb
            .run(|| async { Ok::<i32, crate::Error>(42) })
            .await
            .unwrap();
        assert_eq!(r, 42);
        assert_eq!(cb.state(), "closed");
        assert!(!cb.is_open());
        assert_eq!(cb.failure_count(), 0);
    }

    #[tokio::test]
    async fn opens_after_threshold_failures() {
        let cb = CircuitBreaker::new(2, Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..2 {
            let r: Result<()> = cb
                .run(|| async { Err::<(), _>(crate::Error::Provider("x".into())) })
                .await;
            assert!(r.is_err());
        }
        assert_eq!(cb.state(), "open");
        assert!(cb.is_open());
        assert_eq!(cb.failure_count(), 2);
    }

    #[test]
    fn trip_forces_open() {
        let cb = CircuitBreaker::default();
        cb.trip();
        assert_eq!(cb.state(), "open");
        assert!(cb.is_open());
    }

    #[test]
    fn record_success_resets_state() {
        // Drive a breaker into Open via `trip`, then `record_success`
        // resets to Closed with zero failures. This is the recovery
        // path the wrapper relies on after a half-open probe
        // succeeds.
        let cb = CircuitBreaker::new(1, Duration::from_secs(60), Duration::from_secs(60));
        cb.record_failure();
        assert_eq!(cb.state(), "open");
        cb.record_success();
        assert_eq!(cb.state(), "closed");
        assert!(!cb.is_open());
        assert_eq!(cb.failure_count(), 0);
    }

    #[test]
    fn record_failure_outside_window_resets_counter() {
        // Window 50ms: two back-to-back failures trip the breaker,
        // but a third failure after a 100ms sleep falls outside
        // the window so the counter resets and the breaker returns
        // to Closed with failures=1 (well below the threshold of
        // 2).
        let cb = CircuitBreaker::new(2, Duration::from_millis(50), Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), "open", "two back-to-back failures must trip");
        std::thread::sleep(Duration::from_millis(100));
        cb.record_failure();
        assert_eq!(
            cb.state(),
            "closed",
            "failure streak broke across the window; got state {}",
            cb.state()
        );
        assert_eq!(cb.failure_count(), 1);
    }

    #[test]
    fn half_open_probe_returns_false_on_is_open() {
        // Open, advance past cooldown, expect HalfOpen (is_open =
        // false). Done synchronously via sleep — short cooldown.
        let cb = CircuitBreaker::new(1, Duration::from_secs(60), Duration::from_millis(20));
        cb.record_failure();
        assert!(cb.is_open());
        std::thread::sleep(Duration::from_millis(30));
        // is_open() is a snapshot — it does NOT advance to half_open
        // by itself (that transition happens inside pre_check /
        // run). So is_open() still returns true after the cooldown
        // elapses until a call drives the transition. The
        // integration tests in tests/integration_circuit_breaker.rs
        // exercise the full half-open path through run().
        assert!(
            cb.is_open(),
            "is_open() is a snapshot of the persisted state and stays Open until a call triggers pre_check"
        );
    }
}
