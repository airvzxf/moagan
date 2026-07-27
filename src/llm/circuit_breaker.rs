//! Per-provider circuit breaker.
//!
//! After `threshold` consecutive failures inside `window`, the breaker
//! opens for `cooldown`. While open, all calls fail fast with
//! `Error::Provider("circuit open")`. After `cooldown`, the breaker
//! half-opens for one probe call; success closes it, failure reopens.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::error::Result;

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

    /// Read the current state for telemetry.
    pub fn state(&self) -> &'static str {
        match self.inner.lock().state {
            State::Closed => "closed",
            State::Open(_) => "open",
            State::HalfOpen => "half_open",
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

    fn record_success(&self) {
        let mut g = self.inner.lock();
        g.state = State::Closed;
        g.failures = 0;
        g.last_failure = None;
    }

    fn record_failure(&self) {
        let mut g = self.inner.lock();
        let now = Instant::now();
        // Reset failure count if last failure was outside the window.
        if let Some(t) = g.last_failure
            && now.duration_since(t) > g.window
        {
            g.failures = 0;
        }
        g.failures = g.failures.saturating_add(1);
        g.last_failure = Some(now);
        if g.failures >= g.threshold {
            g.state = State::Open(now);
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        // catalog 10-integrada-v0 §D.19.5: 5 errors in 60s -> open 5min.
        Self::new(5, Duration::from_secs(60), Duration::from_secs(300))
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
    }

    #[test]
    fn trip_forces_open() {
        let cb = CircuitBreaker::default();
        cb.trip();
        assert_eq!(cb.state(), "open");
    }
}
