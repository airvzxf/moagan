//! Concurrency control. The `Parallelism` struct is a global semaphore
//! shared by every phase. Phases ask for permits; if more are asked
//! than `max_parallelism`, they wait.
//!
//! Compliance: T01-06 §6.2 ("min(solicitado, max_parallelism - en_uso)")
//! + 10-integrada-v0 §D.20 (parallelism runtime).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::Result;

/// Failure while acquiring a fixed number of permits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// The request exceeds the configured cap.
    TooManyPermits {
        /// Number requested.
        requested: usize,
        /// Configured cap.
        cap: usize,
    },
    /// The semaphore was closed.
    Closed,
}

/// Handle to the process-wide concurrency cap.
#[derive(Debug, Clone)]
pub struct Parallelism {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Total permits available.
    permits: usize,
    /// The semaphore itself.
    sem: Arc<Semaphore>,
    /// In-use count (mirror of `sem` for telemetry).
    in_use: Arc<AtomicUsize>,
}

impl Parallelism {
    /// Build a new parallelism cap.
    pub fn new(max_parallelism: usize) -> Self {
        let n = max_parallelism.max(1);
        tracing::info!(
            component = "parallelism",
            requested = max_parallelism,
            effective = n,
            "Parallelism::new building cap"
        );
        Self {
            inner: Arc::new(Inner {
                permits: n,
                sem: Arc::new(Semaphore::new(n)),
                in_use: Arc::new(AtomicUsize::new(0)),
            }),
        }
    }

    /// Acquire one permit. The returned guard releases on drop.
    pub async fn acquire(&self) -> Result<Permit> {
        tracing::trace!(
            component = "parallelism",
            cap = self.max(),
            in_use = self.in_use(),
            "Parallelism::acquire awaiting one permit"
        );
        let permit = self.inner.sem.clone().acquire_owned().await.map_err(|e| {
            tracing::error!(
                component = "parallelism",
                error = %e,
                "Parallelism::acquire: semaphore closed"
            );
            crate::Error::Cancelled(format!("semaphore closed: {e}"))
        })?;
        self.inner.in_use.fetch_add(1, Ordering::SeqCst);
        tracing::debug!(
            component = "parallelism",
            in_use = self.in_use(),
            "Parallelism::acquire granted one permit"
        );
        Ok(Permit {
            permit: Some(permit),
            in_use: self.inner.in_use.clone(),
        })
    }

    /// Acquire exactly `n` owned permits without clamping to the cap.
    pub async fn acquire_many_owned(
        &self,
        n: usize,
    ) -> std::result::Result<Vec<OwnedSemaphorePermit>, AcquireError> {
        tracing::debug!(
            component = "parallelism",
            requested = n,
            cap = self.max(),
            "Parallelism::acquire_many_owned enter"
        );
        if n == 0 {
            return Ok(Vec::new());
        }
        let cap = self.max();
        if n > cap {
            tracing::warn!(
                component = "parallelism",
                requested = n,
                cap,
                "Parallelism::acquire_many_owned: request exceeds cap"
            );
            return Err(AcquireError::TooManyPermits { requested: n, cap });
        }
        let mut permits = Vec::with_capacity(n);
        for _ in 0..n {
            let permit = self.inner.sem.clone().acquire_owned().await.map_err(|_| {
                tracing::error!(
                    component = "parallelism",
                    "Parallelism::acquire_many_owned: semaphore closed mid-acquire"
                );
                AcquireError::Closed
            })?;
            permits.push(permit);
        }
        Ok(permits)
    }

    /// Acquire up to `n` permits (clamped to the global cap). Returns
    /// the actual number acquired in a guard.
    pub async fn acquire_many(&self, n: usize) -> Result<PermitsGuard> {
        let want = n.min(self.inner.permits);
        tracing::debug!(
            component = "parallelism",
            requested = n,
            want,
            cap = self.max(),
            "Parallelism::acquire_many enter"
        );
        let mut permits = Vec::with_capacity(want);
        for _ in 0..want {
            let p = self.inner.sem.clone().acquire_owned().await.map_err(|e| {
                tracing::error!(
                    component = "parallelism",
                    error = %e,
                    "Parallelism::acquire_many: semaphore closed"
                );
                crate::Error::Cancelled(format!("semaphore closed: {e}"))
            })?;
            permits.push(p);
        }
        self.inner.in_use.fetch_add(want, Ordering::SeqCst);
        tracing::debug!(
            component = "parallelism",
            count = want,
            in_use = self.in_use(),
            "Parallelism::acquire_many granted"
        );
        Ok(PermitsGuard {
            permits,
            in_use: self.inner.in_use.clone(),
            count: want,
        })
    }

    /// Permits configured.
    pub fn max(&self) -> usize {
        self.inner.permits
    }

    /// Permits currently in use.
    pub fn in_use(&self) -> usize {
        self.inner.in_use.load(Ordering::SeqCst)
    }
}

impl Default for Parallelism {
    fn default() -> Self {
        Self::new(4)
    }
}

/// Single-permit guard. Released on drop.
pub struct Permit {
    /// Owned permit; held to keep the slot in the semaphore. Field
    /// name is deliberately non-underscore so the compiler does not
    /// consider it dead.
    #[allow(dead_code)]
    permit: Option<OwnedSemaphorePermit>,
    in_use: Arc<AtomicUsize>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.in_use.fetch_sub(1, Ordering::SeqCst);
        tracing::trace!(
            component = "parallelism",
            remaining = self.in_use.load(Ordering::SeqCst),
            "Permit dropped; in_use decremented"
        );
    }
}

/// Multi-permit guard.
pub struct PermitsGuard {
    /// Owned permits held by this guard.
    #[allow(dead_code)]
    permits: Vec<OwnedSemaphorePermit>,
    in_use: Arc<AtomicUsize>,
    count: usize,
}

impl PermitsGuard {
    /// Number of permits held.
    pub fn count(&self) -> usize {
        self.count
    }
}

impl Drop for PermitsGuard {
    fn drop(&mut self) {
        self.in_use.fetch_sub(self.count, Ordering::SeqCst);
        tracing::trace!(
            component = "parallelism",
            released = self.count,
            remaining = self.in_use.load(Ordering::SeqCst),
            "PermitsGuard dropped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_many_owned_zero() {
        let permits = Parallelism::new(2).acquire_many_owned(0).await.unwrap();
        assert!(permits.is_empty());
    }

    #[tokio::test]
    async fn acquire_many_owned_exceeds_cap_returns_error() {
        let err = Parallelism::new(2).acquire_many_owned(3).await.unwrap_err();
        assert_eq!(
            err,
            AcquireError::TooManyPermits {
                requested: 3,
                cap: 2
            }
        );
    }

    #[tokio::test]
    async fn acquire_many_owned_within_cap() {
        let permits = Parallelism::new(3).acquire_many_owned(2).await.unwrap();
        assert_eq!(permits.len(), 2);
    }

    #[tokio::test]
    async fn acquire_respects_cap() {
        let p = Parallelism::new(2);
        let _a = p.acquire().await.unwrap();
        let _b = p.acquire().await.unwrap();
        assert_eq!(p.in_use(), 2);
    }

    #[tokio::test]
    async fn drop_releases_permit() {
        let p = Parallelism::new(2);
        {
            let _a = p.acquire().await.unwrap();
            assert_eq!(p.in_use(), 1);
        }
        assert_eq!(p.in_use(), 0);
    }

    #[tokio::test]
    async fn acquire_many_clamps_to_cap() {
        let p = Parallelism::new(3);
        let g = p.acquire_many(10).await.unwrap();
        assert_eq!(g.count(), 3);
        assert_eq!(p.in_use(), 3);
    }
}
