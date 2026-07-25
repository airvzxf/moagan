//! Token-bucket rate limiter. Per-provider, in-memory.
//!
//! Compliance: 10-integrada-v0 §D.15 (rate_limiter). Replaces the
//! `governor` crate (rejected in catalog §C).

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::error::Result;

/// Configuration for a token bucket.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum tokens stored.
    pub capacity: u32,
    /// Tokens added per second.
    pub refill_per_sec: u32,
    /// Initial token count (defaults to `capacity`).
    pub initial: Option<u32>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            capacity: 60,
            refill_per_sec: 4,
            initial: None,
        }
    }
}

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
    /// Longest queued wait so the caller can `tokio::time::sleep` it.
    last_wait: Duration,
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
                last_wait: Duration::ZERO,
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

    /// Latest recorded wait time. Useful for telemetry.
    pub fn last_wait(&self) -> Duration {
        self.inner.lock().last_wait
    }
}

impl Inner {
    /// Refill tokens based on elapsed time and consume one, returning
    /// the wait time needed (zero if a token was already available).
    fn token_after_one(&mut self) -> Duration {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens =
            (self.tokens + elapsed * self.refill_per_sec as f64).min(self.capacity as f64);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            self.last_wait = Duration::ZERO;
            Duration::ZERO
        } else {
            let deficit = 1.0 - self.tokens;
            let secs = deficit / self.refill_per_sec.max(1) as f64;
            let wait = Duration::from_secs_f64(secs);
            self.last_wait = wait;
            wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
