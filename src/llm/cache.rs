//! LLM cache. Idempotent lookups keyed by BLAKE3 over the canonical
//! (role, phase, provider, model, request). On miss, the caller hands
//! back the response and a sidecar JSON gets written atomically.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::writer::AtomicWriter;
use crate::error::{Error, Result};
use crate::ids::canonical_hash;
use crate::llm::prompts::prompt_set_hash;

use super::wire::{Request, Response};

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
    /// Stored usage.
    pub usage: serde_json::Value,
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
    pub fn lookup(&self, cache_key: &str) -> Result<Option<CacheEntry>> {
        if self.config.no_store {
            return Ok(None);
        }
        let path = self.path_for(cache_key);
        if !path.exists() {
            return Ok(None);
        }
        let (raw, _meta) = AtomicWriter::new().read_with_meta(&path)?;
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
            usage: serde_json::json!({"input": resp.usage.input_tokens, "output": resp.usage.output_tokens}),
            created_unix: crate::time::now_unix_secs(),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|e| Error::Cache(format!("encode: {e}")))?;
        let path = self.path_for(cache_key);
        AtomicWriter::new().write(&path, &bytes)?;
        Ok(())
    }

    fn path_for(&self, cache_key: &str) -> PathBuf {
        // Shard by the first 2 hex chars so a single dir never holds
        // too many entries.
        let (_a, _b, rest) = split_hex(cache_key);
        self.config
            .root
            .join(&rest[..4])
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
            usage: super::super::wire::Usage::default(),
        };
        cache.store(key, "mock", "m", &resp).unwrap();
        let entry = cache.lookup(key).unwrap().unwrap();
        assert_eq!(entry.response.text, "hello");
        assert_eq!(entry.provider, "mock");
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
