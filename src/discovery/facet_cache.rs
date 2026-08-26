//! Persistent facet cache.
//!
//! Cross-run cache for `FacetList` payloads. The cache key is
//! `sha256(brief + category_id)` (already computed by
//! `crate::discovery::facet::cache_key`). Entries are persisted to
//! `<MOAGAN_HOME>/cache/facets/<key>.json` so a second run with
//! the same brief and category id skips the LLM call.
//!
//! Per V4 §6.8 ("Caché por hash de (brief, categoría)") and catalog
//! 10-integrada-v0 decision D.13.13. Default TTL is 7 days; the
//! caller can disable TTL with `None` or override per-entry by
//! computing a custom `stale_at_unix` field.
//!
//! The cache is best-effort: a corrupted entry is treated as a miss
//! (logged via tracing) rather than propagated as an error, so a
//! single bad entry cannot block a run.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::FacetList;
use crate::error::Result;

/// Default TTL when the caller does not specify one. 7 days matches
/// the catalog decision D.6.3 default for the LLM cache.
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Schema version for on-disk entries. Bumping it forces the
/// readers to ignore pre-existing entries.
const SCHEMA_VERSION: &str = "v1";

/// On-disk representation of a cached `FacetList`. Wraps the
/// domain type with a `stored_at` timestamp and an optional
/// `stale_at_unix` so the cache can implement TTL eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Schema version (always `"v1"` for v0.2).
    pub schema_version: String,
    /// When the entry was stored.
    pub stored_at: DateTime<Utc>,
    /// Optional stale-at timestamp (unix seconds). `None` means
    /// "never expires".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_at_unix: Option<i64>,
    /// The cached facet list.
    pub facet_list: FacetList,
}

impl CacheEntry {
    /// True if the entry is still fresh at `now_unix`.
    pub fn is_fresh(&self, now_unix: i64) -> bool {
        let v = match self.stale_at_unix {
            None => true,
            Some(t) => t > now_unix,
        };
        tracing::trace!(now_unix, stale_at_unix = ?self.stale_at_unix, fresh = v, "CacheEntry::is_fresh");
        v
    }
}

/// Handle to the persistent facet cache.
#[derive(Debug, Clone)]
pub struct FacetCache {
    /// Root directory for cache entries (`<MOAGAN_HOME>/cache/facets`).
    root: PathBuf,
    /// TTL applied to every new entry. `None` disables expiry.
    ttl_secs: Option<u64>,
    /// Cumulative counter of cache hits (lookups that returned a
    /// fresh `FacetList`). Wrapped in `Arc` so clones share the
    /// counter across tasks.
    hits: Arc<AtomicU64>,
    /// Cumulative counter of cache misses (lookups that returned
    /// `None` because the entry was missing, stale, malformed, or
    /// schema-mismatched).
    misses: Arc<AtomicU64>,
}

/// Read-only snapshot of cache effectiveness counters.
///
/// Returned by [`FacetCache::stats`]. The `entries` count is
/// recomputed on demand from disk; `hits` and `misses` are
/// monotonically increasing counters held inside the cache
/// handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FacetCacheStats {
    /// Number of lookups that returned a fresh cached `FacetList`.
    pub hits: u64,
    /// Number of lookups that did not return a fresh entry.
    pub misses: u64,
    /// Number of cache entries currently persisted on disk.
    pub entries: usize,
}

impl FacetCache {
    /// Open a cache rooted at `root`. The directory is created
    /// lazily on first `store`.
    pub fn new(root: impl Into<PathBuf>, ttl_secs: Option<u64>) -> Self {
        let root: PathBuf = root.into();
        tracing::debug!(
            root = %root.display(),
            ttl_secs = ?ttl_secs,
            "FacetCache::new"
        );
        Self {
            root,
            ttl_secs,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Path to the JSON file backing `cache_key`.
    pub fn path_for(&self, cache_key: &str) -> PathBuf {
        self.root.join(format!("{cache_key}.json"))
    }

    /// Look up a facet list by its cache key. Returns `Ok(None)` on
    /// a cache miss, stale entry, or corrupted file (with a tracing
    /// warn so the operator can debug). Returns `Ok(Some(list))` on
    /// a hit.
    pub fn lookup(&self, cache_key: &str) -> Result<Option<FacetList>> {
        self.lookup_at(cache_key, &self.root, Utc::now().timestamp())
    }

    /// Same as `lookup` but with explicit `now_unix` and `root` so
    /// tests can drive both the freshness check and the storage
    /// path.
    pub fn lookup_at(
        &self,
        cache_key: &str,
        root: &Path,
        now_unix: i64,
    ) -> Result<Option<FacetList>> {
        let path = root.join(format!("{cache_key}.json"));
        tracing::debug!(cache_key, "FacetCache::lookup_at");
        if !path.exists() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(cache_key, "facet cache: file absent");
            return Ok(None);
        }
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    cache_key,
                    error = %e,
                    "facet cache read failed; treating as miss"
                );
                self.misses.fetch_add(1, Ordering::Relaxed);
                return Ok(None);
            }
        };
        let entry: CacheEntry = match serde_json::from_str(&raw) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    cache_key,
                    error = %e,
                    "facet cache entry malformed; treating as miss"
                );
                self.misses.fetch_add(1, Ordering::Relaxed);
                return Ok(None);
            }
        };
        if entry.schema_version != SCHEMA_VERSION {
            tracing::warn!(
                cache_key,
                schema = entry.schema_version,
                "facet cache schema mismatch; treating as miss"
            );
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        if !entry.is_fresh(now_unix) {
            tracing::debug!(cache_key, "facet cache entry stale");
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(
            cache_key,
            stored_at = %entry.stored_at,
            "facet cache hit"
        );
        Ok(Some(entry.facet_list))
    }

    /// Look up a facet list by its cache key and, on miss, run
    /// `compute_fn`, persist the result, and return it. This is
    /// the canonical replacement for the inline
    /// `lookup → compute → store` pattern that
    /// [`crate::phases::discover_facet`] used to spell out
    /// (catalog decision D.13.13).
    ///
    /// Cache-hit path:
    /// 1. `lookup` returns `Some(list)`.
    /// 2. `compute_fn` is **not** invoked — the LLM call that
    ///    the phase would otherwise make is skipped.
    /// 3. The cached list is returned verbatim.
    ///
    /// Cache-miss path:
    /// 1. `lookup` returns `None`.
    /// 2. `compute_fn()` is awaited; the resulting `FacetList`
    ///    must already carry its own `cache_key` (i.e. it was
    ///    built with `FacetList::from_triples` or an equivalent
    ///    helper).
    /// 3. `store` is best-effort: a disk write failure is
    ///    surfaced as a tracing warning (so the operator can
    ///    debug a stale cache) and the freshly-computed list is
    ///    still returned. The cache must never poison a run.
    ///
    /// `compute_fn` failures propagate as `Err`; the caller is
    /// expected to handle them (e.g. by skipping the cluster).
    /// `&mut self` is intentional: it serialises
    /// `get_or_compute` calls on the same instance so two
    /// concurrent tasks racing on the same key can never both
    /// compute and clobber each other's store. Clones of the
    /// cache remain independent because each task gets its own
    /// `FacetCache` clone (the `Arc<AtomicU64>` counters are
    /// the only shared state).
    pub async fn get_or_compute<F, Fut>(
        &mut self,
        cache_key: &str,
        compute_fn: F,
    ) -> Result<FacetList>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<FacetList>>,
    {
        tracing::debug!(cache_key, "FacetCache::get_or_compute (async)");
        if let Some(cached) = self.lookup(cache_key)? {
            tracing::info!(
                cache_key,
                facets = cached.facets.len(),
                "facet cache hit; LLM skipped"
            );
            return Ok(cached);
        }
        tracing::debug!(cache_key, "facet cache miss; invoking compute_fn");
        let list = compute_fn().await?;
        tracing::info!(
            cache_key,
            facets = list.facets.len(),
            "facet cache miss; computed"
        );
        if let Err(e) = self.store(&list) {
            tracing::warn!(
                cache_key,
                error = %e,
                "facet cache store failed during get_or_compute; continuing without persistence"
            );
        }
        Ok(list)
    }

    /// Persist `list` under its `cache_key`. Existing entries are
    /// overwritten. Returns the path that was written.
    pub fn store(&self, list: &FacetList) -> Result<PathBuf> {
        tracing::debug!(
            cache_key = %list.cache_key,
            facets = list.facets.len(),
            "FacetCache::store"
        );
        let now = Utc::now();
        let stale_at_unix = self.ttl_secs.map(|t| now.timestamp() + t as i64);
        let entry = CacheEntry {
            schema_version: SCHEMA_VERSION.into(),
            stored_at: now,
            stale_at_unix,
            facet_list: list.clone(),
        };
        let path = self.path_for(&list.cache_key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_vec_pretty(&entry)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &raw)?;
        fs::rename(&tmp, &path)?;
        tracing::trace!(path = %path.display(), "FacetCache::store ok");
        Ok(path)
    }

    /// Invalidate a single cache entry by its key.
    pub fn invalidate(&self, cache_key: &str) -> Result<()> {
        let path = self.path_for(cache_key);
        tracing::debug!(
            cache_key,
            path = %path.display(),
            exists = path.exists(),
            "FacetCache::invalidate"
        );
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Number of cache entries currently on disk (used by the
    /// smoke tests to confirm the cache is being exercised).
    pub fn count(&self) -> Result<usize> {
        if !self.root.exists() {
            tracing::trace!("FacetCache::count: root absent");
            return Ok(0);
        }
        let mut n = 0usize;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                n += 1;
            }
        }
        tracing::trace!(root = %self.root.display(), n, "FacetCache::count");
        Ok(n)
    }

    /// Snapshot the cache effectiveness counters and the on-disk
    /// entry count.
    ///
    /// `hits` and `misses` are cumulative since the cache handle
    /// was constructed (clones share the same counters). `entries`
    /// is recomputed on demand via [`FacetCache::count`].
    pub fn stats(&self) -> FacetCacheStats {
        let s = FacetCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: self.count().unwrap_or(0),
        };
        tracing::trace!(
            hits = s.hits,
            misses = s.misses,
            entries = s.entries,
            "FacetCache::stats"
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_list(brief: &str, cat: &str) -> FacetList {
        FacetList::from_triples(
            cat,
            "cluster_01",
            brief,
            1_700_000_000,
            vec![("Data Flows".into(), "flows".into(), true)],
        )
    }

    #[test]
    fn lookup_returns_none_on_empty_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        let hit = cache.lookup(&list.cache_key).unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn store_then_lookup_returns_same_list() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();
        let hit = cache.lookup(&list.cache_key).unwrap();
        assert!(hit.is_some());
        let back = hit.unwrap();
        assert_eq!(back.facets.len(), 1);
        assert_eq!(back.facets[0].id, "data-flows");
        assert_eq!(back.category_id, "cat_01");
    }

    #[test]
    fn stale_entry_is_miss() {
        let tmp = tempfile::tempdir().unwrap();
        // Manually craft a cache entry with `stale_at_unix` set
        // before the lookup time so the test is independent of
        // wall-clock timing.
        let list = mk_list("brief", "cat_01");
        let entry = CacheEntry {
            schema_version: SCHEMA_VERSION.into(),
            stored_at: Utc::now(),
            stale_at_unix: Some(1_700_000_060),
            facet_list: list.clone(),
        };
        let path = tmp.path().join(format!("{}.json", list.cache_key));
        std::fs::write(&path, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

        let cache = FacetCache::new(tmp.path(), Some(60));
        // 1 hour past the synthetic stale_at_unix.
        let now = 1_700_000_060 + 3600;
        let hit = cache.lookup_at(&list.cache_key, tmp.path(), now).unwrap();
        assert!(hit.is_none(), "stale entry must miss");
    }

    #[test]
    fn fresh_entry_under_ttl_is_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();

        // 1 second past TTL boundary but inside the 60s window.
        let future = 1_700_000_000 + 60;
        let hit = cache
            .lookup_at(&list.cache_key, tmp.path(), future)
            .unwrap();
        assert!(hit.is_some());
    }

    #[test]
    fn store_with_no_ttl_is_immortal() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), None);
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();

        let future = i64::MAX / 2;
        let hit = cache
            .lookup_at(&list.cache_key, tmp.path(), future)
            .unwrap();
        assert!(hit.is_some(), "TTL=None means immortal");
    }

    #[test]
    fn corrupted_entry_is_miss_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let path = cache.path_for("bad");
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(&path, "not json").unwrap();

        let hit = cache.lookup("bad").unwrap();
        assert!(hit.is_none(), "corrupted entry must miss");
    }

    #[test]
    fn missing_entry_is_miss_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let hit = cache.lookup("does-not-exist").unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn schema_mismatch_is_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let raw = serde_json::json!({
            "schema_version": "v999",
            "stored_at": "2026-01-01T00:00:00Z",
            "facet_list": {
                "category_id": "cat_01",
                "cluster_id": "cluster_01",
                "facets": [],
                "cache_key": "x",
                "created_unix": 0,
                "schema_version": "v1"
            }
        });
        std::fs::write(
            cache.path_for("x"),
            serde_json::to_vec_pretty(&raw).unwrap(),
        )
        .unwrap();

        let hit = cache.lookup("x").unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn invalidate_removes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();
        assert_eq!(cache.count().unwrap(), 1);
        cache.invalidate(&list.cache_key).unwrap();
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn count_reports_zero_on_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn count_increments_with_each_store() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        cache.store(&mk_list("brief-a", "cat_01")).unwrap();
        cache.store(&mk_list("brief-a", "cat_02")).unwrap();
        cache.store(&mk_list("brief-b", "cat_01")).unwrap();
        assert_eq!(cache.count().unwrap(), 3);
    }

    #[test]
    fn path_for_matches_cache_key() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        assert_eq!(
            cache
                .path_for(&list.cache_key)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
            format!("{}.json", list.cache_key)
        );
    }

    #[test]
    fn default_ttl_is_one_week() {
        assert_eq!(DEFAULT_TTL_SECS, 7 * 24 * 60 * 60);
    }

    #[test]
    fn schema_version_constant_is_v1() {
        assert_eq!(SCHEMA_VERSION, "v1");
    }

    #[test]
    fn cache_entry_fresh_with_no_stale_at() {
        let entry = CacheEntry {
            schema_version: SCHEMA_VERSION.into(),
            stored_at: Utc::now(),
            stale_at_unix: None,
            facet_list: FacetList::default(),
        };
        assert!(entry.is_fresh(i64::MAX));
    }

    #[test]
    fn cache_entry_stale_when_stale_at_past() {
        let entry = CacheEntry {
            schema_version: SCHEMA_VERSION.into(),
            stored_at: Utc::now(),
            stale_at_unix: Some(100),
            facet_list: FacetList::default(),
        };
        assert!(!entry.is_fresh(200));
        assert!(entry.is_fresh(50));
    }

    #[test]
    fn facet_cache_stats_initial_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.entries, 0);
    }

    #[test]
    fn facet_cache_stats_records_hits_and_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();

        let miss_stats = cache.stats();
        assert_eq!(miss_stats.hits, 0);
        assert_eq!(miss_stats.misses, 0, "stores don't touch counters");
        assert_eq!(miss_stats.entries, 1);

        assert!(cache.lookup(&list.cache_key).unwrap().is_some());
        assert!(cache.lookup(&list.cache_key).unwrap().is_some());
        assert!(cache.lookup("never-stored").unwrap().is_none());

        let populated = cache.stats();
        assert_eq!(populated.hits, 2);
        assert_eq!(populated.misses, 1);
        assert_eq!(populated.entries, 1);
    }

    #[test]
    fn facet_cache_stats_counts_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FacetCache::new(tmp.path(), Some(60));
        cache.store(&mk_list("brief-a", "cat_01")).unwrap();
        cache.store(&mk_list("brief-a", "cat_02")).unwrap();
        cache.store(&mk_list("brief-b", "cat_01")).unwrap();

        let stats = cache.stats();
        assert_eq!(stats.entries, 3);
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
    }

    /// `get_or_compute` returns the cached list on hit and does
    /// NOT invoke `compute_fn`. The compute counter (callers
    /// track it externally) is the canonical signal that the LLM
    /// was skipped.
    #[tokio::test]
    async fn get_or_compute_returns_cached_value_on_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();

        let compute_calls = std::sync::atomic::AtomicUsize::new(0);
        let counter = &compute_calls;
        let result = cache
            .get_or_compute(&list.cache_key, || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { panic!("compute_fn must not run on a cache hit") }
            })
            .await
            .unwrap();
        assert_eq!(result.category_id, "cat_01");
        assert_eq!(result.facets.len(), 1);
        assert_eq!(compute_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// `get_or_compute` runs `compute_fn` on a miss, persists the
    /// result, and returns it. A subsequent `lookup` sees the
    /// freshly-stored entry as a hit.
    #[tokio::test]
    async fn get_or_compute_runs_compute_fn_on_miss_and_stores() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");

        let result = cache
            .get_or_compute(&list.cache_key, || async { Ok(list.clone()) })
            .await
            .unwrap();
        assert_eq!(result.category_id, "cat_01");
        assert_eq!(
            cache.count().unwrap(),
            1,
            "store must persist the computed list"
        );
        let second = cache.lookup(&list.cache_key).unwrap();
        assert!(second.is_some(), "post-store lookup must hit");
    }

    /// `compute_fn` failures propagate as `Err` and the cache
    /// stays empty (no half-written entry). This is the
    /// canonical "compute failed, don't poison the cache"
    /// contract.
    #[tokio::test]
    async fn get_or_compute_propagates_compute_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = FacetCache::new(tmp.path(), Some(60));
        let key = "fictional-key";

        let err = cache
            .get_or_compute(key, || async {
                Err(crate::error::Error::InvalidState("boom".into()))
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(cache.count().unwrap(), 0, "failed compute must not store");
    }

    /// The cache is best-effort: a `store` failure is logged and
    /// the freshly-computed list is still returned. The phase
    /// relies on this so a disk-full cannot abort a run.
    /// Simulating a portable store failure is tricky (chmod
    /// tricks are root-bypassable); we use a `cache` rooted
    /// under a regular file so `create_dir_all` inside `store`
    /// deterministically fails on every platform.
    #[tokio::test]
    async fn get_or_compute_swallows_store_failure() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a regular file and point the cache at a path
        // *inside* it — `store` will try to `create_dir_all`
        // the parent of the JSON file (which exists as a file,
        // not a directory), so the call fails on every
        // platform.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let cache_root = blocker.join("cache");
        let mut cache = FacetCache::new(&cache_root, Some(60));
        let list = mk_list("brief", "cat_01");

        let result = cache
            .get_or_compute(&list.cache_key, || async { Ok(list.clone()) })
            .await
            .unwrap();
        // The freshly-computed list must come back even though
        // the store failed.
        assert_eq!(result.category_id, "cat_01");
        assert_eq!(result.facets.len(), 1);
    }

    /// `get_or_compute` is the documented replacement for the
    /// inline `lookup → compute → store` triple; the unit
    /// invariants we care about are: hit-skip-compute,
    /// miss-compute-and-store, compute-error-propagates,
    /// store-error-swallowed. The stats counters should
    /// reflect exactly one lookup per `get_or_compute` call.
    #[tokio::test]
    async fn get_or_compute_records_exactly_one_lookup_per_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = FacetCache::new(tmp.path(), Some(60));
        let list = mk_list("brief", "cat_01");
        cache.store(&list).unwrap();

        let before = cache.stats();
        let _ = cache
            .get_or_compute(&list.cache_key, || async { Ok(list.clone()) })
            .await
            .unwrap();
        let after = cache.stats();
        assert_eq!(after.hits, before.hits + 1);
        assert_eq!(after.misses, before.misses);
    }
}
