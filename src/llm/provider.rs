//! Provider trait and registry.
//!
//! Catalog references: 10-integrada-v0 §D.15 (config, cost_estimator),
//! §D.19.5 (circuit_breaker), §D.19.6 (rate_limiter), §D.19.8 (plan),
//! §D.35 (api_key switching). All providers implement [`Provider`].
//!
//! Per-provider circuit breakers (catalog §D.19.5) live on top of
//! the concrete providers: [`BreakeredProvider`] wraps an
//! `Arc<dyn Provider>` with an `Arc<CircuitBreaker>`. The wrapper is
//! transparent to callers — `provider.send(req)` either runs the
//! inner call and records success/failure against the breaker, or
//! fails fast when the breaker is open. The wrapper does not call
//! the inner provider at all while the breaker is open, which is
//! what makes the fail-fast behaviour observable from outside.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{CircuitBreakerConfig, ProviderConfig};
use crate::error::{Error, Result};

use super::circuit_breaker::CircuitBreaker;
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
    /// Per-provider circuit breakers, keyed by registry name. The
    /// breaker is created lazily the first time a provider is
    /// wrapped (so callers that build a registry by hand can opt
    /// out by passing unwrapped providers). The
    /// `registry_from_config` helper wraps every provider it
    /// produces, so production callers always have a breaker
    /// available for inspection.
    breakers: HashMap<String, Arc<CircuitBreaker>>,
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
    /// Build a registry from a list of named providers. Each entry
    /// is wrapped in its own default [`CircuitBreaker`] (catalog
    /// §D.19.5: 5 errors in 60 s, 30 s cooldown).
    pub fn new(providers: Vec<(String, Arc<dyn Provider>)>) -> Self {
        let mut by_name = HashMap::new();
        let mut breakers = HashMap::new();
        for (name, p) in providers {
            let breaker = Arc::new(CircuitBreaker::default());
            breakers.insert(name.clone(), breaker.clone());
            let wrapped: Arc<dyn Provider> = Arc::new(BreakeredProvider::new(p, breaker));
            by_name.insert(name, wrapped);
        }
        Self { by_name, breakers }
    }

    /// Look up a provider by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.by_name.get(name).cloned()
    }

    /// Insert a provider by name, wrapped in the supplied circuit
    /// breaker. Replaces any existing entry; the breaker is also
    /// recorded under the same name so [`Self::breaker`] can find
    /// it.
    pub fn insert_with_breaker(
        &mut self,
        name: String,
        provider: Arc<dyn Provider>,
        breaker: Arc<CircuitBreaker>,
    ) {
        self.breakers.insert(name.clone(), breaker.clone());
        self.by_name
            .insert(name, Arc::new(BreakeredProvider::new(provider, breaker)));
    }

    /// Insert a provider by name, wrapping it in a default
    /// [`CircuitBreaker`]. Replaces any existing entry.
    pub fn insert(&mut self, name: String, provider: Arc<dyn Provider>) {
        let breaker = Arc::new(CircuitBreaker::default());
        self.breakers.insert(name.clone(), breaker.clone());
        self.by_name
            .insert(name, Arc::new(BreakeredProvider::new(provider, breaker)));
    }

    /// Read the circuit breaker for a registered provider. Returns
    /// `None` for registries built without breakers (legacy /
    /// hand-rolled paths).
    pub fn breaker(&self, name: &str) -> Option<Arc<CircuitBreaker>> {
        self.breakers.get(name).cloned()
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

/// Wrapper that fronts an inner provider with a
/// [`CircuitBreaker`]. The wrapper is the place where the
/// "fail-fast when open" and "record success/failure per call"
/// policies live; the inner provider stays untouched, which is
/// what keeps the wrapping non-invasive across the seven concrete
/// provider implementations in this crate.
///
/// Send flow:
///
/// 1. If the breaker is open, return
///    `Error::Provider("circuit open: ...")` immediately. The
///    inner provider is not called.
/// 2. Otherwise, call `inner.send(req)`. On `Ok`, call
///    `breaker.record_success()` (resets the failure counter).
/// 3. On `Err`, consult [`Error::is_circuit_opening`]: if the
///    error should count, call `breaker.record_failure()`; if it
///    should not (schema, operator, cancel), leave the breaker
///    state untouched.
///
/// `count_tokens` is delegated straight to the inner provider —
/// token estimation is a local heuristic and is never responsible
/// for opening the breaker.
pub struct BreakeredProvider {
    inner: Arc<dyn Provider>,
    breaker: Arc<CircuitBreaker>,
}

impl std::fmt::Debug for BreakeredProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BreakeredProvider")
            .field("name", &self.inner.name())
            .field("model", &self.inner.model())
            .field("endpoint", &self.inner.endpoint())
            .field("breaker_state", &self.breaker.state())
            .finish()
    }
}

impl BreakeredProvider {
    /// Build a wrapper around `inner` with the supplied breaker.
    pub fn new(inner: Arc<dyn Provider>, breaker: Arc<CircuitBreaker>) -> Self {
        Self { inner, breaker }
    }

    /// Borrow the inner provider (for tests that need to inspect
    /// call records or bypass the breaker).
    pub fn inner(&self) -> &Arc<dyn Provider> {
        &self.inner
    }

    /// Clone the breaker handle for telemetry / introspection.
    pub fn breaker(&self) -> Arc<CircuitBreaker> {
        self.breaker.clone()
    }
}

#[async_trait]
impl Provider for BreakeredProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        if self.breaker.is_open() {
            return Err(Error::Provider(format!(
                "circuit open: provider '{}' sidelined",
                self.inner.name()
            )));
        }
        match self.inner.send(req).await {
            Ok(pair) => {
                self.breaker.record_success();
                Ok(pair)
            }
            Err(e) => {
                if e.is_circuit_opening() {
                    self.breaker.record_failure();
                }
                Err(e)
            }
        }
    }

    async fn count_tokens(&self, text: &str) -> Option<u64> {
        self.inner.count_tokens(text).await
    }
}

/// Build a registry from a map of provider configurations. The
/// `minimax` and `deepseek` configurations are wired to their provider
/// implementations; everything else returns an explicit-not-implemented
/// error unless the user explicitly opts in.
///
/// Every provider is wrapped in a [`BreakeredProvider`] with the
/// breaker knobs from `breaker_cfg` (catalog §D.19.5). The registry
/// keeps the breakers by name so callers can read state via
/// [`ProviderRegistry::breaker`] for telemetry / dashboards.
pub fn registry_from_config(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
    breaker_cfg: &CircuitBreakerConfig,
) -> Result<ProviderRegistry> {
    use super::opencode_go::OpenCodeGoProvider;
    let mut registry = ProviderRegistry::default();
    for (name, spec) in cfg {
        let provider: Arc<dyn Provider> = match spec.kind.as_str() {
            "deepseek" => Arc::new(super::deepseek::DeepSeekProvider::from_config(spec)?),
            "minimax" => Arc::new(super::minimax::MinimaxProvider::from_config(spec)?),
            "mock" => Arc::new(super::mock::MockProvider::empty()),
            "opencode_go" => {
                if OpenCodeGoProvider::is_blocked(&spec.model) {
                    return Err(crate::Error::InvalidArgs(format!(
                        "model '{}' is blocked for opencode_go; use direct minimax provider instead",
                        spec.model
                    )));
                }
                Arc::new(OpenCodeGoProvider::from_config(spec)?)
            }
            // Other provider kinds are not implemented in v0.1.
            other => {
                return Err(crate::Error::InvalidArgs(format!(
                    "provider kind '{other}' is not implemented in MVP v0.1; \
                     only 'deepseek', 'minimax', 'mock', and 'opencode_go' are supported"
                )));
            }
        };
        let breaker = Arc::new(CircuitBreaker::new(
            breaker_cfg.threshold,
            std::time::Duration::from_secs(breaker_cfg.window_secs),
            std::time::Duration::from_secs(breaker_cfg.cooldown_secs),
        ));
        registry.insert_with_breaker(name.clone(), provider, breaker);
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

    /// Q7 pin: `registry_from_config` must refuse to wire any of the
    /// operator-blocked minimax-* model aliases via the opencode_go
    /// subscription. This is the runtime guard that pairs with the
    /// compile-time `BLOCKED_MODELS` list in `opencode_go.rs`.
    #[test]
    fn registry_from_config_rejects_blocked_opencode_go_models() {
        unsafe {
            std::env::set_var("OPENCODE_GO_API_KEY", "dummy-for-test");
        }
        let mut cfg = std::collections::BTreeMap::new();
        cfg.insert(
            "opencode_go".into(),
            crate::config::ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "minimax-m3".into(),
                max_tokens: Some(8192),
                temperature: Some(0.6),
                top_p: Some(0.95),
                hard_incompatibilities: vec![],
            },
        );
        let result = registry_from_config(&cfg, &CircuitBreakerConfig::default());
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
        }
        match result {
            Err(crate::Error::InvalidArgs(msg)) => {
                assert!(
                    msg.contains("minimax-m3") && msg.contains("blocked"),
                    "unexpected InvalidArgs message: {msg}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }
}
