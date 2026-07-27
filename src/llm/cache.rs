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

/// Cache configuration.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Subdirectory under the run directory (`cache/llm`).
    pub root: PathBuf,
    /// Enable cross-run cache (writes to `<MOAGAN_HOME>/cache/llm`).
    pub cross_run: bool,
    /// Skip the request from being cached at all (e.g. for entropy).
    pub no_store: bool,
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
}

impl CacheEntry {
    /// Current schema version.
    pub const SCHEMA_VERSION: &'static str = "v1";
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
        Ok(Some(entry))
    }

    /// Persist an entry atomically.
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
        let entry = CacheEntry {
            schema_version: CacheEntry::SCHEMA_VERSION.to_owned(),
            cache_key: cache_key.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            response: resp.clone(),
            usage: resp.usage.clone(),
            created_unix: crate::time::now_unix_secs(),
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
        // Shard by the first 2 hex chars so a single dir never holds
        // too many entries.
        let (_a, _b, rest) = split_hex(cache_key);
        let shard = &rest[..rest.len().min(4)];
        self.config
            .root
            .join(shard)
            .join(format!("{cache_key}.json"))
    }

    /// Build directory under `root` for the given key.
    pub fn ensure_dir(root: &Path) -> Result<()> {
        std::fs::create_dir_all(root).map_err(|e| Error::Cache(format!("mkdir {root:?}: {e}")))?;
        Ok(())
    }
}

fn split_hex(s: &str) -> (&str, &str, &str) {
    let bytes = s.as_bytes();
    if bytes.len() < 4 {
        return ("", "", s);
    }
    let mid = bytes.len() / 2;
    (&s[..2], &s[2..mid], &s[mid..])
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
            cross_run: false,
            no_store: false,
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
            cross_run: false,
            no_store: false,
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
            cross_run: false,
            no_store: false,
        });
        let entry = cache.lookup("missing").unwrap();
        assert!(entry.is_none());
    }
}
