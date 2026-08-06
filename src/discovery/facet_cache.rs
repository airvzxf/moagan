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
        match self.stale_at_unix {
            None => true,
            Some(t) => t > now_unix,
        }
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
        Self {
            root: root.into(),
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
        if !path.exists() {
            self.misses.fetch_add(1, Ordering::Relaxed);
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
        Ok(Some(entry.facet_list))
    }

    /// Persist `list` under its `cache_key`. Existing entries are
    /// overwritten. Returns the path that was written.
    pub fn store(&self, list: &FacetList) -> Result<PathBuf> {
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
        Ok(path)
    }

    /// Invalidate a single cache entry by its key.
    pub fn invalidate(&self, cache_key: &str) -> Result<()> {
        let path = self.path_for(cache_key);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Number of cache entries currently on disk (used by the
    /// smoke tests to confirm the cache is being exercised).
    pub fn count(&self) -> Result<usize> {
        if !self.root.exists() {
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
        Ok(n)
    }

    /// Snapshot the cache effectiveness counters and the on-disk
    /// entry count.
    ///
    /// `hits` and `misses` are cumulative since the cache handle
    /// was constructed (clones share the same counters). `entries`
    /// is recomputed on demand via [`FacetCache::count`].
    pub fn stats(&self) -> FacetCacheStats {
        FacetCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: self.count().unwrap_or(0),
        }
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
}
