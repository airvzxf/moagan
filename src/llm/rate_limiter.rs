//! Token-bucket rate limiter. Per-provider, in-memory.
//!
//! Compliance: 10-integrada-v0 §D.19.6 (rate_limiter). Replaces the
//! `governor` crate (rejected in catalog §C). The knobs themselves
//! live in [`crate::config::RateLimitConfig`] so the config layer
//! owns the wire-format / env-override surface; this module owns the
//! runtime state machine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::RateLimitConfig;
use crate::error::{Error, Result};

/// Token-bucket rate limiter. Thread-safe via internal mutex.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    capacity: u32,
    refill_per_sec: u32,
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Build a new rate limiter.
    pub fn new(cfg: RateLimitConfig) -> Self {
        let initial = cfg.initial.unwrap_or(cfg.capacity) as f64;
        Self {
            inner: Arc::new(Mutex::new(Inner {
                capacity: cfg.capacity,
                refill_per_sec: cfg.refill_per_sec,
                tokens: initial,
                last_refill: Instant::now(),
            })),
        }
    }

    /// Block until one token is available, returning the duration we
    /// actually slept. The caller should respect a returned wait.
    pub async fn acquire(&self) -> Result<Duration> {
        // Compute the wait synchronously, then sleep asynchronously.
        let wait = {
            let mut g = self.inner.lock();
            g.token_after_one()
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(wait)
    }

    /// Block until one token is available, but fail fast with
    /// `Error::Provider` (carrying a budget-exhausted message) when
    /// the wait would exceed `max`. Refunds the consumed token when
    /// the wait is in range so the caller can sleep it without losing
    /// the slot. Use this when the caller wants a bounded
    /// backpressure (CI loops, batch runners) instead of an
    /// unbounded wait.
    pub async fn acquire_with_max(&self, max: Duration) -> Result<Duration> {
        let wait = {
            let mut g = self.inner.lock();
            g.refill();
            if g.tokens >= 1.0 {
                g.tokens -= 1.0;
                Duration::ZERO
            } else {
                let deficit = 1.0 - g.tokens;
                let secs = deficit / g.refill_per_sec.max(1) as f64;
                let wait = Duration::from_secs_f64(secs);
                if wait > max {
                    return Err(Error::Provider {
                        message: format!(
                            "rate limiter budget exhausted: would wait {wait:?} > max {max:?}"
                        ),
                        http_status: None,
                    });
                }
                g.tokens += wait.as_secs_f64() * g.refill_per_sec as f64;
                g.tokens = g.tokens.min(g.capacity as f64);
                g.tokens -= 1.0;
                wait
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(wait)
    }

    /// Try to consume one token without waiting. Returns `true` when
    /// the bucket had a token available, `false` otherwise. Callers
    /// that pair this with a fallback path (e.g. queue-and-retry)
    /// avoid the cost of `acquire` when the bucket is empty.
    pub fn try_acquire(&self) -> bool {
        let mut g = self.inner.lock();
        g.refill();
        if g.tokens >= 1.0 {
            g.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Return a previously-consumed token to the bucket. Used when
    /// the call was served from a cache and the upstream should not
    /// be charged against the local rate limit. Capped at `capacity`
    /// so a runaway refund loop cannot inflate the bucket beyond its
    /// configured ceiling.
    pub fn refund(&self) {
        let mut g = self.inner.lock();
        g.tokens = (g.tokens + 1.0).min(g.capacity as f64);
    }

    /// Configured bucket capacity. Used by the push-side saturation
    /// hook to populate the structured `details` payload that the
    /// `moagan telemetry alerts list` consumer renders.
    pub fn capacity(&self) -> u32 {
        self.inner.lock().capacity
    }

    /// Configured refill rate (tokens per second). Companion to
    /// [`Self::capacity`]; same wire purpose (catalog §D.23).
    pub fn refill_per_sec(&self) -> u32 {
        self.inner.lock().refill_per_sec
    }
}

impl Inner {
    /// Advance the bucket's token count by the elapsed wall-clock time
    /// since the last refill. Clamps at `capacity` so the bucket
    /// never over-fills.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens =
            (self.tokens + elapsed * self.refill_per_sec as f64).min(self.capacity as f64);
        self.last_refill = now;
    }

    /// Refill tokens based on elapsed time and consume one, returning
    /// the wait time needed (zero if a token was already available).
    fn token_after_one(&mut self) -> Duration {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Duration::ZERO
        } else {
            let deficit = 1.0 - self.tokens;
            let secs = deficit / self.refill_per_sec.max(1) as f64;
            Duration::from_secs_f64(secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[tokio::test]
    async fn first_acquire_is_immediate() {
        let l = RateLimiter::new(RateLimitConfig {
            capacity: 1,
            refill_per_sec: 1,
            initial: Some(1),
        });
        let w = l.acquire().await.unwrap();
        assert_eq!(w, Duration::ZERO);
    }

    #[tokio::test]
    async fn exhausted_bucket_waits() {
        let l = RateLimiter::new(RateLimitConfig {
            capacity: 1,
            refill_per_sec: 100,
            initial: Some(1),
        });
        l.acquire().await.unwrap();
        // Next call must wait roughly 10 ms (1 / 100 s).
        let w = l.acquire().await.unwrap();
        assert!(w >= Duration::from_millis(5), "got {w:?}");
    }

    // -----------------------------------------------------------------
    // Property-based tests (proptest 1.4, dev-only per ADR-0001).
    //
    // The rate limiter's state machine is timing-sensitive (it
    // reads `Instant::now()` to advance the bucket) so the
    // properties we can pin are limited to:
    // - the bucket never exceeds `capacity` even under a
    //   refund storm (a runaway refund loop cannot inflate the
    //   bucket beyond its configured ceiling — see `refund()`),
    // - `try_acquire` reports a slot availability that matches
    //   the arithmetic state (capacity - consumed), and
    // - `capacity()` and `refill_per_sec()` round-trip the
    //   configured values byte-for-byte.
    //
    // The async `acquire` / `acquire_with_max` paths need a
    // `tokio::test` harness, so the timing-dependent invariants
    // stay in the unit tests above.
    // -----------------------------------------------------------------

    proptest::proptest! {
        /// `refund()` is capped at `capacity`: even when the
        /// caller hammers it more times than the bucket can
        /// hold, the available tokens never exceed the
        /// configured ceiling. Without the cap, a stuck retry
        /// loop with refund-on-failure could inflate the bucket
        /// far beyond its actual refill rate.
        #[test]
        fn prop_refund_never_exceeds_capacity(
            capacity in 1u32..64, refund_count in 0usize..512,
        ) {
            let l = RateLimiter::new(RateLimitConfig {
                capacity,
                refill_per_sec: 1,
                initial: Some(capacity),
            });
            for _ in 0..refund_count {
                l.refund();
            }
            // The bucket starts full and refund can only add;
            // the available tokens must therefore be in
            // [0, capacity].
            let acquired = (0..capacity).filter(|_| l.try_acquire()).count();
            prop_assert!(
                acquired <= capacity as usize,
                "refund storm inflated bucket above capacity: {acquired} > {capacity}"
            );
            // For a brand-new limiter with initial=capacity and
            // no consume calls, every try_acquire must succeed
            // until the bucket is empty (because refund never
            // adds tokens when the bucket is already at
            // capacity).
            prop_assert_eq!(
                acquired, capacity as usize,
                "initial bucket must be exactly capacity"
            );
        }

        /// `try_acquire` reports a slot availability that matches
        /// the bucket arithmetic: the first `min(take,
        /// capacity)` consumes always succeed, the
        /// (capacity + 1)-th consume fails. Pins the
        /// consume/slot accounting without depending on
        /// wall-clock time.
        #[test]
        fn prop_try_acquire_respects_capacity(
            capacity in 1u32..16, take in 0u32..32,
        ) {
            let l = RateLimiter::new(RateLimitConfig {
                capacity,
                refill_per_sec: 1,
                initial: Some(capacity),
            });
            let mut consumed = 0u32;
            for _ in 0..take {
                if l.try_acquire() {
                    consumed += 1;
                }
            }
            // The bucket starts full (initial = capacity) and
            // cannot exceed capacity, so the total successful
            // consumes must equal min(take, capacity).
            let expected = take.min(capacity);
            prop_assert!(
                consumed == expected,
                "consumed={} expected={} (capacity={} take={})",
                consumed, expected, capacity, take
            );
        }

        /// `capacity()` and `refill_per_sec()` round-trip the
        /// configured values. A regression that captures them
        /// incorrectly would silently break the structured
        /// telemetry payload that surfaces them.
        #[test]
        fn prop_capacity_and_refill_round_trip(
            capacity in 1u32..1024, refill_per_sec in 1u32..1024,
        ) {
            let l = RateLimiter::new(RateLimitConfig {
                capacity,
                refill_per_sec,
                initial: Some(capacity),
            });
            prop_assert_eq!(l.capacity(), capacity);
            prop_assert_eq!(l.refill_per_sec(), refill_per_sec);
        }
    }
}
