//! LLM cache. Idempotent lookups keyed by BLAKE3 over the canonical
//! (role, phase, provider, model, request). On miss, the caller hands
//! back the response and a sidecar JSON gets written atomically.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::writer::AtomicWriter;
use crate::error::{Error, Result};
use crate::ids::canonical_hash;
use crate::llm::prompts::prompt_set_hash;

use super::wire::{Request, Response, Usage};

mod sharded;

/// Cache configuration.
#[derive(Debug, Clone, Default)]
pub struct CacheConfig {
    /// Subdirectory under the run directory (`cache/llm`).
    pub root: PathBuf,
    /// Enable cross-run cache (writes to `<MOAGAN_HOME>/cache/llm`).
    pub cross_run: bool,
    /// Skip the request from being cached at all (e.g. for entropy).
    pub no_store: bool,
    /// Optional time-to-live. When `Some(n)`, a stored entry is
    /// considered stale `n` seconds after `created_unix` and `lookup`
    /// returns `None` for it. `None` means "never expires" (matching
    /// the v0.1 behaviour: cache hits were unbounded).
    ///
    /// Stale entries are **not** deleted on miss — they are only
    /// ignored. A future compaction pass (or the LRU eviction in
    /// `CacheConfig.max_bytes`) is what actually reclaims disk space.
    pub ttl_secs: Option<u64>,
}

/// Single cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Schema version.
    pub schema_version: String,
    /// Canonical hash key.
    pub cache_key: String,
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Stored response.
    pub response: Response,
    /// Stored usage breakdown (input/output/cache_read/cache_creation).
    /// Hydrated into `response.usage` and reused for telemetry on hit.
    pub usage: Usage,
    /// Created unix seconds.
    pub created_unix: i64,
    /// Optional stale-at unix seconds. When `Some(t)` and `t <= now`,
    /// the entry is treated as a miss by `lookup`. Absent when the
    /// TTL config was `None` at store time; absent also on entries
    /// written by an older binary that pre-dates the TTL feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_at_unix: Option<i64>,
}

impl CacheEntry {
    /// Current schema version.
    pub const SCHEMA_VERSION: &'static str = "v1";

    /// True if the entry is still fresh at `now_unix`. An entry is
    /// fresh when either:
    /// - it has no `stale_at_unix` (TTL was disabled when stored), or
    /// - `stale_at_unix > now_unix`.
    pub fn is_fresh(&self, now_unix: i64) -> bool {
        match self.stale_at_unix {
            None => true,
            Some(t) => t > now_unix,
        }
    }
}

/// Cache handle.
#[derive(Debug, Clone)]
pub struct Cache {
    config: CacheConfig,
}

impl Cache {
    /// Build a new cache rooted at `root`.
    pub fn new(config: CacheConfig) -> Self {
        Self { config }
    }

    /// Read-only access to the cache config (used by the run context
    /// `Debug` impl to surface the cache root without exposing the
    /// full config).
    pub fn config_for_debug(&self) -> &CacheConfig {
        &self.config
    }

    /// Produce the canonical cache key for `req`.
    pub fn cache_key(req: &Request, provider: &str, model: &str) -> String {
        let prompt_set_hash = prompt_set_hash();
        canonical_hash(&[
            "role",
            req.role.as_str(),
            "provider",
            provider,
            "model",
            model,
            "system",
            &req.system,
            "user",
            &req.user,
            "max_tokens",
            &req.max_tokens.to_string(),
            "temperature",
            &req.temperature.map(|t| t.to_string()).unwrap_or_default(),
            "top_p",
            &req.top_p.map(|t| t.to_string()).unwrap_or_default(),
            "prompt_set_hash",
            &prompt_set_hash,
        ])
    }

    /// Look up an entry. Returns `None` on miss.
    ///
    /// A miss is returned when:
    /// - the cache is disabled (`no_store`),
    /// - the shard file does not exist,
    /// - the entry cannot be decoded, or
    /// - the entry exists but its `stale_at_unix` is in the past
    ///   (`CacheConfig.ttl_secs` is set and the entry is older than
    ///   the TTL).
    ///
    /// Reads the JSON file directly without consulting the
    /// `AtomicWriter` sidecar: the sidecar's `sealed_at_unix` field
    /// changes every second, so re-reading more than one second after
    /// writing would otherwise spuriously fail with `MetaMismatch`.
    /// The cache is a perf optimisation, not a tamper-evidence store,
    /// so the sidecar check is unnecessary here.
    pub fn lookup(&self, cache_key: &str) -> Result<Option<CacheEntry>> {
        if self.config.no_store {
            return Ok(None);
        }
        let path = self.path_for(cache_key);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&path).map_err(|e| Error::Cache(format!("read {path:?}: {e}")))?;
        let entry: CacheEntry = serde_json::from_slice(&raw)
            .map_err(|e| Error::Cache(format!("decode {path:?}: {e}")))?;
        if !entry.is_fresh(crate::time::now_unix_secs()) {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Persist an entry atomically.
    ///
    /// When `CacheConfig.ttl_secs` is set, the entry's `stale_at_unix`
    /// is set to `created_unix + ttl_secs`. When `ttl_secs` is `None`,
    /// `stale_at_unix` is left as `None` and the entry never expires
    /// (matches v0.1 behaviour).
    pub fn store(
        &self,
        cache_key: &str,
        provider: &str,
        model: &str,
        resp: &Response,
    ) -> Result<()> {
        if self.config.no_store {
            return Ok(());
        }
        let created_unix = crate::time::now_unix_secs();
        let stale_at_unix = self
            .config
            .ttl_secs
            .map(|ttl| created_unix.saturating_add(ttl as i64));
        let entry = CacheEntry {
            schema_version: CacheEntry::SCHEMA_VERSION.to_owned(),
            cache_key: cache_key.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            response: resp.clone(),
            usage: resp.usage.clone(),
            created_unix,
            stale_at_unix,
        };
        let bytes = serde_json::to_vec(&entry).map_err(|e| Error::Cache(format!("encode: {e}")))?;
        let path = self.path_for(cache_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Cache(format!("mkdir {parent:?}: {e}")))?;
        }
        AtomicWriter::new().write(&path, &bytes)?;
        Ok(())
    }

    fn path_for(&self, cache_key: &str) -> PathBuf {
        sharded::path_for(&self.config.root, cache_key)
    }

    /// Build directory under `root` for the given key.
    pub fn ensure_dir(root: &Path) -> Result<()> {
        std::fs::create_dir_all(root).map_err(|e| Error::Cache(format!("mkdir {root:?}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::role::Role;

    fn req(system: &str, user: &str) -> Request {
        Request {
            role: Role::Intake,
            model: "m".into(),
            system: system.into(),
            user: user.into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        let k1 = Cache::cache_key(&req("s", "u"), "mock", "m");
        let k2 = Cache::cache_key(&req("s", "u"), "mock", "m");
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_differs_by_inputs() {
        let k1 = Cache::cache_key(&req("s", "u"), "mock", "m");
        let k2 = Cache::cache_key(&req("s", "different"), "mock", "m");
        assert_ne!(k1, k2);
    }

    #[test]
    fn store_and_lookup_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        let key = "abc12345";
        let resp = Response {
            text: "hello".into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                cache_read: 0,
                cache_creation: 0,
            },
        };
        cache.store(key, "mock", "m", &resp).unwrap();
        let entry = cache.lookup(key).unwrap().unwrap();
        assert_eq!(entry.response.text, "hello");
        assert_eq!(entry.provider, "mock");
        assert_eq!(entry.usage.input_tokens, 11);
        assert_eq!(entry.usage.output_tokens, 7);
    }

    #[test]
    fn lookup_after_one_second_does_not_fail_with_meta_mismatch() {
        // Regression: Cache used to call `AtomicWriter::read_with_meta`,
        // whose sidecar `sealed_at_unix` field changes every second.
        // After the wall clock crossed a second boundary, re-reading
        // the cache spuriously raised `IoError::MetaMismatch`. The
        // fix: skip the sidecar check for cache reads.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        let resp = Response {
            text: "ok".into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage::default(),
        };
        cache.store("k", "mock", "m", &resp).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let entry = cache.lookup("k").unwrap().expect("hit");
        assert_eq!(entry.response.text, "ok");
    }

    #[test]
    fn lookup_returns_none_on_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        let entry = cache.lookup("missing").unwrap();
        assert!(entry.is_none());
    }

    #[test]
    fn entry_is_fresh_when_stale_at_is_none() {
        // TTL disabled at store time -> stale_at_unix = None. The entry
        // must be fresh at any wall-clock time.
        let entry = CacheEntry {
            schema_version: "v1".into(),
            cache_key: "k".into(),
            provider: "mock".into(),
            model: "m".into(),
            response: Response {
                text: "x".into(),
                finish_reason: Some("end_turn".into()),
                truncated: false,
                usage: Usage::default(),
            },
            usage: Usage::default(),
            created_unix: 0,
            stale_at_unix: None,
        };
        assert!(entry.is_fresh(0));
        assert!(entry.is_fresh(i64::MAX));
    }

    #[test]
    fn entry_is_fresh_only_before_stale_at() {
        // Boundary semantics: fresh strictly before `stale_at_unix`,
        // stale at and after.
        let entry = CacheEntry {
            schema_version: "v1".into(),
            cache_key: "k".into(),
            provider: "mock".into(),
            model: "m".into(),
            response: Response {
                text: "x".into(),
                finish_reason: Some("end_turn".into()),
                truncated: false,
                usage: Usage::default(),
            },
            usage: Usage::default(),
            created_unix: 100,
            stale_at_unix: Some(200),
        };
        assert!(entry.is_fresh(199));
        assert!(!entry.is_fresh(200));
        assert!(!entry.is_fresh(201));
    }

    #[test]
    fn store_with_ttl_sets_stale_at_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ttl_secs: Some(60),
            ..Default::default()
        });
        let resp = Response {
            text: "ok".into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage::default(),
        };
        let before = crate::time::now_unix_secs();
        cache.store("k", "mock", "m", &resp).unwrap();
        let entry = cache.lookup("k").unwrap().expect("hit");
        let stale_at = entry.stale_at_unix.expect("ttl set -> stale_at");
        let after = crate::time::now_unix_secs();
        // store sets stale_at = created + 60. The created time is
        // within [before, after], so stale_at must land in
        // [before+60, after+60].
        assert!(
            stale_at >= before + 60 && stale_at <= after + 60,
            "stale_at={stale_at}, before={before}, after={after}"
        );
    }

    #[test]
    fn store_without_ttl_leaves_stale_at_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ttl_secs: None,
            ..Default::default()
        });
        let resp = Response {
            text: "ok".into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage::default(),
        };
        cache.store("k", "mock", "m", &resp).unwrap();
        let entry = cache.lookup("k").unwrap().expect("hit");
        assert!(entry.stale_at_unix.is_none());
    }

    #[test]
    fn lookup_treats_stale_entry_as_miss() {
        // Bypass time-travel by hand-crafting an entry whose
        // `stale_at_unix` is one second in the past, then write it to
        // disk and confirm `lookup` returns `None`.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ttl_secs: Some(60),
            ..Default::default()
        });
        let now = crate::time::now_unix_secs();
        let entry = CacheEntry {
            schema_version: "v1".into(),
            cache_key: "k".into(),
            provider: "mock".into(),
            model: "m".into(),
            response: Response {
                text: "hello".into(),
                finish_reason: Some("end_turn".into()),
                truncated: false,
                usage: Usage::default(),
            },
            usage: Usage::default(),
            created_unix: now - 120,
            // stale_at is already in the past -> miss.
            stale_at_unix: Some(now - 60),
        };
        let path = sharded::path_for(tmp.path(), "k");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
        let got = cache.lookup("k").unwrap();
        assert!(got.is_none(), "stale entry must miss");
    }

    #[test]
    fn backward_compat_reads_legacy_entries_without_stale_at() {
        // Cache files written by v0.1 (no `stale_at_unix` field) must
        // still be readable: serde omits -> None -> always fresh.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        let legacy = serde_json::json!({
            "schema_version": "v1",
            "cache_key": "k",
            "provider": "mock",
            "model": "m",
            "response": {
                "text": "legacy",
                "finish_reason": "end_turn",
                "truncated": false,
                "usage": { "input_tokens": 0, "output_tokens": 0, "cache_read": 0, "cache_creation": 0 }
            },
            "usage": { "input_tokens": 0, "output_tokens": 0, "cache_read": 0, "cache_creation": 0 },
            "created_unix": 0
        })
        .to_string();
        let path = sharded::path_for(tmp.path(), "k");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, legacy).unwrap();
        let entry = cache.lookup("k").unwrap().expect("legacy hit");
        assert_eq!(entry.response.text, "legacy");
        assert!(entry.stale_at_unix.is_none());
    }
}
