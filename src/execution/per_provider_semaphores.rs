//! D.9.6: per-provider semaphores.
//!
//! Wraps arbitrary async operations with a per-provider semaphore
//! so concurrent calls to the same provider don't exceed its
//! capacity. The existing global `ParallelismPool` is orthogonal.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

/// Async semaphores keyed by provider name; each provider gets
/// its own independent concurrency cap.
pub struct PerProviderSemaphores {
    map: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl PerProviderSemaphores {
    /// Build an empty registry.
    pub fn new() -> Self {
        tracing::info!(
            component = "per_provider_semaphores",
            "PerProviderSemaphores::new building empty registry"
        );
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }
    /// Acquire `permits` slots for `provider`, lazily creating
    /// the semaphore on first use. Awaits until the slot is
    /// available.
    pub async fn acquire(&self, provider: &str, permits: usize) -> OwnedSemaphorePermit {
        let effective = permits.max(1);
        let sem = {
            let mut map = self.map.lock().await;
            let entry = map.entry(provider.to_string());
            let existed = matches!(entry, std::collections::hash_map::Entry::Occupied(_));
            let arc = entry
                .or_insert_with(|| Arc::new(Semaphore::new(effective)))
                .clone();
            if !existed {
                tracing::info!(
                    component = "per_provider_semaphores",
                    provider = %provider,
                    permits = effective,
                    "PerProviderSemaphores created new semaphore for provider"
                );
            } else {
                tracing::trace!(
                    component = "per_provider_semaphores",
                    provider = %provider,
                    requested = permits,
                    "PerProviderSemaphores reusing existing semaphore"
                );
            }
            arc
        };
        sem.acquire_owned().await.unwrap()
    }

    /// Remaining permits for `provider`, or `None` when no slot
    /// has been created yet (no `acquire` has run for that key).
    pub async fn available_permits(&self, provider: &str) -> Option<usize> {
        let map = self.map.lock().await;
        let result = map.get(provider).map(|s| s.available_permits());
        tracing::trace!(
            component = "per_provider_semaphores",
            provider = %provider,
            available = ?result,
            "PerProviderSemaphores::available_permits"
        );
        result
    }
}

impl Default for PerProviderSemaphores {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    #[test]
    fn per_provider_semaphores_acquires_for_each_provider() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let sem = PerProviderSemaphores::new();
            let _p1 = sem.acquire("minimax", 1).await;
            let _p2 = sem.acquire("openai_compat", 1).await;
            assert_eq!(sem.map.lock().await.len(), 2);
        });
    }

    #[test]
    fn per_provider_semaphores_zero_permits_becomes_one() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let sem = PerProviderSemaphores::new();
            let _p = sem.acquire("minimax", 0).await;
            let s = sem.map.lock().await;
            assert_eq!(s.get("minimax").unwrap().available_permits(), 0);
        });
    }

    #[test]
    fn per_provider_semaphores_blocks_at_capacity() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let sem = PerProviderSemaphores::new();
            let _p1 = sem.acquire("minimax", 1).await;
            // Second acquire would block; we don't await it here.
            let s = sem.map.lock().await;
            assert_eq!(s.get("minimax").unwrap().available_permits(), 0);
        });
    }
}
