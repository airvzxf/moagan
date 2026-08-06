//! D.19.19 + D.19.20: ProviderPool with round-robin pick and
//! allow_paused gate.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[async_trait::async_trait]
pub trait ProviderPoolEntry: Send + Sync {
    async fn is_available(&self) -> bool;
}

pub struct ProviderPool {
    entries: Vec<Arc<dyn ProviderPoolEntry>>,
    counter: AtomicUsize,
}

impl ProviderPool {
    pub fn new(entries: Vec<Arc<dyn ProviderPoolEntry>>) -> Self {
        Self { entries, counter: AtomicUsize::new(0) }
    }
    pub fn round_robin(&self) -> Option<usize> {
        if self.entries.is_empty() { return None; }
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.entries.len();
        Some(idx)
    }
    /// D.19.20: pick with allow_paused gate.
    pub async fn pick(&self, allow_paused: bool) -> Option<usize> {
        let start = self.round_robin()?;
        let n = self.entries.len();
        for i in 0..n {
            let idx = (start + i) % n;
            let entry = &self.entries[idx];
            if allow_paused || entry.is_available().await {
                return Some(idx);
            }
        }
        None
    }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockEntry { available: AtomicBool }
    #[async_trait::async_trait]
    impl ProviderPoolEntry for MockEntry {
        async fn is_available(&self) -> bool { self.available.load(Ordering::Relaxed) }
    }

    #[test]
    fn provider_pool_empty_round_robin_returns_none() {
        let pool = ProviderPool::new(vec![]);
        assert!(pool.round_robin().is_none());
        assert!(pool.is_empty());
    }

    #[test]
    fn provider_pool_round_robin_distributes() {
        let pool = ProviderPool::new(vec![
            Arc::new(MockEntry { available: AtomicBool::new(true) }),
            Arc::new(MockEntry { available: AtomicBool::new(true) }),
            Arc::new(MockEntry { available: AtomicBool::new(true) }),
        ]);
        assert_eq!(pool.round_robin(), Some(0));
        assert_eq!(pool.round_robin(), Some(1));
        assert_eq!(pool.round_robin(), Some(2));
        assert_eq!(pool.round_robin(), Some(0));
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn provider_pool_pick_skips_unavailable_when_not_allow_paused() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = ProviderPool::new(vec![
                Arc::new(MockEntry { available: AtomicBool::new(false) }),
                Arc::new(MockEntry { available: AtomicBool::new(true) }),
                Arc::new(MockEntry { available: AtomicBool::new(false) }),
            ]);
            // round_robin starts at 0; pick with allow_paused=false
            // skips 0 and 2 (paused), returns 1.
            assert_eq!(pool.pick(false).await, Some(1));
        });
    }

    #[test]
    fn provider_pool_pick_returns_none_when_all_paused() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = ProviderPool::new(vec![
                Arc::new(MockEntry { available: AtomicBool::new(false) }),
                Arc::new(MockEntry { available: AtomicBool::new(false) }),
            ]);
            assert!(pool.pick(false).await.is_none());
        });
    }

    #[test]
    fn provider_pool_pick_allow_paused_returns_round_robin() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let pool = ProviderPool::new(vec![
                Arc::new(MockEntry { available: AtomicBool::new(false) }),
                Arc::new(MockEntry { available: AtomicBool::new(false) }),
            ]);
            assert_eq!(pool.pick(true).await, Some(0)); // accepts even paused
        });
    }
}
