//! Provider trait and registry.
//!
//! Catalog references: 10-integrada-v0 §D.15 (config, cost_estimator),
//! §D.19.5 (circuit_breaker), §D.19.6 (rate_limiter), §D.19.8 (plan),
//! §D.35 (api_key switching). All providers implement [`Provider`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::Result;

use super::wire::{Request, Response};

/// A provider can take a `Request` and produce a `Response`. Providers
/// must be `Send + Sync` so they can live inside an `Arc` and be shared
/// across the run process.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable short name (e.g. `"minimax"`, `"mock"`).
    fn name(&self) -> &str;

    /// Model identifier (e.g. `"MiniMax-M3"`).
    fn model(&self) -> &str;

    /// HTTP endpoint. Providers that do not talk to HTTP may return
    /// any stable string (e.g. `"mock://local"`).
    fn endpoint(&self) -> &str;

    /// Send a request and return the HTTP status alongside the
    /// response. The status is recorded in the call-level telemetry
    /// so audits can see which calls were clipped, throttled, or
    /// failed at the transport layer without parsing the response
    /// body. Implementations must honour the caller's cancellation
    /// context if they support it.
    async fn send(&self, req: &Request) -> Result<(u16, Response)>;

    /// Optional: count tokens for pre-flight estimation. Default returns
    /// `None` and the caller falls back to a heuristic.
    async fn count_tokens(&self, text: &str) -> Option<u64> {
        let _ = text;
        None
    }
}

/// Registry of providers by name.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    by_name: HashMap<String, Arc<dyn Provider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        f.debug_struct("ProviderRegistry")
            .field("names", &names)
            .finish()
    }
}

impl ProviderRegistry {
    /// Build a registry from a list of named providers.
    pub fn new(providers: Vec<(String, Arc<dyn Provider>)>) -> Self {
        let mut by_name = HashMap::new();
        for (name, p) in providers {
            by_name.insert(name, p);
        }
        Self { by_name }
    }

    /// Look up a provider by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.by_name.get(name).cloned()
    }

    /// Insert a provider by name. Replaces any existing entry.
    pub fn insert(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.by_name.insert(name, provider);
    }

    /// Iterate over all registered providers.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Provider>)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of registered providers.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// True if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Build a registry from a map of provider configurations. The
/// `minimax` configuration is wired against the `minimax` provider
/// implementation; everything else falls back to `MockProvider` with an
/// explicit-not-implemented error unless the user explicitly opts in.
pub fn registry_from_config(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
) -> Result<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    for (name, spec) in cfg {
        let provider: Arc<dyn Provider> = match spec.kind.as_str() {
            "minimax" => Arc::new(super::minimax::MinimaxProvider::from_config(spec)?),
            "mock" => Arc::new(super::mock::MockProvider::empty()),
            // Other provider kinds are not implemented in v0.1. They
            // must be explicitly enabled by the user in a future
            // release; the MVP only ships minimax + mock.
            other => {
                return Err(crate::Error::InvalidArgs(format!(
                    "provider kind '{other}' is not implemented in MVP v0.1; \
                     only 'minimax' and 'mock' are supported"
                )));
            }
        };
        registry.insert(name.clone(), provider);
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_insert_and_get() {
        let mut r = ProviderRegistry::default();
        let p: Arc<dyn Provider> = Arc::new(super::super::mock::MockProvider::empty());
        r.insert("mock".into(), p);
        assert!(r.get("mock").is_some());
        assert!(r.get("nope").is_none());
        assert_eq!(r.len(), 1);
    }
}
