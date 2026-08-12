//! D.6.4: `PromptCache` wrapper that indexes by `prompt_id`
//! (a stable identifier per prompt template version) alongside the
//! existing content-hash cache. Callers that want to invalidate or
//! look up a response without recomputing the content hash can use
//! the `prompt_id` index instead.

use std::collections::HashMap;
use std::sync::Arc;

use crate::llm::cache::{Cache, CacheEntry};

/// Wraps a [`Cache`] with an additional index keyed by stable
/// `prompt_id`. The `prompt_id` is opaque to the underlying cache
/// — it just maps to the canonical content-hash key that the rest
/// of moagan uses.
pub struct PromptCache {
    /// `prompt_id -> cache_key` mapping. The value is the same
    /// canonical hash that [`Cache::cache_key`] produces for the
    /// rendered prompt.
    pub by_id: HashMap<String, String>,
    /// The underlying content-hash cache.
    pub cache: Arc<Cache>,
}

impl PromptCache {
    /// Wrap an existing `Cache`. The returned `PromptCache` starts
    /// with an empty `prompt_id` index; callers populate it with
    /// [`register`] as they observe new prompts.
    pub fn new(cache: Arc<Cache>) -> Self {
        Self {
            by_id: HashMap::new(),
            cache,
        }
    }

    /// Look up a cache entry by stable `prompt_id`. Returns
    /// `None` when the id is unknown, or when the canonical key
    /// it points at is a cache miss (e.g. evicted, expired,
    /// never stored).
    pub fn lookup_by_id(&self, prompt_id: &str) -> Option<CacheEntry> {
        let key = self.by_id.get(prompt_id)?;
        self.cache.lookup(key).ok().flatten()
    }

    /// Register the canonical `cache_key` for a `prompt_id`. The
    /// id can then be used with [`lookup_by_id`] until the entry
    /// is evicted or overwritten.
    pub fn register(&mut self, prompt_id: &str, cache_key: String) {
        self.by_id.insert(prompt_id.to_string(), cache_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::cache::{Cache, CacheConfig, CacheEntry};
    use crate::llm::role::Role;
    use crate::llm::wire::{Request, Response, Usage};

    fn make_request(text: &str) -> Request {
        Request {
            role: Role::Intake,
            model: "m".into(),
            system: "sys".into(),
            user: text.into(),
            max_tokens: 8,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            reasoning_tokens: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn prompt_cache_lookup_by_id_returns_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        }));
        let resp = Response {
            text: "hi".into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read: 0,
                cache_creation: 0,
            },
        };
        let key = Cache::cache_key(&make_request("hello"), "mock", "m");
        cache.store(&key, "mock", "m", &resp).unwrap();

        let mut pc = PromptCache::new(cache.clone());
        pc.register("greeting-v1", key.clone());

        let entry: CacheEntry = pc.lookup_by_id("greeting-v1").expect("hit");
        assert_eq!(entry.response.text, "hi");
        assert!(pc.lookup_by_id("unknown").is_none(), "missing id -> None");
    }

    #[test]
    fn prompt_cache_register_overwrites_existing_id() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Arc::new(Cache::new(CacheConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        }));
        let mut pc = PromptCache::new(cache);
        pc.register("greeting-v1", "aaaa".into());
        pc.register("greeting-v1", "bbbb".into());
        assert_eq!(
            pc.by_id.get("greeting-v1").map(String::as_str),
            Some("bbbb")
        );
    }
}
