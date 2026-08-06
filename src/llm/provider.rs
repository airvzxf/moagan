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
//!
//! Per-provider token-bucket rate limiters (catalog §D.19.6) live
//! on the same wrapper via the optional
//! [`super::rate_limiter::RateLimiter`]. When configured, every
//! `send` consumes a token before the inner call; responses whose
//! `Usage::cache_read > 0` (the upstream served from its own prompt
//! cache) refund the token so cached responses do not drain the
//! local bucket. Cache hits at the cross-run layer never reach
//! `provider.send` (they short-circuit at
//! `phases::phase::PhaseContext::call`), so the wrapper only
//! observes provider-level cache hits via the response payload.
//!
//! Per-provider semaphores (catalog §D.9.6) live on the same
//! wrapper via the optional
//! [`crate::execution::PerProviderSemaphores`]. When configured,
//! every `send` acquires one permit keyed by `inner.name()` before
//! the call; the permit is held for the duration of the call and
//! released on drop (RAII). Concurrent calls to the same provider
//! therefore stay within the configured capacity; calls to other
//! providers are not blocked because the semaphore map is keyed
//! per provider name. This is independent of the global
//! [`crate::execution::Parallelism`] cap, which still bounds total
//! in-flight LLM calls across every provider.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{CircuitBreakerConfig, ProviderConfig};
use crate::error::{Error, Result};
use crate::execution::PerProviderSemaphores;

use super::capabilities::ProviderCapabilities;
use super::circuit_breaker::CircuitBreaker;
use super::rate_limiter::RateLimiter;
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

    /// Static capability matrix — see
    /// [`ProviderCapabilities`](super::capabilities::ProviderCapabilities).
    /// The default returns the OpenAI-compat baseline so providers
    /// built before the capability layer (or third-party impls that
    /// don't care about wire-format routing) keep working unchanged.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Stable wire-format identifier derived from `capabilities()`.
    /// The wrapper layer exposes this for telemetry; production
    /// providers should NOT override it.
    fn wire_format_id(&self) -> &'static str {
        self.capabilities().wire_format_id()
    }

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
/// 2. If a [`RateLimiter`] is configured, consume a token before
///    the inner call. The default `acquire` awaits forever when
///    the bucket is empty; `acquire_with_max` (enabled via
///    [`Self::with_rate_limit_max_wait`]) fails fast with an
///    `Error::Provider` carrying a budget-exhausted message when
///    the wait would exceed the configured cap.
/// 3. Otherwise, call `inner.send(req)`. On `Ok`, call
///    `breaker.record_success()` (resets the failure counter) and
///    refund the rate-limit token when `Usage::cache_read > 0`
///    (the upstream served the call from its own prompt cache).
/// 4. On `Err`, consult [`Error::is_circuit_opening`]: if the
///    error should count, call `breaker.record_failure()`; if it
///    should not (schema, operator, cancel), leave the breaker
///    state untouched.
///
/// `count_tokens` is delegated straight to the inner provider —
/// token estimation is a local heuristic and is never responsible
/// for opening the breaker or consuming a rate-limit token.
pub struct BreakeredProvider {
    inner: Arc<dyn Provider>,
    breaker: Arc<CircuitBreaker>,
    rate_limiter: Option<Arc<RateLimiter>>,
    rate_limit_max_wait: Option<Duration>,
    /// Optional per-provider capacity gate (catalog §D.9.6). When
    /// set, `send` acquires one permit from the inner provider's
    /// slot before calling `inner.send(req)` so concurrent calls
    /// to the same provider cannot exceed the configured capacity.
    /// The permit is held for the duration of the inner call and
    /// released on drop (RAII). When `None`, no per-provider
    /// throttling is applied — only the breaker, the (optional)
    /// rate limiter, and the global parallelism pool gate calls.
    provider_semaphores: Option<Arc<PerProviderSemaphores>>,
}

impl std::fmt::Debug for BreakeredProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BreakeredProvider")
            .field("name", &self.inner.name())
            .field("model", &self.inner.model())
            .field("endpoint", &self.inner.endpoint())
            .field("breaker_state", &self.breaker.state())
            .field(
                "rate_limiter",
                &self.rate_limiter.as_ref().map(|_| "configured"),
            )
            .field("rate_limit_max_wait", &self.rate_limit_max_wait)
            .field(
                "provider_semaphores",
                &self.provider_semaphores.as_ref().map(|_| "configured"),
            )
            .finish()
    }
}

impl BreakeredProvider {
    /// Build a wrapper around `inner` with the supplied breaker and
    /// no rate limiter. Equivalent to the pre-rate-limit
    /// constructor — callers that want token-bucket backpressure
    /// chain [`Self::with_rate_limiter`] (and optionally
    /// [`Self::with_rate_limit_max_wait`]) on top of this.
    pub fn new(inner: Arc<dyn Provider>, breaker: Arc<CircuitBreaker>) -> Self {
        Self {
            inner,
            breaker,
            rate_limiter: None,
            rate_limit_max_wait: None,
            provider_semaphores: None,
        }
    }

    /// Build a wrapper around `inner` with the supplied breaker and
    /// rate limiter. Every `send` call consumes a token from the
    /// bucket; calls when the bucket is empty await the next refill
    /// (use [`Self::with_rate_limit_max_wait`] for fail-fast
    /// semantics).
    pub fn with_rate_limiter(
        inner: Arc<dyn Provider>,
        breaker: Arc<CircuitBreaker>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            inner,
            breaker,
            rate_limiter: Some(rate_limiter),
            rate_limit_max_wait: None,
            provider_semaphores: None,
        }
    }

    /// Cap the rate-limit wait. When set, calls that would wait
    /// longer than `max` for the next refill fail with
    /// `Error::Provider` (carrying a budget-exhausted message)
    /// instead of sleeping. Default (`None`) is unbounded wait.
    pub fn with_rate_limit_max_wait(mut self, max: Duration) -> Self {
        self.rate_limit_max_wait = Some(max);
        self
    }

    /// Attach a per-provider semaphore pool (catalog §D.9.6).
    /// Every `send` call acquires one permit keyed by
    /// `inner.name()` before the inner call; the permit is held for
    /// the call's lifetime and released on drop. Concurrent calls
    /// to the same provider therefore cannot exceed the configured
    /// capacity. Calls to other providers are not blocked because
    /// the semaphore map is keyed per provider name. The global
    /// [`crate::execution::Parallelism`] cap still applies on top
    /// of this.
    pub fn with_per_provider_semaphores(mut self, semaphores: Arc<PerProviderSemaphores>) -> Self {
        self.provider_semaphores = Some(semaphores);
        self
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

    /// Clone the rate-limiter handle (when configured) for
    /// telemetry / introspection.
    pub fn rate_limiter(&self) -> Option<Arc<RateLimiter>> {
        self.rate_limiter.clone()
    }

    /// Clone the per-provider semaphore handle (when configured)
    /// for telemetry / introspection.
    pub fn provider_semaphores(&self) -> Option<Arc<PerProviderSemaphores>> {
        self.provider_semaphores.clone()
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

    fn capabilities(&self) -> ProviderCapabilities {
        // Delegate so telemetry sees the inner provider's exact
        // preference rather than the OpenAI-compat default. The
        // wrapper is transparent: the wire-format choice was made
        // when the inner provider was built, and that decision
        // propagates through the wrapper untouched.
        self.inner.capabilities()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        if self.breaker.is_open() {
            return Err(Error::Provider(format!(
                "circuit open: provider '{}' sidelined",
                self.inner.name()
            )));
        }
        if let Some(rl) = &self.rate_limiter {
            let _wait = match self.rate_limit_max_wait {
                Some(max) => rl.acquire_with_max(max).await?,
                None => rl.acquire().await?,
            };
        }
        // Per-provider capacity gate (catalog §D.9.6): acquire one
        // permit from the inner provider's slot before the call and
        // hold it across `inner.send(req)`. The permit is released
        // by RAII when `_permit` drops at the end of this function.
        // When no semaphores are configured, the global parallelism
        // pool still bounds in-flight calls; this gate is purely
        // additive.
        let _permit = if let Some(sem) = &self.provider_semaphores {
            Some(sem.acquire(self.inner.name(), 1).await)
        } else {
            None
        };
        // Wire-format dispatch decision: log the inner provider's
        // preferred wire so dashboards can pin each call to its
        // protocol without re-parsing the request body. The
        // dispatch is metadata here — the inner provider does its
        // own encoding (with model-specific hooks like the
        // response_format opt-out and thinking-block fallback).
        let wire = self.inner.wire_format_id();
        tracing::debug!(
            provider = self.inner.name(),
            model = self.inner.model(),
            wire,
            stage = "wire.dispatch",
            "BreakeredProvider dispatched via {wire} wire"
        );
        match self.inner.send(req).await {
            Ok(pair) => {
                let cache_hit = pair.1.usage.cache_read > 0;
                if cache_hit && let Some(rl) = &self.rate_limiter {
                    rl.refund();
                }
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

    // ----------------------------------------------------------------
    // Rate-limiter wiring tests (PR E1).
    //
    // Each test wraps a `MockProvider` in a `BreakeredProvider` with a
    // token bucket, exercises a `send` call, and asserts on the
    // bucket's observed state. The mock provider returns canned
    // responses so the inner call is deterministic; the breaker stays
    // closed because no errors are produced.
    // ----------------------------------------------------------------

    use crate::config::RateLimitConfig;
    use crate::llm::mock::{MockProvider, MockResponse};
    use crate::llm::role::Role;
    use crate::llm::wire::{Request, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ErrorProvider {
        calls: AtomicUsize,
        error: fn() -> Error,
    }

    #[async_trait]
    impl Provider for ErrorProvider {
        fn name(&self) -> &str {
            "error-provider"
        }

        fn model(&self) -> &str {
            "error-model"
        }

        fn endpoint(&self) -> &str {
            "mock://error"
        }

        async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err((self.error)())
        }
    }

    fn opening_error() -> Error {
        Error::Provider("upstream 503".into())
    }

    fn breaker_request() -> Request {
        Request {
            role: Role::Intake,
            model: "error-model".into(),
            system: String::new(),
            user: String::new(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
        }
    }

    fn threshold_one_breaker() -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn breakered_provider_records_circuit_opening_failure() {
        let inner = Arc::new(ErrorProvider {
            calls: AtomicUsize::new(0),
            error: opening_error,
        });
        let breaker = threshold_one_breaker();
        let provider = BreakeredProvider::new(inner.clone(), breaker.clone());

        let result = provider.send(&breaker_request()).await;
        assert!(matches!(result, Err(Error::Provider(_))));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(breaker.failure_count(), 1);
        assert!(breaker.is_open());
    }

    #[tokio::test]
    async fn breakered_provider_skips_calls_when_open() {
        let inner = Arc::new(ErrorProvider {
            calls: AtomicUsize::new(0),
            error: opening_error,
        });
        let breaker = threshold_one_breaker();
        let provider = BreakeredProvider::new(inner.clone(), breaker.clone());

        let _ = provider.send(&breaker_request()).await;
        let result = provider.send(&breaker_request()).await;
        assert!(
            matches!(result, Err(Error::Provider(message)) if message.starts_with("circuit open"))
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn minimax_provider_records_circuit_opening() {
        let spec = ProviderConfig {
            kind: "minimax".into(),
            endpoint: "http://127.0.0.1:1".into(),
            model: "MiniMax-M3".into(),
            max_tokens: Some(16),
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        };
        let minimax = MinimaxProvider::new(&spec, crate::secret::SecretString::new("dummy".into()))
            .unwrap()
            .with_max_retries(1);
        let breaker = threshold_one_breaker();
        let provider = BreakeredProvider::new(Arc::new(minimax), breaker.clone());

        let result = provider.send(&breaker_request()).await;
        assert!(result.is_err());
        assert!(breaker.is_open());
        assert_eq!(breaker.failure_count(), 1);
    }

    fn sample_request() -> Request {
        Request {
            role: Role::Intake,
            model: "mock-model".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
        }
    }

    /// Each successful `send` consumes one token from the bucket.
    /// Mock provider returns a fresh response every call so the
    /// wrapper has no cache_hit refund path to consider.
    #[tokio::test]
    async fn provider_send_consumes_rate_limit_token() {
        let rl = Arc::new(RateLimiter::new(RateLimitConfig {
            capacity: 3,
            refill_per_sec: 100,
            initial: Some(3),
        }));
        let mock = Arc::new(MockProvider::new(vec![
            MockResponse::plain("a"),
            MockResponse::plain("b"),
            MockResponse::plain("c"),
        ]));
        let breaker = Arc::new(CircuitBreaker::default());
        let provider = BreakeredProvider::with_rate_limiter(mock, breaker, rl.clone());

        // Three calls drain the bucket exactly.
        for expected in ["a", "b", "c"] {
            let (_, resp) = provider.send(&sample_request()).await.unwrap();
            assert_eq!(resp.text, expected);
        }
        // Bucket is at 0; the next `try_acquire` (mirroring the
        // wrapper's pre-call probe) must report the bucket empty.
        assert!(
            !rl.try_acquire(),
            "bucket should be empty after three calls against capacity=3"
        );
    }

    /// Responses whose `Usage::cache_read > 0` refund the token so
    /// the upstream prompt cache does not drain the local bucket.
    /// Three calls with full cache hits leave the bucket at 3.
    #[tokio::test]
    async fn provider_send_skips_rate_limit_on_cache_hit() {
        let rl = Arc::new(RateLimiter::new(RateLimitConfig {
            capacity: 3,
            refill_per_sec: 100,
            initial: Some(3),
        }));
        let mut mock = MockProvider::new(vec![
            MockResponse {
                text: "cached-a".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read: 100,
                    cache_creation: 0,
                },
                finish_reason: Some("end_turn".into()),
            },
            MockResponse {
                text: "cached-b".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read: 100,
                    cache_creation: 0,
                },
                finish_reason: Some("end_turn".into()),
            },
            MockResponse {
                text: "cached-c".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read: 100,
                    cache_creation: 0,
                },
                finish_reason: Some("end_turn".into()),
            },
        ]);
        // Disable cycle so an over-call returns an error instead of
        // wrapping and confusing the assertion.
        mock.set_cycle(false);
        let breaker = Arc::new(CircuitBreaker::default());
        let provider = BreakeredProvider::with_rate_limiter(Arc::new(mock), breaker, rl.clone());

        for expected in ["cached-a", "cached-b", "cached-c"] {
            let (_, resp) = provider.send(&sample_request()).await.unwrap();
            assert_eq!(resp.text, expected);
        }
        // All three tokens were refunded because every response was
        // a cache hit. The next non-blocking probe must succeed.
        assert!(
            rl.try_acquire(),
            "cache hits must not drain the bucket; expected at least one token"
        );
    }

    /// When the bucket is empty and no max-wait is configured, the
    /// wrapper awaits the next refill instead of failing. With
    /// `refill_per_sec=100` the wait is ~10 ms — measurable but
    /// short enough for the test budget.
    #[tokio::test]
    async fn provider_send_waits_when_bucket_empty() {
        let rl = Arc::new(RateLimiter::new(RateLimitConfig {
            capacity: 1,
            refill_per_sec: 100,
            initial: Some(1),
        }));
        let mock = Arc::new(MockProvider::new(vec![
            MockResponse::plain("first"),
            MockResponse::plain("second"),
        ]));
        let breaker = Arc::new(CircuitBreaker::default());
        let provider = BreakeredProvider::with_rate_limiter(mock, breaker, rl.clone());

        // First call drains the bucket (initial = 1).
        let (_, r1) = provider.send(&sample_request()).await.unwrap();
        assert_eq!(r1.text, "first");
        // Second call must wait for the refill and still succeed.
        let started = std::time::Instant::now();
        let (_, r2) = provider.send(&sample_request()).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(r2.text, "second");
        assert!(
            elapsed >= std::time::Duration::from_millis(5),
            "expected >=5 ms wait when bucket is empty at refill_per_sec=100, got {elapsed:?}"
        );
    }

    /// When the bucket is empty AND a max-wait is configured AND
    /// the would-be wait exceeds that cap, the wrapper returns an
    /// `Error::Provider` carrying a budget-exhausted message
    /// instead of sleeping. The inner provider is not called.
    #[tokio::test]
    async fn provider_send_returns_budget_exhausted_on_overflow() {
        let rl = Arc::new(RateLimiter::new(RateLimitConfig {
            capacity: 1,
            refill_per_sec: 1,
            initial: Some(1),
        }));
        let mock = Arc::new(MockProvider::new(vec![
            MockResponse::plain("first"),
            MockResponse::plain("second"),
        ]));
        let breaker = Arc::new(CircuitBreaker::default());
        // Cap the wait at 1 ms; with refill_per_sec=1 and an empty
        // bucket the wait is ~1 s, so the call must fail fast.
        let provider = BreakeredProvider::with_rate_limiter(mock, breaker.clone(), rl)
            .with_rate_limit_max_wait(std::time::Duration::from_millis(1));

        // First call drains the bucket.
        let (_, r1) = provider.send(&sample_request()).await.unwrap();
        assert_eq!(r1.text, "first");

        // Second call exceeds the max-wait; the wrapper must fail
        // fast without touching the inner provider.
        let err = provider
            .send(&sample_request())
            .await
            .expect_err("overflow must return an error");
        match err {
            Error::Provider(msg) => {
                assert!(
                    msg.contains("budget exhausted"),
                    "overflow error must mention budget exhausted, got: {msg}"
                );
            }
            other => panic!("expected Error::Provider, got {other:?}"),
        }
        // Breaker was not opened by the local overflow (the error
        // is not a circuit-opening error class).
        assert!(
            !breaker.is_open(),
            "local overflow must not open the breaker"
        );
    }

    // ----------------------------------------------------------------
    // Capability + wire-format dispatch tests (PR F4).
    //
    // The dispatcher pattern: each concrete provider reports its
    // wire-format preference via `capabilities()`; `BreakeredProvider`
    // forwards the preference through unchanged so callers can
    // log / branch on it. Each provider below has its own
    // expected wire id so the test pins the routing table.
    // ----------------------------------------------------------------

    use crate::llm::deepseek::DeepSeekProvider;
    use crate::llm::minimax::MinimaxProvider;
    use crate::llm::openai_compat::OpenAiCompatProvider;
    use crate::llm::opencode_go::OpenCodeGoProvider;
    use crate::llm::opencode_go_anthropic::OpenCodeGoAnthropicProvider;
    use crate::llm::opencode_go_responses::OpenCodeGoResponsesProvider;

    /// Per-provider capability pin. Every concrete provider must
    /// declare its wire-format preference; the table here mirrors
    /// the constructor matrix in `capabilities.rs`.
    #[test]
    fn provider_capabilities_for_each_provider() {
        let cfg = crate::config::ProviderConfig {
            kind: "minimax".into(),
            endpoint: "https://api.minimax.io/anthropic/v1".into(),
            model: "MiniMax-M3".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        };
        let minimax =
            MinimaxProvider::new(&cfg, crate::secret::SecretString::new("dummy".into())).unwrap();
        let cap = minimax.capabilities();
        assert!(cap.prefers_anthropic_wire, "minimax must prefer anthropic");
        assert_eq!(cap.wire_format_id(), "anthropic");
        assert!(!cap.supports_response_format);

        let cfg_d = crate::config::ProviderConfig {
            kind: "deepseek".into(),
            endpoint: "https://api.deepseek.com/v1".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: Some(8192),
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        };
        let deepseek =
            DeepSeekProvider::new(&cfg_d, crate::secret::SecretString::new("dummy".into()))
                .unwrap();
        let cap = deepseek.capabilities();
        assert!(cap.prefers_openai_wire, "deepseek must prefer openai");
        assert_eq!(cap.wire_format_id(), "openai");
        assert!(cap.supports_response_format);

        let cfg_oc = crate::config::ProviderConfig {
            kind: "opencode_go".into(),
            endpoint: "https://opencode.ai/zen/go/v1".into(),
            model: "qwen3.7-max".into(), // Anthropic-compat path
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        };
        let oc_a = OpenCodeGoAnthropicProvider::new(
            &cfg_oc,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(oc_a.capabilities().wire_format_id(), "anthropic");

        let cfg_ocr = crate::config::ProviderConfig {
            model: "gpt-5.6-luna".into(),
            ..cfg_oc.clone()
        };
        let oc_r = OpenCodeGoResponsesProvider::new(
            &cfg_ocr,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(oc_r.capabilities().wire_format_id(), "responses");

        let cfg_occ = crate::config::ProviderConfig {
            model: "kimi-k2.7-code".into(),
            ..cfg_oc.clone()
        };
        let oc =
            OpenCodeGoProvider::new(&cfg_occ, crate::secret::SecretString::new("dummy".into()))
                .unwrap();
        // Dispatcher delegates to the inner provider; for the
        // chat-completions path the inner is OpenAiCompatProvider
        // and reports `"openai"`.
        assert_eq!(oc.capabilities().wire_format_id(), "openai");

        let cfg_dispatcher = crate::config::ProviderConfig {
            model: "qwen3.7-max".into(),
            ..cfg_oc.clone()
        };
        let oc_d_anthropic = OpenCodeGoProvider::new(
            &cfg_dispatcher,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        // Anthropic-routed dispatcher reports `anthropic`.
        assert_eq!(oc_d_anthropic.capabilities().wire_format_id(), "anthropic");

        let cfg_dispatcher_r = crate::config::ProviderConfig {
            model: "gpt-5.6-luna".into(),
            ..cfg_oc.clone()
        };
        let oc_d_responses = OpenCodeGoProvider::new(
            &cfg_dispatcher_r,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        // Responses-routed dispatcher reports `responses`.
        assert_eq!(oc_d_responses.capabilities().wire_format_id(), "responses");

        let mock_cap = MockProvider::empty().capabilities();
        assert_eq!(mock_cap.wire_format_id(), "openai");
        assert!(mock_cap.supports_streaming);
        assert!(mock_cap.supports_tools);
    }

    /// `BreakeredProvider::capabilities` and
    /// `BreakeredProvider::wire_format_id` delegate to the inner
    /// provider so the dispatcher layer sees the same wire id
    /// the inner provider chose at construction.
    #[test]
    fn breakered_provider_dispatches_via_correct_wire() {
        // OpenAI-compat inner: dispatcher picks the OpenAI wire.
        let inner_oai: Arc<dyn Provider> = Arc::new(
            OpenAiCompatProvider::new(
                &crate::config::ProviderConfig {
                    kind: "deepseek".into(),
                    endpoint: "https://api.deepseek.com/v1".into(),
                    model: "deepseek-v4-flash".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                },
                crate::secret::SecretString::new("dummy".into()),
            )
            .unwrap(),
        );
        let wrapped = BreakeredProvider::new(inner_oai, Arc::new(CircuitBreaker::default()));
        assert_eq!(wrapped.capabilities().wire_format_id(), "openai");
        assert_eq!(wrapped.wire_format_id(), "openai");
        assert!(wrapped.capabilities().supports_response_format);

        // Anthropic inner: dispatcher flips to the Anthropic wire.
        let inner_anth: Arc<dyn Provider> = Arc::new(
            MinimaxProvider::new(
                &crate::config::ProviderConfig {
                    kind: "minimax".into(),
                    endpoint: "https://api.minimax.io/anthropic/v1".into(),
                    model: "MiniMax-M3".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                },
                crate::secret::SecretString::new("dummy".into()),
            )
            .unwrap(),
        );
        let wrapped = BreakeredProvider::new(inner_anth, Arc::new(CircuitBreaker::default()));
        assert_eq!(wrapped.capabilities().wire_format_id(), "anthropic");
        assert_eq!(wrapped.wire_format_id(), "anthropic");
        assert!(!wrapped.capabilities().supports_response_format);

        // Responses inner: dispatcher flips to the Responses wire.
        let inner_resp: Arc<dyn Provider> = Arc::new(
            OpenCodeGoResponsesProvider::new(
                &crate::config::ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: "https://opencode.ai/zen/go/v1".into(),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                },
                crate::secret::SecretString::new("dummy".into()),
            )
            .unwrap(),
        );
        let wrapped = BreakeredProvider::new(inner_resp, Arc::new(CircuitBreaker::default()));
        assert_eq!(wrapped.capabilities().wire_format_id(), "responses");
        assert_eq!(wrapped.wire_format_id(), "responses");
    }

    /// AnthropicWire is the round-trip pair used by the
    /// `BreakeredProvider` path: encode + decode produce the same
    /// parsed shape.
    #[test]
    fn anthropic_wire_format_round_trip() {
        use crate::llm::role::Role;
        use crate::llm::wire_format::{AnthropicWire, WireFormat};

        let req = Request {
            role: Role::Intake,
            model: "qwen3.7-max".into(),
            system: "sys".into(),
            user: "u".into(),
            max_tokens: 64,
            temperature: None,
            top_p: None,
            response_schema: None,
        };
        let wire = AnthropicWire;
        let body = wire.encode_body(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["model"], "qwen3.7-max");
        assert_eq!(parsed["system"], "sys");
        assert_eq!(parsed["messages"][0]["content"], "u");

        // Round-trip the response decoder too.
        let raw = r#"{"content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":2}}"#;
        let decoded = wire.decode(200, raw.as_bytes()).unwrap();
        assert_eq!(decoded.status, 200);
        assert_eq!(decoded.body.text, "hi");
        assert_eq!(decoded.body.usage.input_tokens, 11);
    }

    /// ResponsesWire round-trip: encode produces a body with
    /// `instructions` + `input`; decode threads the
    /// `output_text` blocks into a flat string. Pinned so future
    /// refactors of the OpenAI Responses provider see the same
    /// shape as the dispatcher table.
    #[test]
    fn responses_wire_format_round_trip() {
        use crate::llm::role::Role;
        use crate::llm::wire_format::{ResponsesWire, WireFormat};

        let req = Request {
            role: Role::Intake,
            model: "gpt-5.6-luna".into(),
            system: "instructions".into(),
            user: "the user prompt".into(),
            max_tokens: 32,
            temperature: None,
            top_p: None,
            response_schema: None,
        };
        let wire = ResponsesWire;
        let body = wire.encode_body(&req).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["model"], "gpt-5.6-luna");
        assert_eq!(parsed["instructions"], "instructions");
        assert_eq!(parsed["input"], "the user prompt");
        assert_eq!(parsed["max_tokens"], 32);

        let raw = serde_json::json!({
            "output": [
                {"content": [{"type": "output_text", "text": "hello"}]}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 1}
        });
        let decoded = wire
            .decode(200, &serde_json::to_vec(&raw).unwrap())
            .unwrap();
        assert_eq!(decoded.body.text, "hello");
        assert_eq!(decoded.body.usage.input_tokens, 5);
    }

    // ----------------------------------------------------------------
    // Per-provider semaphores wiring tests (PR G6, D.9.6 wire).
    //
    // `BreakeredProvider` accepts an `Arc<PerProviderSemaphores>` and
    // acquires one permit keyed by `inner.name()` before each `send`.
    // The permit is held for the duration of the inner call and
    // released on drop (RAII). Tests below pin three behaviours:
    //
    // 1. The semaphore map records the inner provider's name the
    //    first time `send` is called, and the permit is released at
    //    end of `send` (RAII).
    // 2. With `permits=1` and the only permit held externally, a
    //    second concurrent `send` blocks inside the wrapper until
    //    the permit is dropped. The inner provider is not invoked
    //    while the call is blocked at the gate.
    // 3. The pre-wire-up legacy path (no semaphores configured)
    //    stays unchanged: `send` returns immediately and there is
    //    no per-provider capacity gate.
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn breakered_provider_acquires_per_provider_semaphore() {
        use crate::execution::PerProviderSemaphores;
        let sem = Arc::new(PerProviderSemaphores::new());
        let mock = Arc::new(MockProvider::new(vec![MockResponse::plain("ok")]));
        let breaker = Arc::new(CircuitBreaker::default());
        let provider =
            BreakeredProvider::new(mock.clone(), breaker).with_per_provider_semaphores(sem.clone());

        let (_, resp) = provider.send(&sample_request()).await.expect("ok");
        assert_eq!(resp.text, "ok");
        assert_eq!(mock.calls().len(), 1, "inner provider must be called");

        // The wrapper acquires one permit keyed by `inner.name()`.
        // The RAII permit is released at the end of `send`, so the
        // slot is back at its initial capacity after the call.
        let permits_after = sem
            .available_permits("mock")
            .await
            .expect("per-provider semaphore slot must exist for the wrapped provider");
        assert_eq!(
            permits_after, 1,
            "permit must be released after send (RAII)"
        );
    }

    #[tokio::test]
    async fn breakered_provider_blocks_when_provider_semaphore_saturated() {
        use crate::execution::PerProviderSemaphores;
        let sem = Arc::new(PerProviderSemaphores::new());
        let mock = Arc::new(MockProvider::new(vec![MockResponse::plain("ok")]));
        let provider: Arc<dyn Provider> = Arc::new(
            BreakeredProvider::new(mock.clone(), Arc::new(CircuitBreaker::default()))
                .with_per_provider_semaphores(sem.clone()),
        );

        // Pre-acquire the only available permit on the `mock`
        // provider's slot. Capacity is 1 (permits=1 -> one permit),
        // so any subsequent `send` on this provider must block at
        // the gate until we drop ours.
        let held_permit = sem.acquire("mock", 1).await;

        // Spawn a second `send` against the same provider. It must
        // sit at the semaphore acquire until we release ours; the
        // inner provider must NOT see it during that window.
        let p = provider.clone();
        let task = tokio::spawn(async move { p.send(&sample_request()).await });

        // Give the spawned task enough wall-clock time to either
        // complete or be observably stuck on the acquire.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !task.is_finished(),
            "spawned send must block while a permit is held externally"
        );
        assert_eq!(
            mock.calls().len(),
            0,
            "inner provider must NOT be called while the second send is blocked at the gate"
        );

        // Release the permit. The spawned task unblocks, runs the
        // inner call, and returns Ok.
        drop(held_permit);
        let result = task.await.expect("task panicked").expect("send ok");
        assert_eq!(result.1.text, "ok");
        assert_eq!(
            mock.calls().len(),
            1,
            "inner provider must be called exactly once after the permit is released"
        );
    }

    #[tokio::test]
    async fn breakered_provider_no_semaphore_does_not_block() {
        let mock = Arc::new(MockProvider::new(vec![
            MockResponse::plain("first"),
            MockResponse::plain("second"),
            MockResponse::plain("third"),
        ]));
        let breaker = Arc::new(CircuitBreaker::default());
        // No `.with_per_provider_semaphores(...)` call: the wrapper
        // must keep the legacy path where `send` returns as soon as
        // the inner call finishes, with no extra capacity gate on
        // the per-provider slot.
        let provider = BreakeredProvider::new(mock.clone(), breaker);

        // Three back-to-back calls must each return immediately --
        // the only guard between them is the inner provider (a
        // mock that returns in well under one millisecond).
        let started = std::time::Instant::now();
        for expected in ["first", "second", "third"] {
            let (_, resp) = provider.send(&sample_request()).await.expect("ok");
            assert_eq!(resp.text, expected);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "no-semaphore path must not introduce extra waits, got {elapsed:?}"
        );
        assert_eq!(mock.calls().len(), 3);
    }
}
