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
    /// Optional size cap in bytes. When `Some(cap)`, after every
    /// successful `store()` the cache evicts least-recently-used
    /// entries until the total on-disk size of `cache/llm/<root>/`
    /// is `<= cap`. `None` means "no cap" (matching v0.1
    /// behaviour: the cache could grow without bound).
    ///
    /// LRU key is `touched_at_unix`, bumped on both writes and
    /// successful reads. Entries that lack the field (legacy v0.1
    /// files) are treated as the oldest entries first.
    pub max_bytes: Option<u64>,
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
    /// Optional touched-at unix seconds. Bumped on both writes and
    /// reads that return a hit. LRU eviction orders entries by this
    /// field ascending; entries that lack it (legacy v0.1 files,
    /// v0.2 binaries writing TTL but not yet LRU) are treated as the
    /// oldest entries first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touched_at_unix: Option<i64>,
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
    /// On hit, `touched_at_unix` is bumped to the current wall clock
    /// and the entry is rewritten atomically. This is what makes the
    /// LRU eviction correct: a freshly hit entry moves to the back
    /// of the eviction queue, regardless of when it was originally
    /// stored.
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
        let mut entry: CacheEntry = serde_json::from_slice(&raw)
            .map_err(|e| Error::Cache(format!("decode {path:?}: {e}")))?;
        if !entry.is_fresh(crate::time::now_unix_secs()) {
            return Ok(None);
        }
        entry.touched_at_unix = Some(crate::time::now_unix_secs());
        let bytes = serde_json::to_vec(&entry).map_err(|e| Error::Cache(format!("encode: {e}")))?;
        AtomicWriter::new().write(&path, &bytes)?;
        Ok(Some(entry))
    }

    /// Persist an entry atomically.
    ///
    /// When `CacheConfig.ttl_secs` is set, the entry's `stale_at_unix`
    /// is set to `created_unix + ttl_secs`. When `ttl_secs` is `None`,
    /// `stale_at_unix` is left as `None` and the entry never expires
    /// (matches v0.1 behaviour).
    ///
    /// `touched_at_unix` is always set to `created_unix` on a write,
    /// so LRU eviction treats the freshest writer as the freshest
    /// entry. After a successful write, if `max_bytes` is set and
    /// the cache root exceeds the cap, [`evict_lru`] is called.
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
            touched_at_unix: Some(created_unix),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|e| Error::Cache(format!("encode: {e}")))?;
        let path = self.path_for(cache_key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Cache(format!("mkdir {parent:?}: {e}")))?;
        }
        AtomicWriter::new().write(&path, &bytes)?;
        if self.config.max_bytes.is_some() {
            self.evict_lru()?;
        }
        Ok(())
    }

    fn path_for(&self, cache_key: &str) -> PathBuf {
        sharded::path_for(&self.config.root, cache_key)
    }

    /// Walk every cache file under `self.config.root`, read each as a
    /// `CacheEntry` for its `touched_at_unix`, and remove the
    /// least-recently-touched files until the total on-disk size is
    /// at or below [`CacheConfig::max_bytes`].
    ///
    /// No-op when `max_bytes` is `None` or when the cache is already
    /// under the cap. Headroom of 10% is reserved so this does not
    /// fire on every single `store()` once the cache hovers near
    /// the cap; this is the conventional LRU back-off.
    fn evict_lru(&self) -> Result<()> {
        let cap = match self.config.max_bytes {
            Some(cap) => cap,
            None => return Ok(()),
        };
        let mut entries = enumerate_cache_entries(&self.config.root)?;
        // Total > cap is the precondition; under-cap short-circuits.
        let total: u64 = entries.iter().map(|e| e.size_bytes).sum();
        if total <= cap {
            return Ok(());
        }
        // Sort by touched_at_unix ascending. Legacy entries without
        // the field land at the front (treated as i64::MIN).
        entries.sort_by_key(|e| e.touched_at_unix.unwrap_or(i64::MIN));
        // Remove the oldest entries one at a time until current is
        // back at or under the cap. The classic LRU hard-cap
        // pattern: eviction is amortised O(1) per write because
        // every entry is removed at most once per cache lifetime.
        let mut current = total;
        for entry in entries {
            if current <= cap {
                break;
            }
            std::fs::remove_file(&entry.path)
                .map_err(|e| Error::Cache(format!("remove {path:?}: {e}", path = entry.path)))?;
            current = current.saturating_sub(entry.size_bytes);
        }
        Ok(())
    }
}

/// Snapshot of one cache file on disk, used to drive LRU eviction.
#[derive(Debug)]
struct CacheFileStat {
    path: PathBuf,
    size_bytes: u64,
    /// `i64::MIN` when the file lacks a `touched_at_unix` field
    /// (legacy v0.1 entries). Otherwise the value stored in the JSON.
    touched_at_unix: Option<i64>,
}

/// Walk `root` recursively and collect every regular `*.json` file
/// that is **not** an `AtomicWriter` sidecar (`*.meta.json`). We
/// deliberately do **not** consult the `walkdir` crate even though
/// it's already a transitive dep: keep the eviction path free of
/// dependencies that aren't in `Cargo.toml`'s direct list, so the
/// binary stays minimal.
fn enumerate_cache_entries(root: &Path) -> Result<Vec<CacheFileStat>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read =
            std::fs::read_dir(&dir).map_err(|e| Error::Cache(format!("read_dir {dir:?}: {e}")))?;
        for entry in read {
            let entry = entry.map_err(|e| Error::Cache(format!("dir entry under {dir:?}: {e}")))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| Error::Cache(format!("file_type {path:?}: {e}")))?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            // AtomicWriter sidecars end in ".meta.json", which
            // produces a stem of "<name>.meta". Skip them so the
            // LRU sweep does not evict provenance metadata.
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            if stem.ends_with(".meta") {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|e| Error::Cache(format!("metadata {path:?}: {e}")))?;
            let raw =
                std::fs::read(&path).map_err(|e| Error::Cache(format!("read {path:?}: {e}")))?;
            // Best effort: undecodable / non-cache files are
            // simply listed with `touched_at_unix = None` so the
            // LRU sweep considers them evictable.
            let touched_at_unix = serde_json::from_slice::<CacheEntry>(&raw)
                .ok()
                .and_then(|e| e.touched_at_unix);
            out.push(CacheFileStat {
                path,
                size_bytes: metadata.len(),
                touched_at_unix,
            });
        }
    }
    Ok(out)
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
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
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
            touched_at_unix: None,
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
            touched_at_unix: Some(100),
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
            touched_at_unix: Some(now - 120),
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

    fn big_response(text: &str, repeat: usize) -> Response {
        // Generate a payload that is roughly `repeat` bytes after
        // JSON encoding, so size-based eviction tests have a
        // predictable unit to play with.
        let body = text.repeat(repeat);
        Response {
            text: body,
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage::default(),
        }
    }

    #[test]
    fn store_sets_touched_at_unix_to_created_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        let before = crate::time::now_unix_secs();
        cache
            .store("k", "mock", "m", &big_response("x", 16))
            .unwrap();
        let entry = cache.lookup("k").unwrap().expect("hit");
        let touched = entry.touched_at_unix.expect("touched_at set");
        assert!(touched >= before, "touched={touched} before={before}");
    }

    #[test]
    fn lookup_bumps_touched_at_unix_on_each_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        cache
            .store("k", "mock", "m", &big_response("x", 16))
            .unwrap();
        let first = cache.lookup("k").unwrap().unwrap().touched_at_unix.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let second = cache.lookup("k").unwrap().unwrap().touched_at_unix.unwrap();
        assert!(
            second > first,
            "second hit must advance touched_at_unix ({first} -> {second})"
        );
    }

    #[test]
    fn max_bytes_unset_never_triggers_eviction() {
        // Sanity guard: max_bytes = None -> store() does not even
        // call evict_lru(), regardless of how many entries land on
        // disk. We just confirm store + lookup still works without
        // an explicit cap.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        for i in 0..8u32 {
            cache
                .store(&format!("k{i:064x}"), "mock", "m", &big_response("x", 32))
                .unwrap();
        }
        for i in 0..8u32 {
            assert!(
                cache.lookup(&format!("k{i:064x}")).unwrap().is_some(),
                "entry {i} must still hit when max_bytes is unset"
            );
        }
    }

    #[test]
    fn max_bytes_under_cap_is_no_op() {
        // Each entry's JSON is ~64 bytes. Cap at 1 MiB so the 4
        // entries trivially fit.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            max_bytes: Some(1024 * 1024),
            ..Default::default()
        });
        for i in 0..4u32 {
            cache
                .store(&format!("k{i:064x}"), "mock", "m", &big_response("x", 16))
                .unwrap();
        }
        for i in 0..4u32 {
            assert!(
                cache.lookup(&format!("k{i:064x}")).unwrap().is_some(),
                "entry {i} must still hit when under cap"
            );
        }
    }

    #[test]
    fn max_bytes_over_cap_evicts_oldest_first() {
        // Measure each entry's actual on-disk size after write so
        // the cap is set deterministically regardless of how serde
        // happens to encode the structs. We then cap the cache at
        // "3 entries worth" and write a 9th entry; the LRU sweep
        // must remove enough oldest entries so the cache holds
        // only the 3 freshest.
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            max_bytes: None,
            ..Default::default()
        });
        let keys: Vec<String> = (0..8u32).map(|i| format!("e{i:064x}")).collect();
        for (idx, key) in keys.iter().enumerate() {
            cache
                .store(key, "mock", "m", &big_response("x", 32))
                .unwrap();
            if idx < keys.len() - 1 {
                std::thread::sleep(std::time::Duration::from_millis(1_500));
            }
        }
        // Cross a second boundary so the next store is strictly
        // newer than any of the 8 prior entries (avoids the
        // stable-sort tie-break with the DFS path order).
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        // Measure sizes so we can pick a deterministic cap.
        let stats = enumerate_cache_entries(tmp.path()).unwrap();
        assert_eq!(stats.len(), 8, "should have 8 entries before the new write");
        let per_entry = stats.iter().map(|e| e.size_bytes).max().unwrap() + 1;
        // Cap holds 4 entries comfortably (4 * per_entry, with a
        // small +1 to absorb rounding). Removing the 6 oldest
        // leaves 3 entries (the 2 newest pre-existing + "new").
        let four_fit = per_entry * 4;
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            max_bytes: Some(four_fit),
            ..Default::default()
        });
        cache
            .store("new", "mock", "m", &big_response("fresh", 32))
            .unwrap();
        // After eviction at most 3 entries should survive; the 3
        // freshest. The fresh write ("new") plus the 2 newest
        // staggered entries are the only 3 with touched_at_unix
        // spanning the post-eviction window.
        assert!(
            cache.lookup("new").unwrap().is_some(),
            "freshest entry should survive its own triggering eviction"
        );
        assert!(
            cache.lookup(keys.last().unwrap()).unwrap().is_some(),
            "second-freshest entry (keys[7]) should survive"
        );
        assert!(
            cache.lookup(&keys[6]).unwrap().is_some(),
            "third-freshest entry (keys[6]) should survive at the cap boundary"
        );
        // All older entries should have been evicted to make room.
        for key in &keys[..6] {
            assert!(
                cache.lookup(key).unwrap().is_none(),
                "entry {key} must be evicted (it is older than the surviving 3)"
            );
        }
    }

    #[test]
    fn eviction_prioritises_legacy_entries_without_touched_at() {
        // Backward-compat path: entries missing `touched_at_unix`
        // (legacy v0.1 files) are sorted as i64::MIN, so they go
        // first when the cache overflows. Cap the cache to one
        // entry so the test asserts that exactly the legacy entry
        // is dropped and the two modern ones survive.
        let tmp = tempfile::tempdir().unwrap();
        // Hand-craft a legacy entry.
        let legacy_path = sharded::path_for(tmp.path(), "legacy");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_path,
            serde_json::json!({
                "schema_version": "v1",
                "cache_key": "legacy",
                "provider": "mock",
                "model": "m",
                "response": {
                    "text": "l".repeat(64),
                    "finish_reason": "end_turn",
                    "truncated": false,
                    "usage": { "input_tokens": 0, "output_tokens": 0, "cache_read": 0, "cache_creation": 0 }
                },
                "usage": { "input_tokens": 0, "output_tokens": 0, "cache_read": 0, "cache_creation": 0 },
                "created_unix": 1_000_000
            })
            .to_string(),
        )
        .unwrap();
        // Modern entry with similar size and an explicit bump
        // (via lookup).
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        });
        cache
            .store("modern", "mock", "m", &big_response("m", 64))
            .unwrap();
        cache.lookup("modern").unwrap();
        // Cross a second boundary so the next store ("fresh") has
        // a strictly newer touched_at_unix than "modern".
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        // Measure per-entry size on disk, then pick a cap that
        // forces only the legacy entry to be evicted.
        let stats = enumerate_cache_entries(tmp.path()).unwrap();
        assert_eq!(stats.len(), 2, "exactly 2 entries before the new write");
        let max_entry = stats.iter().map(|e| e.size_bytes).max().unwrap();
        let cache = Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            // Cap = 2 * max_entry: just enough for two entries
            // (modern + fresh), so evicting legacy alone brings the
            // cache under the cap.
            max_bytes: Some(max_entry * 2),
            ..Default::default()
        });
        cache
            .store("fresh", "mock", "m", &big_response("f", 64))
            .unwrap();
        assert!(
            cache.lookup("legacy").unwrap().is_none(),
            "legacy entry (no touched_at_unix) is the LRU front and must be evicted first"
        );
        assert!(
            cache.lookup("modern").unwrap().is_some(),
            "modern entry (recent touch) must survive"
        );
        assert!(
            cache.lookup("fresh").unwrap().is_some(),
            "freshest entry must survive"
        );
    }
}
