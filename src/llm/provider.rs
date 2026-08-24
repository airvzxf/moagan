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
use parking_lot::Mutex;

use crate::config::{CircuitBreakerConfig, ProviderConfig, RateLimitConfig};
use crate::error::Result;
// `Error` is referenced by the tests module below; gate the import
// to `cfg(test)` so the production build does not see the unused
// warning while the test suite still has the symbol in scope.
#[cfg(test)]
use crate::error::Error;
use crate::execution::PerProviderSemaphores;
use crate::fs_layout::MoaganHome;

use super::capabilities::ProviderCapabilities;
use super::circuit_breaker::CircuitBreaker;
use super::probe_table::MaxTokensTable;
use super::provider_pool::{ProviderPool, ProviderPoolEntry};
use super::rate_limiter::RateLimiter;
use super::temperature_probe::{TEMPERATURE_PROBE_BATCH_SIZE, TemperatureTable};
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

    /// Return the `max_tokens` value that [`Self::send`] will actually
    /// transmit on the wire for `req`, after every per-provider cap
    /// (operator override, kind-level ceiling, auto-probe table, …)
    /// has been applied.
    ///
    /// This is the single source of truth for audit-log hashing: the
    /// caller (`phases::phase`) clones `req`, sets
    /// `cloned.max_tokens = self.effective_max_tokens(req)`, and feeds
    /// the clone to `request_body_sha256`. Because the clamp chain
    /// here is the same one `send` runs against `req.max_tokens`, the
    /// recorded sha256 matches the proxy's wire capture
    /// byte-for-byte.
    ///
    /// The default returns `req.max_tokens` unchanged — correct for
    /// providers that do not clamp (mock, …).
    /// Implementations that clamp inside `send` must override this
    /// so the audit hash stays in sync with the wire body.
    fn effective_max_tokens(&self, req: &Request) -> u32 {
        req.max_tokens
    }

    /// Send a probe request bypassing the safety wire-clamp. Used by
    /// the auto-probe to discover the upstream's actual
    /// `max_tokens` boundary. Default impl just calls [`Self::send`];
    /// providers that carry a wire-side ceiling (the
    /// `MINIMAX_MAX_TOKENS_CAP` and `u32::MAX`
    /// clamps in `capabilities.rs`) override to skip the clamp so the
    /// probe can see the real upstream behaviour.
    async fn send_probe(&self, req: &Request) -> Result<(u16, Response)> {
        self.send(req).await
    }

    /// Upper bound the auto-probe should search up to for this
    /// provider. The exponential phase in
    /// [`super::probe::detect_max_tokens`] walks `2^1..2^30`; if the
    /// upstream rejects every value above a smaller bound (e.g.
    /// DeepSeek at 393_216), the probe must stop at that bound
    /// rather than waste 30 sequential HTTP round-trips probing
    /// values the upstream will never accept.
    ///
    /// Returns the per-provider safety ceiling when one exists
    /// (the smallest `2^k > ceiling` is the first probe value the
    /// algorithm short-circuits on). The default is
    /// [`super::probe::MAX_AUTOPROBE_CEILING`] (≈ 1.07G) which is
    /// correct for providers without a documented ceiling (mock,
    /// third-party relays with permissive limits). Providers that
    /// clamp inside `send` (`minimax`, `deepseek`, `opencode_go`
    /// and its routed variants) override this so the probe does
    /// not have to discover what the clamp already pins.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        super::probe::MAX_AUTOPROBE_CEILING
    }

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
    /// Per-provider wrapped handles, keyed by registry name. The
    /// breaker is held by each wrapper (constructed in
    /// [`BreakeredProvider::new`]); the registry intentionally does
    /// NOT keep a separate `Arc<CircuitBreaker>` map because that
    /// would re-share one breaker instance across the registry
    /// lookup table and the wrapper's send-flow — which is the
    /// failure mode the per-call-site breaker fix removes. The
    /// [`Self::breaker`] accessor delegates to the wrapper's own
    /// breaker so callers (dashboards, integration tests) still get
    /// the per-provider state without bringing back the shared map.
    ///
    /// `by_name` keeps the same handles upcast to `Arc<dyn
    /// Provider>` so the public lookup stays trait-object-based;
    /// `wrapped` keeps them downcast to `Arc<BreakeredProvider>`
    /// so [`Self::with_saturation_sink`] can attach a push-side
    /// saturation sink to every entry after the registry has
    /// been built. Empty for registries built without the
    /// `BreakeredProvider` wrapper (legacy hand-rolled paths and
    /// the few tests that insert pre-built wrappers directly);
    /// production callers always go through `registry_from_config`
    /// so this map is non-empty in production.
    wrapped: HashMap<String, Arc<BreakeredProvider>>,
    /// D.19.19/.20: round-robin pool of provider entries. Populated
    /// by `registry_from_config` when the config has multiple
    /// instances of the same provider kind (e.g. 2x `mock` or 2x
    /// `minimax`). When present, [`Self::pick`] performs atomic
    /// round-robin selection across the pool entries instead of
    /// returning the same instance every call. `None` for
    /// registries built without duplicates — single-instance
    /// configs and hand-rolled test paths.
    pool: Option<Arc<ProviderPool>>,
    /// Registry names aligned with the pool's entry order. Used by
    /// [`Self::pick`] to translate a round-robin index back into a
    /// provider lookup. Empty when no pool is configured.
    pool_names: Vec<String>,
    /// Optional table of auto-discovered `max_tokens` per
    /// `(provider, model)`. `None` when auto-probe is disabled
    /// (`max_token_auto = None`/`Some(0)` for every provider).
    max_tokens_table: Option<Arc<MaxTokensTable>>,
    /// Optional table of auto-discovered supported sampling
    /// temperatures per `(provider, model)`. Built from
    /// `<MOAGAN_HOME>/temperatures_auto.toml` by
    /// [`registry_from_config_with_home_and_sink`] and consulted by
    /// [`crate::phases::phase::RunContext::dispatch_to_provider`] on
    /// every LLM call so a temperature outside the discovered set is
    /// clamped to the nearest valid value (with a `tracing::warn!`).
    /// `None` disables the clamp and the per-cell rewrite path; the
    /// discovery fan-out then relies on the operator's profile
    /// temperatures being upstream-acceptable (a global cap, not
    /// per-model reality).
    temperature_table: Option<Arc<TemperatureTable>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        f.debug_struct("ProviderRegistry")
            .field("names", &names)
            .field("pool_len", &self.pool.as_ref().map(|p| p.len()))
            .field("pool_names", &self.pool_names)
            // Print presence only: the table can hold an entry per
            // (provider, model) pair and dumping it would swamp any
            // log line that debug-prints the registry.
            .field(
                "max_tokens_table",
                &if self.max_tokens_table.is_some() {
                    "present"
                } else {
                    "absent"
                },
            )
            .field(
                "temperature_table",
                &if self.temperature_table.is_some() {
                    "present"
                } else {
                    "absent"
                },
            )
            .finish()
    }
}

impl ProviderRegistry {
    /// Build a registry from a list of named providers. Each entry
    /// is wrapped in its own default [`CircuitBreaker`] (catalog
    /// §D.19.5: 5 errors in 60 s, 30 s cooldown).
    ///
    /// When two or more entries share the same inner `Provider::name`
    /// (e.g. two `mock` instances with different registry keys), the
    /// registry also builds a [`ProviderPool`] for round-robin
    /// selection across those entries (D.19.19/.20).
    pub fn new(providers: Vec<(String, Arc<dyn Provider>)>) -> Self {
        let mut by_name = HashMap::new();
        let mut wrapped: HashMap<String, Arc<BreakeredProvider>> = HashMap::new();
        let mut wrapped_entries: Vec<(String, Arc<BreakeredProvider>)> = Vec::new();
        for (name, p) in providers {
            // Per-call-site breaker: each wrapper instance owns its
            // own breaker (constructed in `BreakeredProvider::new`
            // by the time we're done refactoring). For now the
            // wrapper still accepts an external breaker, but the
            // registry no longer mirrors it into a shared map —
            // a transient outage on one provider can no longer
            // amplify across the registry lookup table. The breaker
            // is constructed fresh per call site (one wrapper, one
            // breaker), so two wrappers created from the same inner
            // `Arc<dyn Provider>` independently observe their own
            // counters. Each fresh breaker uses the lenient
            // production defaults (50/300s/60s); tests that need
            // a fast-tripping 5/60s/30s breaker use
            // `CircuitBreaker::default()` directly.
            let breaker = Arc::new(CircuitBreaker::lenient());
            let entry: Arc<BreakeredProvider> = Arc::new(BreakeredProvider::new(p, breaker));
            by_name.insert(name.clone(), entry.clone() as Arc<dyn Provider>);
            wrapped.insert(name.clone(), entry.clone());
            wrapped_entries.push((name, entry));
        }
        let (pool, pool_names) = build_pool_from_entries(&wrapped_entries);
        Self {
            by_name,
            wrapped,
            pool,
            pool_names,
            max_tokens_table: None,
            temperature_table: None,
        }
    }

    /// Attach an auto-discovered `max_tokens` table to the registry.
    /// Consuming-builder form so `registry_from_config` can chain it
    /// onto a freshly built registry and hand-rolled test paths can
    /// opt in with one call.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        self.max_tokens_table = Some(table);
        self
    }

    /// The auto-discovered `max_tokens` table, when the auto-probe is
    /// enabled for at least one provider. `None` disables every
    /// probe-aware code path so callers fall back to the static
    /// `ProviderConfig::max_tokens` knob.
    pub fn max_tokens_table(&self) -> Option<&Arc<MaxTokensTable>> {
        self.max_tokens_table.as_ref()
    }

    /// Attach an auto-discovered supported-temperatures table to the
    /// registry. Mirrors [`Self::with_max_tokens_table`] — the
    /// consuming-builder form lets `registry_from_config_with_home_and_sink`
    /// chain it onto a freshly built registry, and hand-rolled test
    /// paths can opt in with one call.
    pub fn with_temperature_table(mut self, table: Arc<TemperatureTable>) -> Self {
        self.temperature_table = Some(table);
        self
    }

    /// The auto-discovered supported-temperatures table, when the
    /// home directory was resolvable and
    /// `temperatures_auto.toml` was loadable. `None` disables the
    /// temperature clamp and the per-cell rewrite — every call falls
    /// back to the operator's raw temperature value, so a hand-rolled
    /// test path keeps the legacy "send whatever you asked for"
    /// behaviour.
    pub fn temperature_table(&self) -> Option<&Arc<TemperatureTable>> {
        self.temperature_table.as_ref()
    }

    /// Look up a provider by registry key (section name or
    /// `"section::model_id"` for the v0.10 multi-model sections).
    /// Returns `None` when the key is not registered. Use
    /// [`Self::get_model`] to look up a specific `(section, model_id)`
    /// pair explicitly; the legacy single-string `get(section)` is
    /// retained as a thin shim for callers that operate on a single
    /// model per section (mock, deepseek-direct, …).
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.by_name.get(name).cloned()
    }

    /// Look up a specific `(section, model_id)` pair.
    ///
    /// Returns `None` when either the section or the model id is
    /// missing. The pair-keyed form is the v0.10 lookup signature
    /// — the dispatcher writes one entry per `(section, model_id)`
    /// into the registry and every LLM call site asks for the
    /// pair by name. The legacy `get(section)` shortcut stays as a
    /// backwards-compat shim for sections that only register a
    /// single model.
    pub fn get_model(&self, section: &str, model_id: &str) -> Option<Arc<dyn Provider>> {
        let key = Self::registry_key(section, model_id);
        self.by_name.get(&key).cloned()
    }

    /// Canonical registry key for a `(section, model_id)` pair.
    /// The legacy single-model sections (mock, deepseek-direct,
    /// minimax direct aliases) keep their plain section name as
    /// the key so old lookup code keeps working; multi-model
    /// sections register every model under
    /// `"{section}::{model_id}"`.
    pub fn registry_key(section: &str, model_id: &str) -> String {
        if section == model_id || model_id.is_empty() {
            section.to_owned()
        } else {
            format!("{section}::{model_id}")
        }
    }

    /// Insert a provider by registry key. Stores the provider as-is
    /// without re-wrapping. The per-call-site breaker fix lives in
    /// [`Self::new`] (the constructor used by production
    /// registries) and in
    /// [`registry_from_config_with_home_and_sink`] (the production
    /// entry point). This `insert` method is a manual / test
    /// shim that preserves the caller's wrapper — hand-rolled
    /// test paths (e.g. `src/phases/judge.rs` and
    /// `src/discovery/coordinator.rs`) insert bare
    /// `Arc<dyn Provider>` values like `MockProvider` or scripted
    /// providers without a breaker, and the registry just needs
    /// to expose them through `by_name`. The accessor
    /// [`Self::breaker`] and the sink walker
    /// [`Self::attach_saturation_sink`] operate on `self.wrapped`,
    /// which is populated only by [`Self::new`] (production) and
    /// [`Self::insert_wrapped`] (test shim that wants the wrapped
    /// state observable through the registry).
    pub fn insert(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.by_name.insert(name, provider);
    }

    /// Insert a pre-built [`BreakeredProvider`] wrapper. Mirrors
    /// the wrapper into both `by_name` (for `get` lookup) and
    /// `wrapped` (for `breaker`, `attach_saturation_sink`, and
    /// the per-call-site breaker accessor). This is the test shim
    /// `tests/integration_telemetry_saturation.rs` relies on to
    /// wire a wrapper with a specific breaker (`default()` /
    /// `lenient()` / hand-rolled) into the registry without
    /// losing the breaker reference. Production code paths go
    /// through [`Self::new`] which constructs the wrapper
    /// internally and never needs this shim.
    pub fn insert_wrapped(&mut self, name: String, wrapper: Arc<BreakeredProvider>) {
        self.wrapped.insert(name.clone(), wrapper.clone());
        self.by_name.insert(name, wrapper as Arc<dyn Provider>);
    }

    /// Read the circuit breaker for a registered provider. Returns
    /// `None` for registries built without the wrapper (hand-rolled
    /// paths that never went through [`Self::insert`] /
    /// [`registry_from_config_with_home_and_sink`]). The accessor
    /// delegates to the wrapper's own breaker rather than a
    /// registry-wide map, so two providers cannot trip each other
    /// through a shared `Arc<CircuitBreaker>`.
    pub fn breaker(&self, name: &str) -> Option<Arc<CircuitBreaker>> {
        self.wrapped.get(name).map(|w| w.breaker().clone())
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

    /// D.19.19/.20: round-robin pick of the next provider. Returns
    /// `None` when no pool is configured (single-instance configs,
    /// hand-rolled registries); callers should fall back to
    /// [`Self::get`] for the non-pool path. The returned `Arc<dyn
    /// Provider>` is the same wrapper registered via
    /// [`Self::insert`], so the
    /// breaker / rate-limiter / semaphore layer stays in front of
    /// the inner call.
    ///
    /// `allow_paused` toggles the D.19.20 gate: when `true`, the
    /// pool hands back the round-robin index even if every entry's
    /// breaker is open (useful for diagnostics / drain); when
    /// `false`, the pool skips open entries and returns the first
    /// available one in round-robin order, or `None` if every entry
    /// is paused.
    pub async fn pick(&self, allow_paused: bool) -> Option<Arc<dyn Provider>> {
        let pool = self.pool.as_ref()?;
        let idx = pool.pick(allow_paused).await?;
        let name = self.pool_names.get(idx)?;
        self.by_name.get(name).cloned()
    }

    /// D.19.19: attach a round-robin pool to the registry. Each
    /// tuple is wrapped in a [`BreakeredProvider`] with the supplied
    /// breaker and inserted into the registry under `name`; the
    /// resulting `Arc<BreakeredProvider>` is also fed into the
    /// [`ProviderPool`] so its `is_available` hook consults the
    /// per-entry breaker state. When two or more tuples share the
    /// same inner `Provider::name`, the pool is built and
    /// [`Self::pick`] does round-robin selection across those
    /// entries. With a single entry the registry stays on the
    /// legacy `HashMap`-only path.
    ///
    /// Use this for tests / hand-rolled registries; production
    /// callers should rely on `registry_from_config` which builds
    /// the pool automatically when the config has multiple
    /// instances of the same provider kind.
    ///
    /// Each tuple still carries the `Arc<CircuitBreaker>` because
    /// tests / hand-rolled paths need to drive a specific breaker
    /// (e.g. `breaker.trip()` to test `pick` skip-paused semantics).
    /// The breaker is owned by the wrapper, not by the registry —
    /// see the per-call-site breaker note on [`Self::insert`].
    pub fn with_pool(
        mut self,
        entries: Vec<(String, Arc<dyn Provider>, Arc<CircuitBreaker>)>,
    ) -> Self {
        let mut wrapped_entries: Vec<(String, Arc<BreakeredProvider>)> = Vec::new();
        for (name, provider, breaker) in entries {
            let wrapped: Arc<BreakeredProvider> =
                Arc::new(BreakeredProvider::new(provider, breaker));
            self.wrapped.insert(name.clone(), wrapped.clone());
            self.by_name
                .insert(name.clone(), wrapped.clone() as Arc<dyn Provider>);
            wrapped_entries.push((name, wrapped));
        }
        let (pool, pool_names) = build_pool_from_entries(&wrapped_entries);
        self.pool = pool;
        self.pool_names = pool_names;
        self
    }

    /// Attach a push-side [`SaturationSink`] to every wrapped
    /// provider in the registry. Used by the CLI pipeline after
    /// [`crate::telemetry::Telemetry`] has been opened so the
    /// `BreakeredProvider::send` rejections land in both
    /// `telemetry/saturation.jsonl` and the `saturation_events`
    /// SQLite mirror (catalog §D.23 + §D.27, v0.8; PR #494
    /// follow-up). Returns `self` for builder-style chaining.
    ///
    /// Hand-rolled registries that bypass the wrapper
    /// (`wrapped` empty) are left untouched — those callers
    /// already configure the sink directly on the underlying
    /// `BreakeredProvider`.
    pub fn with_saturation_sink(self, sink: Arc<dyn SaturationSink>) -> Self {
        for wrapped in self.wrapped.values() {
            wrapped.set_saturation_sink(sink.clone());
        }
        self
    }

    /// Same as [`Self::with_saturation_sink`] but takes `&self` so
    /// the call works on `Arc<ProviderRegistry>` after the registry
    /// has been shared into the run context. The two methods
    /// coexist: tests that build a registry step-by-step keep the
    /// consuming-builder form, and the production CLI pipeline
    /// uses this one because the registry is wrapped in an `Arc`
    /// by the time telemetry is opened.
    pub fn attach_saturation_sink(&self, sink: Arc<dyn SaturationSink>) {
        for wrapped in self.wrapped.values() {
            wrapped.set_saturation_sink(sink.clone());
        }
    }

    /// Read the configured [`SaturationSink`] (if any) for the
    /// provider registered under `name`. Returns `None` for
    /// registries built without the wrapper or before the sink
    /// has been attached. Used by integration tests to assert
    /// the wiring path.
    #[allow(dead_code)]
    pub fn saturation_sink(&self, name: &str) -> Option<Arc<dyn SaturationSink>> {
        self.wrapped.get(name)?.saturation_sink()
    }

    /// True when a round-robin pool is configured.
    pub fn has_pool(&self) -> bool {
        self.pool.is_some()
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
    /// Per-provider token-bucket rate limiter (catalog §D.19.6).
    /// `None` means no rate-limit backpressure — the wrapper just
    /// forwards calls. Wrapped in `Mutex<Option<_>>` (matching the
    /// `saturation_sink` field below) so the CLI plumbing can
    /// attach a limiter through `&self` after the wrapper has
    /// already been shared into `Arc<dyn Provider>`. The lock is
    /// only held for the duration of the read in `send`; the
    /// project's no-go rule against `Mutex<Option<T>>` for state
    /// targets global mutable state, not per-instance configuration
    /// fields, and `cancel.rs::CancelToken` uses the same shape.
    rate_limiter: Mutex<Option<Arc<RateLimiter>>>,
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
    /// Push-side saturation sink (catalog §D.23 + §D.27, v0.8).
    /// Fired when the wrapper rejects a call because the circuit
    /// breaker is open or the rate-limiter budget is exhausted.
    /// The sink stays optional so hand-rolled test paths that do
    /// not need telemetry can construct a wrapper without it; the
    /// production path wires it through
    /// [`Self::with_saturation_sink`] (consuming builder) or
    /// [`Self::set_saturation_sink`] (interior-mutability setter
    /// used by the registry wiring after the wrapper has been
    /// shared into an `Arc<dyn Provider>`).
    ///
    /// Wrapped in a `Mutex<Option<_>>` because the wrapper is
    /// normally held via `Arc<dyn Provider>` — every clone shares
    /// the same inner state, so the sink has to be assignable
    /// through `&self`. The lock is held only for the duration of
    /// `Sink::on_saturation` reads in `send`, which never block;
    /// the project's no-go rule against `Mutex<Option<T>>` for
    /// state targets global mutable state, not per-instance
    /// configuration fields, and `src/cancel.rs::CancelToken`
    /// already uses the same shape.
    saturation_sink: Mutex<Option<Arc<dyn SaturationSink>>>,
}

/// Push-side sink for [`crate::telemetry::saturation::SaturationEvent`].
///
/// Implemented by `Telemetry` (which mirrors the event into the
/// per-run JSONL stream and the SQLite index) and by the in-memory
/// `Vec<>` used by tests. The trait keeps the wrapper
/// telemetry-agnostic: the registry wires the right implementation
/// at construction time so the LLM call path never reaches into
/// the telemetry module directly.
pub trait SaturationSink: Send + Sync {
    /// Fire one saturation event. Implementations should be
    /// best-effort: a failure must not abort the LLM call.
    fn on_saturation(&self, event: &crate::telemetry::saturation::SaturationEvent);
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
                &self.rate_limiter.lock().as_ref().map(|_| "configured"),
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
            rate_limiter: Mutex::new(None),
            rate_limit_max_wait: None,
            provider_semaphores: None,
            saturation_sink: Mutex::new(None),
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
            rate_limiter: Mutex::new(Some(rate_limiter)),
            rate_limit_max_wait: None,
            provider_semaphores: None,
            saturation_sink: Mutex::new(None),
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

    /// Attach a push-side [`SaturationSink`] (catalog §D.23 +
    /// §D.27, v0.8). Every time the wrapper rejects a call because
    /// the breaker is open or the rate limiter exhausted its
    /// budget, the sink is invoked with the matching
    /// [`crate::telemetry::saturation::SaturationEvent`].
    ///
    /// Production callers wire the [`Telemetry`] handle; tests can
    /// pass a `Vec` collector through
    /// [`crate::llm::test_support::InMemorySaturationSink`] to
    /// assert on the fired events without standing up a database.
    pub fn with_saturation_sink(self, sink: Arc<dyn SaturationSink>) -> Self {
        *self.saturation_sink.lock() = Some(sink);
        self
    }

    /// Set the [`SaturationSink`] on a wrapper that is already
    /// shared (e.g. through `Arc<dyn Provider>` inside a
    /// [`ProviderRegistry`]). The setter uses interior mutability
    /// so the call does not need ownership of the wrapper; the
    /// registry wiring path invokes it after every `Arc` clone has
    /// been inserted.
    ///
    /// Calling this twice replaces the previous sink; tests that
    /// build a wrapper directly prefer the consuming
    /// [`Self::with_saturation_sink`] form which makes the
    /// construction order obvious.
    pub fn set_saturation_sink(&self, sink: Arc<dyn SaturationSink>) {
        *self.saturation_sink.lock() = Some(sink);
    }

    /// Read the configured [`SaturationSink`] (if any). Exposed so
    /// registry-level wiring can verify the sink was attached
    /// without going through `Debug` (which elides the trait
    /// object). Returns a clone of the `Arc` because the trait
    /// object lives behind a `Mutex` and the caller generally
    /// wants to share the handle across threads.
    #[allow(dead_code)]
    pub fn saturation_sink(&self) -> Option<Arc<dyn SaturationSink>> {
        self.saturation_sink.lock().clone()
    }

    /// Set the per-provider [`RateLimiter`] on a wrapper that is
    /// already shared (e.g. through `Arc<dyn Provider>` inside a
    /// [`ProviderRegistry`]). Mirrors the interior-mutability
    /// pattern of [`Self::set_saturation_sink`]: the lock is held
    /// only briefly, the `Arc` is shared with the rest of the
    /// registry. The CLI plumbing uses this to attach a
    /// `--max-parallelism`-derived rate limiter after
    /// [`super::registry_from_config_with_home_and_sink`] has
    /// already inserted the wrapper into the registry. Calling
    /// this twice replaces the previous limiter; the
    /// consuming-builder form [`Self::with_rate_limiter`] is the
    /// better fit for tests that want to bake the limiter into
    /// construction.
    pub fn set_rate_limiter(&self, rate_limiter: Arc<RateLimiter>) {
        *self.rate_limiter.lock() = Some(rate_limiter);
    }

    /// Borrow the inner provider (used by the probe spawner to reach
    /// past the wrapper without opening the breaker).
    fn inner(&self) -> &Arc<dyn Provider> {
        &self.inner
    }

    /// Borrow the wrapper's own breaker. Used by
    /// [`ProviderRegistry::breaker`] to expose per-provider breaker
    /// state without sharing the breaker across registry and
    /// wrapper (catalog §D.19.5). Each `BreakeredProvider` instance
    /// owns its breaker independent of every other wrapper, so the
    /// returned `Arc` is the unique breaker for this call site —
    /// failures recorded by `send` show up here, but a transient
    /// outage on a different provider never does.
    pub fn breaker(&self) -> &Arc<CircuitBreaker> {
        &self.breaker
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
        // v0.9.6: the per-provider circuit breaker check moved to
        // the call-site (`RunContext::call_with_retry_parse`) so it
        // can be keyed on `(provider, role)` instead of just
        // `provider`. The breaker stored on `BreakeredProvider` is
        // kept here ONLY for two things:
        //
        // 1. The provider pool's `is_available()` signal (consumed
        //    by `ProviderPool::pick`).
        // 2. Firing the saturation event when the breaker is
        //    externally tripped (admin `moagan admin breakers
        //    trip`, run pre-pause, etc.) so the alert consumer
        //    still sees the pause even though `send` itself does
        //    NOT short-circuit anymore.
        //
        // We DO NOT short-circuit on the open breaker: that was
        // the cascade bug in run02's `discover_facet` (one role's
        // PlanExhausted / 429 tripped the breaker for every role
        // on the same provider). The per-(provider, role) breaker
        // in `dispatch_with_governors` replaces the short-circuit
        // for runtime breaker logic; this saturation hook stays
        // only for telemetry.

        // Clone the `Arc` out of the mutex so the lock is released
        // before the `await` on `acquire` (parking_lot's lock is
        // not async-aware and would otherwise serialize every call).
        let rate_limiter: Option<Arc<RateLimiter>> = self.rate_limiter.lock().clone();
        if let Some(rl) = rate_limiter {
            let acquire_result = match self.rate_limit_max_wait {
                Some(max) => rl.acquire_with_max(max).await,
                None => rl.acquire().await.map(|_| Duration::ZERO),
            };
            if let Err(e) = acquire_result {
                // Rate-limiter rejection (catalog §D.19.6 → §D.23).
                // The bucket was empty at the configured `max_wait`
                // horizon, so the next refill would have exceeded
                // it. The threshold reported in the event is the
                // inverse of the bucket deficit: 0% means the bucket
                // was fully empty, larger values mean there were
                // still some tokens left (caller picked a strict
                // horizon).
                if let Some(sink) = self.saturation_sink.lock().clone() {
                    let ev = crate::telemetry::saturation::SaturationEvent::from_rate_limit(
                        self.inner.name(),
                        self.inner.model(),
                        None,
                        0.0,
                        rl.capacity(),
                        rl.refill_per_sec(),
                    );
                    sink.on_saturation(&ev);
                }
                return Err(e);
            }
            let _wait = acquire_result?;
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

        // v0.9.6: fire the saturation event when the legacy
        // per-provider breaker is externally tripped. This is the
        // ONLY remaining signal that the breaker produces; the
        // short-circuit on `is_open()` is gone (it caused the
        // cascade in `discover_facet`). The sink consumes the
        // event for the operator-facing alert path.
        if self.breaker.is_open()
            && let Some(sink) = self.saturation_sink.lock().clone()
        {
            let ev = crate::telemetry::saturation::SaturationEvent::from_circuit_breaker(
                self.inner.name(),
                self.inner.model(),
                None,
                self.breaker.failure_count(),
            );
            sink.on_saturation(&ev);
            // Note: do NOT return Err here. `send` falls through to
            // the inner call; the per-(provider, role) breaker
            // (driven by the call-site) is the one that will fail
            // fast on the NEXT call if the operator-driven trip is
            // meant to suppress the role entirely.
        }

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
                if cache_hit && let Some(rl) = self.rate_limiter.lock().clone() {
                    rl.refund();
                }
                Ok(pair)
            }
            Err(e) => {
                // v0.9.6: do NOT record_failure on the per-provider
                // breaker here. Persistent failures (PlanExhausted)
                // are routed to the per-(provider, role) breaker
                // owned by `RunContext`, NOT to this legacy breaker.
                // Transient failures (Throttled) are absorbed by
                // the `ThrottleGovernor`. Recoding here would have
                // cascaded role errors into the shared breaker.
                Err(e)
            }
        }
    }

    async fn count_tokens(&self, text: &str) -> Option<u64> {
        self.inner.count_tokens(text).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Delegate to the inner provider: the wrapper is transparent
        // and does not participate in the `max_tokens` clamp chain.
        self.inner.effective_max_tokens(req)
    }

    /// Delegate to the inner provider so the wrapper is transparent
    /// and the probe observes the inner provider's per-kind ceiling
    /// (e.g. `DEEPSEEK_MAX_TOKENS_CAP` on the direct DeepSeek
    /// provider, `u32::MAX` on OpenCode Go
    /// chat-completions, `MINIMAX_MAX_TOKENS_CAP` on minimax).
    /// Without this delegation the wrapper would inherit the trait
    /// default (`MAX_AUTOPROBE_CEILING` ≈ 1.07G) and the probe
    /// would waste round-trips on values the inner provider will
    /// never accept.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        self.inner.max_tokens_probe_ceiling()
    }
}

/// D.19.19/.20: `BreakeredProvider` reports its breaker state to the
/// pool through `ProviderPoolEntry::is_available`. When the breaker
/// is open, the entry is considered paused and `ProviderPool::pick`
/// (with `allow_paused = false`) skips it in favour of the next
/// round-robin candidate. The wrapper's `is_available` is the only
/// pause signal the pool consults — the inner provider is opaque to
/// the pool, so failures that don't open the breaker stay invisible
/// until `send` records them.
#[async_trait]
impl ProviderPoolEntry for BreakeredProvider {
    async fn is_available(&self) -> bool {
        !self.breaker.is_open()
    }
}

/// D.19.19/.20: build a round-robin pool when the supplied entries
/// contain two or more instances of the same provider kind (as
/// reported by `BreakeredProvider::name()`). Returns `(None, vec![])`
/// when no kind has duplicates — single-instance configs and the
/// hand-rolled test paths keep the legacy `HashMap`-only registry.
///
/// The pool covers every entry whose kind has duplicates, not the
/// full entry list, so registries with mixed kinds (e.g. one
/// `mock` plus two `minimax`) keep the singleton `mock` out of
/// the round-robin rotation and only spin the duplicate-kind
/// instances.
///
/// Insertion order is preserved so callers that build the entries
/// in a deterministic order (BTreeMap iteration in
/// `registry_from_config`, explicit `Vec` in tests) get a
/// deterministic round-robin sequence.
fn build_pool_from_entries(
    entries: &[(String, Arc<BreakeredProvider>)],
) -> (Option<Arc<ProviderPool>>, Vec<String>) {
    // First pass: tally per-kind counts while remembering the
    // order each kind was first seen. The order is what makes the
    // pool's round-robin sequence deterministic.
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut seen: Vec<String> = Vec::new();
    for (_, wrapped) in entries {
        let kind = wrapped.name().to_owned();
        if !counts.contains_key(&kind) {
            seen.push(kind.clone());
        }
        *counts.entry(kind).or_insert(0) += 1;
    }
    // Pick the first seen kind that has 2+ entries.
    let chosen_kind = seen
        .into_iter()
        .find(|k| counts.get(k).copied().unwrap_or(0) >= 2);
    let Some(kind) = chosen_kind else {
        return (None, Vec::new());
    };
    // Second pass: collect entries of the chosen kind, preserving
    // insertion order so the round-robin sequence is deterministic.
    let names: Vec<String> = entries
        .iter()
        .filter(|(_, w)| w.name() == kind)
        .map(|(n, _)| n.clone())
        .collect();
    let pool_entries: Vec<Arc<dyn ProviderPoolEntry>> = entries
        .iter()
        .filter(|(_, w)| w.name() == kind)
        .map(|(_, w)| w.clone() as Arc<dyn ProviderPoolEntry>)
        .collect();
    (Some(Arc::new(ProviderPool::new(pool_entries))), names)
}

/// Build a registry from a map of provider configurations. The
/// `minimax` and `deepseek` configurations are wired to their provider
/// implementations; everything else returns an explicit-not-implemented
/// error unless the user explicitly opts in.
///
/// Every provider is wrapped in a [`BreakeredProvider`] with the
/// breaker knobs from `breaker_cfg` (catalog §D.19.5). Per the
/// per-call-site breaker fix, the breaker is owned by the wrapper —
/// callers read state via [`ProviderRegistry::breaker`], which
/// delegates to the wrapper's own breaker rather than a registry-
/// wide map (no shared `Arc<CircuitBreaker>` survives after this
/// refactor).
///
/// When the config has multiple instances of the same provider kind
/// (e.g. two `mock` entries or two `minimax` entries), the registry
/// also builds a [`ProviderPool`] so [`ProviderRegistry::pick`] does
/// round-robin selection across those instances (D.19.19/.20).
///
/// Resolves `MOAGAN_HOME` to back the auto-discovered `max_tokens`
/// table. A home that cannot be resolved is not fatal: the registry
/// is built without the table and every probe-aware path falls back
/// to the static `ProviderConfig::max_tokens` knob.
pub fn registry_from_config(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
    breaker_cfg: &CircuitBreakerConfig,
) -> Result<ProviderRegistry> {
    registry_from_config_with_sink(cfg, breaker_cfg, None)
}

/// [`registry_from_config`] variant that also wires a push-side
/// [`SaturationSink`] onto every wrapped provider. Used by the
/// production CLI pipeline after [`crate::telemetry::Telemetry`]
/// has been opened so circuit-open and rate-limit rejections
/// land in both `telemetry/saturation.jsonl` and the
/// `saturation_events` SQLite mirror. `sink = None` is the
/// default; tests and tools that do not care about saturation
/// telemetry stay on the [`registry_from_config`] shorthand.
pub fn registry_from_config_with_sink(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
    breaker_cfg: &CircuitBreakerConfig,
    sink: Option<Arc<dyn SaturationSink>>,
) -> Result<ProviderRegistry> {
    match MoaganHome::resolve() {
        Ok(home) => registry_from_config_with_home_and_sink(cfg, breaker_cfg, Some(&home), sink),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "provider registry: MOAGAN_HOME could not be resolved; \
                 building without the max_tokens auto-probe table"
            );
            registry_from_config_with_home_and_sink(cfg, breaker_cfg, None, sink)
        }
    }
}

/// [`registry_from_config`] with an explicit home, so tests (and any
/// caller that already holds a [`MoaganHome`]) can point the
/// `max_tokens_auto.toml` table at a scratch directory instead of the
/// operator's real home. `home = None` skips the table entirely.
///
/// This constructor never blocks on the probe: it only loads whatever
/// the previous run persisted. The probe itself runs asynchronously
/// after construction and writes back into the same table.
pub fn registry_from_config_with_home(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
    breaker_cfg: &CircuitBreakerConfig,
    home: Option<&MoaganHome>,
) -> Result<ProviderRegistry> {
    registry_from_config_with_home_and_sink(cfg, breaker_cfg, home, None)
}

/// [`registry_from_config_with_home`] variant that also wires a
/// push-side [`SaturationSink`]. Single source of truth for the
/// builder logic; the sink-less shorthands delegate here with
/// `sink = None`.
pub fn registry_from_config_with_home_and_sink(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
    breaker_cfg: &CircuitBreakerConfig,
    home: Option<&MoaganHome>,
    sink: Option<Arc<dyn SaturationSink>>,
) -> Result<ProviderRegistry> {
    use super::anthropic_compat::AnthropicCompatProvider;
    use super::openai_compat::OpenAICompatProvider;
    use super::openai_compatible::OpenAICompatibleProvider;
    use super::wire_format::wire_format_from_url;
    use crate::config::ResolvedModelConfig;

    let mut by_name: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    let mut wrapped: HashMap<String, Arc<BreakeredProvider>> = HashMap::new();
    let mut wrapped_entries: Vec<(String, Arc<BreakeredProvider>)> = Vec::new();

    for (section_name, spec) in cfg {
        // Mock sections (any section whose endpoint starts with
        // `mock://`, or the canonical `mock` alias) have no upstream;
        // build one canned placeholder per registered model so the
        // pool can group identical `Provider::name()` entries.
        let is_mock = section_name == "mock"
            || spec
                .endpoint
                .as_deref()
                .is_some_and(|e| e.starts_with("mock://"))
            || spec.endpoint.is_none() && section_name.starts_with("mock");
        if is_mock {
            let models: Vec<crate::config::ModelConfig> = if spec.models.is_empty() {
                // Backwards-compat: synthesize one entry from the
                // section's `endpoint`. New configs carry the model
                // list explicitly so this branch is only hit by
                // legacy / hand-rolled callers.
                vec![crate::config::ModelConfig {
                    id: section_name.clone(),
                    endpoint: spec.endpoint.clone(),
                    max_tokens: None,
                }]
            } else {
                spec.models.clone()
            };
            for model_cfg in models {
                let key = ProviderRegistry::registry_key(section_name, &model_cfg.id);
                let breaker = Arc::new(CircuitBreaker::new(
                    breaker_cfg.threshold,
                    std::time::Duration::from_secs(breaker_cfg.window_secs),
                    std::time::Duration::from_secs(breaker_cfg.cooldown_secs),
                ));
                let entry: Arc<BreakeredProvider> = Arc::new(BreakeredProvider::new(
                    Arc::new(super::mock::MockProvider::empty()),
                    breaker,
                ));
                if let Some(s) = sink.as_ref() {
                    entry.set_saturation_sink(s.clone());
                }
                wrapped.insert(key.clone(), entry.clone());
                by_name.insert(key.clone(), entry.clone() as Arc<dyn Provider>);
                wrapped_entries.push((key, entry));
            }
            continue;
        }

        // Every non-mock section produces one `Provider` per
        // `models[]` entry. The wire format comes from the URL
        // path; the section name (and only the section name)
        // drives API-key lookup. Two providers keep their
        // per-section wrapper so the kind-level cap stays in
        // place:
        //
        // * `minimax` → `MinimaxProvider` (clamps at
        //   `MINIMAX_MAX_TOKENS_CAP`).
        // * `deepseek` → `DeepSeekProvider` (clamps at
        //   `DEEPSEEK_MAX_TOKENS_CAP`).
        let models: Vec<crate::config::ModelConfig> = if spec.models.is_empty() {
            vec![crate::config::ModelConfig {
                id: section_name.clone(),
                endpoint: spec.endpoint.clone(),
                max_tokens: None,
            }]
        } else {
            spec.models.clone()
        };

        for model_cfg in models {
            let endpoint = model_cfg
                .endpoint
                .clone()
                .or_else(|| spec.endpoint.clone())
                .ok_or_else(|| {
                    crate::Error::InvalidArgs(format!(
                        "provider '{section_name}' model '{}' has no endpoint \
                         (neither section nor model specifies one)",
                        model_cfg.id
                    ))
                })?;
            let wire_format = wire_format_from_url(&endpoint)?;
            let resolved = ResolvedModelConfig {
                section: section_name.clone(),
                id: model_cfg.id.clone(),
                endpoint: endpoint.clone(),
                max_tokens: model_cfg.max_tokens,
                temperature: spec.temperature,
                top_p: spec.top_p,
                wire_format,
                omit_max_tokens: spec.omit_max_tokens,
            };

            let provider: Arc<dyn Provider> = if section_name == "deepseek" {
                Arc::new(super::deepseek::DeepSeekProvider::from_resolved(&resolved)?)
            } else if section_name == "minimax" {
                Arc::new(super::minimax::MinimaxProvider::from_resolved(&resolved)?)
            } else {
                match wire_format {
                    super::wire_format::WireFormatId::Anthropic => {
                        Arc::new(AnthropicCompatProvider::from_resolved(&resolved)?)
                    }
                    super::wire_format::WireFormatId::OpenAI => {
                        Arc::new(OpenAICompatProvider::from_resolved(&resolved)?)
                    }
                    super::wire_format::WireFormatId::OpenAICompatible => {
                        Arc::new(OpenAICompatibleProvider::from_resolved(&resolved)?)
                    }
                }
            };

            let breaker = Arc::new(CircuitBreaker::new(
                breaker_cfg.threshold,
                std::time::Duration::from_secs(breaker_cfg.window_secs),
                std::time::Duration::from_secs(breaker_cfg.cooldown_secs),
            ));
            let entry: Arc<BreakeredProvider> = Arc::new(BreakeredProvider::new(provider, breaker));
            if let Some(s) = sink.as_ref() {
                entry.set_saturation_sink(s.clone());
            }
            let key = ProviderRegistry::registry_key(section_name, &model_cfg.id);
            wrapped.insert(key.clone(), entry.clone());
            by_name.insert(key.clone(), entry.clone() as Arc<dyn Provider>);
            wrapped_entries.push((key, entry));
        }
    }
    let (pool, pool_names) = build_pool_from_entries(&wrapped_entries);
    let mut registry = ProviderRegistry {
        by_name,
        wrapped,
        pool,
        pool_names,
        max_tokens_table: None,
        temperature_table: None,
    };
    if let Some(home) = home {
        if let Some(settings) = probe_settings(cfg) {
            tracing::info!(
                providers = cfg.len(),
                floor = settings.floor,
                save = settings.save,
                "max_tokens_auto: registry carrying the probe table; firing background probes"
            );
            let table = MaxTokensTable::from_home(home, settings.floor, settings.save)?;
            let table = Arc::new(table);
            spawn_pending_probes(&wrapped_entries, cfg, Arc::clone(&table));
            registry = registry.with_max_tokens_table(table);
        } else {
            tracing::info!("max_tokens_auto: no provider enabled the probe");
        }
        // PR-7: build the supported-temperatures table regardless
        // of whether the max_tokens probe is enabled. The
        // temperature probe is opt-out per (provider, model) (it
        // fires for every non-mock provider) because a stale or
        // empty supported set is strictly safer than the legacy
        // "send whatever the operator asked for" path — every
        // out-of-range temperature gets clamped to the nearest
        // supported value rather than producing an HTTP 400.
        match TemperatureTable::from_home(home, /* save= */ true) {
            Ok(table) => {
                let table = Arc::new(table);
                spawn_pending_temperature_probes(&wrapped_entries, Arc::clone(&table));
                tracing::info!(
                    providers = cfg.len(),
                    "temperature_probe: registry carrying the supported-set table; firing background probes"
                );
                registry = registry.with_temperature_table(table);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "temperature_probe: failed to build the supported-set table; \
                     every LLM call will skip the temperature clamp"
                );
            }
        }
    } else {
        tracing::info!(
            "auto-probe tables: no MOAGAN_HOME resolved; \
             building without max_tokens / temperature auto-probe tables"
        );
    }
    Ok(registry)
}

/// Attach a per-provider `RateLimiter` to every wrapper inside
/// `registry`, deriving the bucket size from
/// `--max-parallelism` (`effective_rate_limit`) but letting any
/// operator-supplied entry in `rate_limit_per_provider` win.
///
/// The CLI plumbing runs this AFTER
/// [`super::registry_from_config_with_home_and_sink`] so the
/// registry is already shared through `Arc<dyn Provider>` — the
/// setter is the only way to install a rate limiter at that
/// point without rebuilding every `Arc`. Catalog §D.19.6 says
/// the operator's explicit override (env var or
/// `[rate_limit_per_provider]` in `~/.config/moagan/config.toml`)
/// beats derived defaults, which is why this function looks up
/// the per-provider map first and falls back to
/// `effective_rate_limit` when the map is empty for that
/// provider.
///
/// `RateLimitConfig::default()` (capacity=60, refill=4) is
/// intentionally NOT consulted here. The pre-fix wiring did not
/// apply any per-provider rate limiter, so the previous default
/// was "no rate limiter at all" — this helper preserves that
/// profile when the CLI did not supply an effective rate limit
/// by short-circuiting on `effective_rate_limit`. Production
/// callers always supply `Some(_)` so `--max-parallelism=32`
/// genuinely produces 32 in flight instead of being throttled at
/// `refill_per_sec = 4`.
pub fn attach_parallelism_rate_limit(
    registry: &ProviderRegistry,
    effective_rate_limit: Option<&RateLimitConfig>,
    rate_limit_per_provider: &std::collections::HashMap<String, RateLimitConfig>,
) {
    let Some(default_cfg) = effective_rate_limit else {
        return;
    };
    for (name, wrapped) in &registry.wrapped {
        // Per-provider override wins (catalog §D.19.6); when
        // absent, use the parallelism-derived default so the
        // throttling scales with `--max-parallelism` instead of
        // the hardcoded `refill_per_sec = 4`.
        let rl_cfg = rate_limit_per_provider
            .get(name)
            .cloned()
            .unwrap_or_else(|| default_cfg.clone());
        wrapped.set_rate_limiter(Arc::new(RateLimiter::new(rl_cfg)));
    }
}

/// Aggregate the per-provider auto-probe knobs into the single
/// `(floor, save)` pair the shared [`MaxTokensTable`] carries.
/// Returns `None` when no provider enables the probe, which is the
/// signal to leave `ProviderRegistry::max_tokens_table` unset.
///
/// A provider opts in with `max_token_auto = Some(n)`, `n > 0`;
/// `None` and the `Some(0)` env sentinel both mean "off".
fn probe_settings(
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
) -> Option<ProbeSettings> {
    let mut settings: Option<ProbeSettings> = None;
    for spec in cfg.values() {
        let Some(floor) = spec.max_token_auto.filter(|n| *n > 0) else {
            continue;
        };
        let acc = settings.get_or_insert(ProbeSettings {
            floor: 0,
            save: false,
        });
        // The table holds one floor for every (provider, model) pair,
        // so mixed per-provider floors collapse to the highest: the
        // floor is a guarantee that we ask for at least `n`, and
        // taking the minimum would silently break that promise for
        // the provider that asked for more. A floor above a given
        // upstream's real ceiling makes only that provider's probe
        // fail, which degrades to the static `max_tokens` knob.
        acc.floor = acc.floor.max(floor);
        // Persist when any opted-in provider wants persistence; the
        // file is shared, and a single provider asking to remember
        // its ceiling is enough to justify writing it.
        acc.save |= spec.max_token_auto_save;
    }
    settings
}

/// Fire one background probe per opted-in `(provider, model)` pair.
/// Each probe writes its discovered value into the shared
/// [`MaxTokensTable`]; the pipeline consults the table on every
/// LLM call so the probe result takes effect as soon as it lands.
///
/// Providers with `max_token_auto = None` or `Some(0)` are skipped;
/// the `mock` provider is also skipped because it cannot answer a
/// probe (no real upstream). The `tokio::spawn` calls are recorded
/// as `JoinHandle`s on the table so the caller can `await_ready()`
/// if it wants to gate the first LLM call behind the discovery.
///
/// The probe deliberately bypasses the [`BreakeredProvider`] wrapper
/// (a failing probe must not poison the steady-state circuit) and
/// runs against the inner [`Provider`] via
/// [`ProviderProbeTransport`]. Per-call work is bounded by
/// [`super::probe::PROBE_TIMEOUT`] (5 s).
fn spawn_pending_probes(
    wrapped_entries: &[(String, Arc<BreakeredProvider>)],
    cfg: &std::collections::BTreeMap<String, ProviderConfig>,
    table: Arc<MaxTokensTable>,
) {
    for (name, wrapped) in wrapped_entries {
        let Some(spec) = cfg.get(name) else {
            continue;
        };
        // Honor the operator's per-provider opt-out: `None` or
        // `Some(0)` means "no probe".
        let Some(floor) = spec.max_token_auto.filter(|n| *n > 0) else {
            tracing::debug!(
                provider = %name,
                "max_tokens_auto: provider opted out of the probe"
            );
            continue;
        };
        // Mock has no upstream; the probe would burn 30 HTTP calls
        // against a canned-response queue. Skip it.
        let inner = wrapped.inner();
        if inner.name() == "mock" {
            continue;
        }
        table.set_floor_for(name, inner.model(), floor);
        // Query the per-provider probe ceiling so the exponential
        // phase short-circuits at the first `2^k` past the
        // upstream's hard cap. DeepSeek-direct caps at 393_216
        // (DEEPSEEK_MAX_TOKENS_CAP), MiniMax at 524_288
        // (MINIMAX_MAX_TOKENS_CAP), OpenCode Go at 16_384
        // (u32::MAX); providers without a
        // documented ceiling inherit the trait default
        // (≈ 1.07G). Without this query the algorithm would burn
        // 30 sequential HTTP round-trips probing values the
        // upstream will reject — and the rejections would classify
        // as `Indeterminate` per the v0.7.1 contract, collapsing
        // the discovered ceiling to the last accepted probe.
        let ceiling = inner.max_tokens_probe_ceiling();
        let transport = match super::probe::ProviderProbeTransport::new(Arc::clone(inner)) {
            Ok(t) => Arc::new(t) as Arc<dyn super::probe::ProbeTransport>,
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    model = %inner.model(),
                    error = %e,
                    "max_tokens_auto: failed to build probe transport; skipping"
                );
                continue;
            }
        };
        let table_for_task = Arc::clone(&table);
        let provider_name = name.clone();
        let model_name = inner.model().to_owned();
        let provider_name_for_handle = provider_name.clone();
        let model_name_for_handle = model_name.clone();
        let handle = tokio::spawn(async move {
            tracing::info!(
                provider = %provider_name,
                model = %model_name,
                "max_tokens_auto: probe task spawned; verifying cached entry"
            );
            let verified = table_for_task
                .verify(&provider_name, &model_name, Arc::clone(&transport))
                .await
                .unwrap_or(false);
            if !verified {
                tracing::info!(
                    provider = %provider_name,
                    model = %model_name,
                    "max_tokens_auto: no usable cached entry; running full probe"
                );
                match table_for_task
                    .probe_and_store(&provider_name, &model_name, transport, ceiling)
                    .await
                {
                    Ok(value) => tracing::info!(
                        provider = %provider_name,
                        model = %model_name,
                        discovered = value,
                        "max_tokens_auto: discovered value"
                    ),
                    Err(e) => tracing::warn!(
                        provider = %provider_name,
                        model = %model_name,
                        error = %e,
                        "max_tokens_auto: probe failed; provider will use the static `max_tokens` knob"
                    ),
                }
            } else {
                tracing::info!(
                    provider = %provider_name,
                    model = %model_name,
                    "max_tokens_auto: cached entry verified"
                );
            }
        });
        table.record_probe_join_handle(provider_name_for_handle, model_name_for_handle, handle);
    }
}

/// PR-7: fire one background temperature probe per `(provider,
/// model)` pair not already cached on the [`TemperatureTable`].
///
/// The temperature probe is opt-out (every non-mock provider is
/// probed automatically) because an empty or stale supported-set
/// table degrades safely — the runtime clamps every out-of-range
/// temperature to the nearest cached value, so a slow probe on
/// first run is strictly less harmful than not probing at all. The
/// `mock` provider is skipped because it has no real upstream to
/// answer the probe. Each `tokio::spawn` task first verifies the
/// cached entry (single-probe, cheap) and only runs the full
/// 21-point fan-out when the cache is missing or rejected.
///
/// The probe deliberately bypasses the [`BreakeredProvider`]
/// wrapper (a failing probe must not poison the steady-state
/// circuit) and runs against the inner [`Provider`] via
/// [`super::temperature_probe::ProviderTemperatureProbeTransport`].
/// Per-call work is bounded by
/// [`super::temperature_probe::PROBE_TIMEOUT`] (5 s) and the
/// fan-out is batched at
/// [`super::temperature_probe::TEMPERATURE_PROBE_BATCH_SIZE`] (3).
fn spawn_pending_temperature_probes(
    wrapped_entries: &[(String, Arc<BreakeredProvider>)],
    table: Arc<TemperatureTable>,
) {
    for (name, wrapped) in wrapped_entries {
        let inner = wrapped.inner();
        if inner.name() == "mock" {
            continue;
        }
        // Skip pairs already cached: a fresh probe on every run
        // would burn 21 HTTP calls per provider per run against
        // sets the operator has already verified. A stale entry
        // survives until the verify step rejects it, at which
        // point the full probe replaces it.
        if table.get(name, inner.model()).is_some() {
            continue;
        }
        let transport = match super::temperature_probe::ProviderTemperatureProbeTransport::new(
            Arc::clone(inner),
        ) {
            Ok(t) => Arc::new(t) as Arc<dyn super::temperature_probe::TemperatureProbeTransport>,
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    model = %inner.model(),
                    error = %e,
                    "temperature_probe: failed to build probe transport; skipping"
                );
                continue;
            }
        };
        let table_for_task = Arc::clone(&table);
        let provider_name = name.clone();
        let model_name = inner.model().to_owned();
        let handle = tokio::spawn(async move {
            tracing::info!(
                provider = %provider_name,
                model = %model_name,
                "temperature_probe: probe task spawned; verifying cached entry"
            );
            let verified = table_for_task
                .verify(&provider_name, &model_name, Arc::clone(&transport))
                .await
                .unwrap_or(false);
            if !verified {
                tracing::info!(
                    provider = %provider_name,
                    model = %model_name,
                    "temperature_probe: no usable cached entry; running full probe"
                );
                match table_for_task
                    .probe_and_store(
                        &provider_name,
                        &model_name,
                        transport,
                        TEMPERATURE_PROBE_BATCH_SIZE,
                    )
                    .await
                {
                    Ok(value) => tracing::info!(
                        provider = %provider_name,
                        model = %model_name,
                        discovered = ?value,
                        "temperature_probe: discovered supported set"
                    ),
                    Err(e) => tracing::warn!(
                        provider = %provider_name,
                        model = %model_name,
                        error = %e,
                        "temperature_probe: probe failed; provider will skip the \
                         supported-set clamp and the runtime will fall back to the \
                         operator's raw temperature value"
                    ),
                }
            } else {
                tracing::info!(
                    provider = %provider_name,
                    model = %model_name,
                    "temperature_probe: cached entry verified"
                );
            }
        });
        table.record_probe_join_handle(handle);
    }
}

/// Aggregate the per-provider auto-probe knobs into the single
/// `(floor, save)` pair the shared [`MaxTokensTable`] carries.
/// Returns `None` when no provider enables the probe, which is the
/// signal to leave `ProviderRegistry::max_tokens_table` unset.
///
/// A provider opts in with `max_token_auto = Some(n)`, `n > 0`;
/// `None` and the `Some(0)` env sentinel both mean "off".
///
/// Aggregated auto-probe knobs for the shared table.
struct ProbeSettings {
    floor: u32,
    save: bool,
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

    // ----------------------------------------------------------------
    // ProviderPool wiring tests (PR-09, D.19.19/.20).
    //
    // `ProviderRegistry` builds a `ProviderPool` whenever the
    // config has multiple instances of the same provider kind.
    // Tests below pin three behaviours:
    //
    // 1. Single-instance configs keep the legacy `HashMap`-only
    //    registry: `has_pool()` is false and `pick()` returns
    //    `None` so callers fall back to `get(name)`.
    // 2. Two `mock` entries flip the pool on and `pick()` walks
    //    them in alternating order via the pool's atomic
    //    round-robin counter.
    // 3. Mixed-kind configs (1 mock + 2 minimax) only pool the
    //    duplicate-kind entries; the singleton `mock` stays out
    //    of the round-robin rotation.
    // ----------------------------------------------------------------

    /// Two `mock` entries must produce a pool of size 2 whose
    /// entries match the registry names.
    /// v0.10 pin: a single section with two model entries produces
    /// two `Provider` instances under the same section name, which
    /// the pool builder groups together for round-robin selection.
    /// (Pre-v0.10 this required two BTreeMap entries with the same
    /// `kind`; v0.10's `models[]` list is the new equivalent.)
    #[tokio::test]
    async fn registry_from_config_with_two_mocks_builds_pool() {
        let mut cfg = std::collections::BTreeMap::new();
        cfg.insert(
            "mock".into(),
            crate::config::ProviderConfig {
                endpoint: None,
                models: vec![
                    crate::config::ModelConfig {
                        max_tokens: None,
                        id: "mock-a".into(),
                        endpoint: None,
                    },
                    crate::config::ModelConfig {
                        max_tokens: None,
                        id: "mock-b".into(),
                        endpoint: None,
                    },
                ],
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_save: true,
                plan: None,
            },
        );
        let r = registry_from_config(&cfg, &CircuitBreakerConfig::default()).unwrap();
        // v0.10: the registry exposes every (section, model) pair
        // under the joined `"{section}::{model_id}"` key so the
        // pool can group identical `Provider::name()` entries.
        // `has_pool()` is true when more than one entry shares a
        // name; the pool key for the two mocks is
        // `"mock::mock-a"` / `"mock::mock-b"`.
        assert!(
            r.has_pool(),
            "two mocks under one section must build a pool"
        );
        assert!(
            r.iter().any(|(n, _)| n == "mock::mock-a"),
            "registry must register `mock::mock-a`"
        );
        assert!(
            r.iter().any(|(n, _)| n == "mock::mock-b"),
            "registry must register `mock::mock-b`"
        );
    }

    /// Single-instance configs must NOT build a pool so the legacy
    /// `HashMap`-only path stays bit-for-bit equivalent.
    #[tokio::test]
    async fn registry_from_config_with_single_mock_has_no_pool() {
        let mut cfg = std::collections::BTreeMap::new();
        cfg.insert(
            "mock".into(),
            crate::config::ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_save: true,
                plan: None,
            },
        );
        let r = registry_from_config(&cfg, &CircuitBreakerConfig::default()).unwrap();
        assert!(!r.has_pool(), "single instance must not build a pool");
        assert!(r.pick(false).await.is_none());
        // The non-pool path still resolves by name.
        assert!(r.get("mock").is_some());
    }

    /// `pick()` returns the next provider via round-robin. With
    /// two entries sharing the same kind, consecutive calls must
    /// alternate between them.
    #[tokio::test]
    async fn registry_pick_round_robins_between_two_mocks() {
        use crate::llm::mock::{MockProvider, MockResponse};
        let mut mock_a = MockProvider::new(vec![MockResponse::plain("a-1")]);
        mock_a.set_endpoint("mock://a");
        let mut mock_b = MockProvider::new(vec![MockResponse::plain("b-1")]);
        mock_b.set_endpoint("mock://b");
        let mock_a: Arc<dyn Provider> = Arc::new(mock_a);
        let mock_b: Arc<dyn Provider> = Arc::new(mock_b);
        let breaker_a = Arc::new(CircuitBreaker::default());
        let breaker_b = Arc::new(CircuitBreaker::default());
        let registry = ProviderRegistry::default().with_pool(vec![
            ("mock-a".to_owned(), mock_a, breaker_a),
            ("mock-b".to_owned(), mock_b, breaker_b),
        ]);
        let p1 = registry.pick(false).await.expect("first pick");
        let p2 = registry.pick(false).await.expect("second pick");
        let p3 = registry.pick(false).await.expect("third pick");
        let p4 = registry.pick(false).await.expect("fourth pick");
        // Round-robin alternates 0 -> 1 -> 0 -> 1.
        assert_eq!(p1.endpoint(), "mock://a");
        assert_eq!(p2.endpoint(), "mock://b");
        assert_eq!(p3.endpoint(), "mock://a");
        assert_eq!(p4.endpoint(), "mock://b");
    }

    /// `allow_paused = true` returns the round-robin index even
    /// when every entry's breaker is open. This is the D.19.20
    /// "diagnostic / drain" mode.
    #[tokio::test]
    async fn registry_pick_allow_paused_returns_round_robin_when_open() {
        use crate::llm::mock::{MockProvider, MockResponse};
        let mut mock_a = MockProvider::new(vec![MockResponse::plain("a-1")]);
        mock_a.set_endpoint("mock://a");
        let mut mock_b = MockProvider::new(vec![MockResponse::plain("b-1")]);
        mock_b.set_endpoint("mock://b");
        let mock_a: Arc<dyn Provider> = Arc::new(mock_a);
        let mock_b: Arc<dyn Provider> = Arc::new(mock_b);
        let breaker_a = Arc::new(CircuitBreaker::new(
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        ));
        let breaker_b = Arc::new(CircuitBreaker::new(
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        ));
        // Open both breakers by recording one failure each.
        breaker_a.record_failure();
        breaker_b.record_failure();
        let registry = ProviderRegistry::default().with_pool(vec![
            ("mock-a".to_owned(), mock_a, breaker_a),
            ("mock-b".to_owned(), mock_b, breaker_b),
        ]);
        // `pick(false)` skips both paused entries → None. The call
        // also advances the pool's counter, so the next two
        // `pick(true)` calls hand out indices 1 then 0.
        assert!(registry.pick(false).await.is_none());
        let p1 = registry.pick(true).await.expect("allow_paused first");
        let p2 = registry.pick(true).await.expect("allow_paused second");
        assert_eq!(p1.endpoint(), "mock://b");
        assert_eq!(p2.endpoint(), "mock://a");
    }

    /// v0.10 pin: the legacy `BLOCKED_MODELS` gate
    /// (`opencode_go.rs::BLOCKED_MODELS`) is gone — the operator
    /// controls which models route through OpenCode by choosing
    /// what to put in their `config.toml`. The dispatcher no
    /// longer refuses any alias. Verify the registry accepts the
    /// formerly-blocked `minimax-m3` model id without complaint.
    #[tokio::test]
    async fn registry_from_config_accepts_minimax_m3_no_blocked_gate() {
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "dummy-for-test");
        }
        let mut cfg = std::collections::BTreeMap::new();
        cfg.insert(
            "minimax".into(),
            crate::config::ProviderConfig {
                models: vec![crate::config::ModelConfig {
                    max_tokens: None,
                    id: "minimax-m3".into(),
                    endpoint: None,
                }],
                endpoint: Some("https://api.minimax.io/anthropic/v1/messages".to_owned()),
                temperature: Some(0.6),
                top_p: Some(0.95),
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_save: true,
                plan: None,
            },
        );
        let registry = registry_from_config(&cfg, &CircuitBreakerConfig::default());
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
        }
        // v0.10: the registry builds without rejecting any alias.
        // The minimax-m3 entry routes through MinimaxProvider (per-
        // section wrapper, section name == "minimax") and lands
        // under the joined key "minimax::minimax-m3" so the test
        // can pin the canonical (section, model_id) pair.
        let registry = registry.expect("registry must build without the BLOCKED_MODELS gate");
        let key = crate::llm::provider::ProviderRegistry::registry_key("minimax", "minimax-m3");
        let provider = registry
            .get(&key)
            .expect("minimax::minimax-m3 entry must be present");
        assert_eq!(provider.name(), "minimax");
        assert_eq!(provider.model(), "minimax-m3");
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
        Error::Provider {
            message: "upstream 503".into(),
            http_status: None,
        }
    }

    fn breaker_request() -> Request {
        Request {
            model: "MiniMax-M3".into(),
            role: Role::Intake,
            system: String::new(),
            user: String::new(),
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

    fn threshold_one_breaker() -> Arc<CircuitBreaker> {
        Arc::new(CircuitBreaker::new(
            1,
            Duration::from_secs(60),
            Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn breakered_provider_does_not_record_circuit_opening_failure() {
        // v0.9.6: per-provider circuit breaker recording moved to
        // the call-site (`RunContext::breaker_for(provider, role)`).
        // The legacy `BreakeredProvider.breaker` is kept ONLY for the
        // provider pool's `is_available` signal — `send` no longer
        // records failures into it. This test pins the new contract:
        // after a circuit-opening error the per-provider breaker
        // stays at zero failures; the per-(provider, role) breaker
        // (not present here) is what would trip.
        let inner = Arc::new(ErrorProvider {
            calls: AtomicUsize::new(0),
            error: opening_error,
        });
        let breaker = threshold_one_breaker();
        let provider = BreakeredProvider::new(inner.clone(), breaker.clone());

        let result = provider.send(&breaker_request()).await;
        assert!(matches!(result, Err(Error::Provider { .. })));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(breaker.failure_count(), 0);
        assert!(!breaker.is_open());
    }

    #[tokio::test]
    async fn breakered_provider_does_not_skip_calls_when_breaker_tripped_externally() {
        // v0.9.6: since `send` no longer consults the per-provider
        // breaker, a breaker that has been externally tripped (e.g.
        // by an admin `moagan admin breakers trip` command) does
        // not stop `send` from dispatching — that's now the
        // responsibility of `RunContext::breaker_for(provider, role)`.
        // This test pins the new contract: `send` is a thin passthrough.
        let inner = Arc::new(ErrorProvider {
            calls: AtomicUsize::new(0),
            error: opening_error,
        });
        let breaker = threshold_one_breaker();
        breaker.trip();
        let provider = BreakeredProvider::new(inner.clone(), breaker.clone());

        let result = provider.send(&breaker_request()).await;
        // `send` no longer short-circuits on a tripped breaker; the
        // upstream call goes through.
        assert!(result.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn minimax_provider_does_not_record_circuit_opening() {
        // v0.9.6: same contract as
        // `breakered_provider_does_not_record_circuit_opening_failure`
        // but exercised through the real `MinimaxProvider` (which
        // sets its own retry-on-429 — see `minimax_skips_circuit_recording_on_429`
        // above for the rate-limit retry path). After a failure the
        // per-provider breaker stays at zero; the per-(provider,
        // role) breaker is what would trip.
        let spec = ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
        };
        let minimax = MinimaxProvider::new(&spec, crate::secret::SecretString::new("dummy".into()))
            .unwrap()
            .with_max_retries(1);
        let breaker = threshold_one_breaker();
        let provider = BreakeredProvider::new(Arc::new(minimax), breaker.clone());

        let result = provider.send(&breaker_request()).await;
        assert!(result.is_err());
        // v0.9.6: per-provider breaker stays closed; per-(provider,
        // role) breaker is what triggers.
        assert!(!breaker.is_open());
        assert_eq!(breaker.failure_count(), 0);
    }

    fn sample_request() -> Request {
        Request {
            model: "MiniMax-M3".into(),
            role: Role::Intake,
            system: "sys".into(),
            user: "user".into(),
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
            Error::Provider { message, .. } => {
                assert!(
                    message.contains("budget exhausted"),
                    "overflow error must mention budget exhausted, got: {message}"
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

    use crate::llm::anthropic_compat::AnthropicCompatProvider;
    use crate::llm::deepseek::DeepSeekProvider;
    use crate::llm::minimax::MinimaxProvider;
    use crate::llm::openai_compat::OpenAICompatProvider;
    use crate::llm::openai_compatible::OpenAICompatibleProvider;

    /// Per-provider capability pin. Every concrete provider must
    /// declare its wire-format preference; the table here mirrors
    /// the constructor matrix in `capabilities.rs`.
    #[test]
    fn provider_capabilities_for_each_provider() {
        let cfg = crate::config::ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
        };
        let minimax =
            MinimaxProvider::new(&cfg, crate::secret::SecretString::new("dummy".into())).unwrap();
        let cap = minimax.capabilities();
        assert!(cap.prefers_anthropic_wire, "minimax must prefer anthropic");
        assert_eq!(cap.wire_format_id(), "anthropic");
        assert!(!cap.supports_response_format);

        let cfg_d = crate::config::ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
        };
        let deepseek =
            DeepSeekProvider::new(&cfg_d, crate::secret::SecretString::new("dummy".into()))
                .unwrap();
        let cap = deepseek.capabilities();
        assert!(cap.prefers_openai_wire, "deepseek must prefer openai");
        assert_eq!(cap.wire_format_id(), "openai_compatible");
        assert!(cap.supports_response_format);

        let cfg_oc = crate::config::ProviderConfig {
            models: vec![crate::config::ModelConfig {
                id: "qwen3.7-max".into(), // Anthropic-compat path
                endpoint: None,
                max_tokens: None,
            }],
            endpoint: None,
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
        };
        let oc_a =
            AnthropicCompatProvider::new(&cfg_oc, crate::secret::SecretString::new("dummy".into()))
                .unwrap();
        assert_eq!(oc_a.capabilities().wire_format_id(), "anthropic");

        let cfg_ocr = crate::config::ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            ..cfg_oc.clone()
        };
        let oc_r =
            OpenAICompatProvider::new(&cfg_ocr, crate::secret::SecretString::new("dummy".into()))
                .unwrap();
        assert_eq!(oc_r.capabilities().wire_format_id(), "responses");

        let cfg_occ = crate::config::ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            ..cfg_oc.clone()
        };
        let oc = OpenAICompatibleProvider::new(
            &cfg_occ,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        // Chat-completions OpenAI-compat inner reports `"openai"`.
        assert_eq!(oc.capabilities().wire_format_id(), "openai_compatible");

        let cfg_dispatcher = crate::config::ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            ..cfg_oc.clone()
        };
        let oc_d_anthropic = AnthropicCompatProvider::new(
            &cfg_dispatcher,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        // Anthropic-routed provider reports `anthropic`.
        assert_eq!(oc_d_anthropic.capabilities().wire_format_id(), "anthropic");

        let cfg_dispatcher_r = crate::config::ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            ..cfg_oc.clone()
        };
        let oc_d_responses = OpenAICompatProvider::new(
            &cfg_dispatcher_r,
            crate::secret::SecretString::new("dummy".into()),
        )
        .unwrap();
        // Responses-routed provider reports `responses`.
        assert_eq!(oc_d_responses.capabilities().wire_format_id(), "responses");

        let mock_cap = MockProvider::empty().capabilities();
        assert_eq!(mock_cap.wire_format_id(), "openai_compatible");
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
            OpenAICompatibleProvider::new(
                &crate::config::ProviderConfig {
                    endpoint: None,
                    models: Vec::new(),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                crate::secret::SecretString::new("dummy".into()),
            )
            .unwrap(),
        );
        let wrapped = BreakeredProvider::new(inner_oai, Arc::new(CircuitBreaker::default()));
        assert_eq!(wrapped.capabilities().wire_format_id(), "openai_compatible");
        assert_eq!(wrapped.wire_format_id(), "openai_compatible");
        assert!(wrapped.capabilities().supports_response_format);

        // Anthropic inner: dispatcher flips to the Anthropic wire.
        let inner_anth: Arc<dyn Provider> = Arc::new(
            MinimaxProvider::new(
                &crate::config::ProviderConfig {
                    endpoint: None,
                    models: Vec::new(),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
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
            OpenAICompatProvider::new(
                &crate::config::ProviderConfig {
                    endpoint: None,
                    models: Vec::new(),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
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
            model: "MiniMax-M3".into(),
            role: Role::Intake,
            system: "sys".into(),
            user: "u".into(),
            max_tokens: 64,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
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
            model: "MiniMax-M3".into(),
            role: Role::Intake,
            system: "instructions".into(),
            user: "the user prompt".into(),
            max_tokens: 32,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
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

    // ----------------------------------------------------------------
    // max_tokens auto-probe table wiring (feat/max-tokens-auto).
    //
    // `registry_from_config_with_home` attaches a `MaxTokensTable`
    // only when at least one provider opts in with
    // `max_token_auto = Some(n)`, `n > 0`. Construction never blocks
    // on the probe: it loads whatever the previous run persisted and
    // returns immediately.
    // ----------------------------------------------------------------

    /// Build a single-`mock` provider map with the given auto-probe
    /// floor, so the table tests do not need network-backed kinds.
    fn probe_cfg(
        max_token_auto: Option<u32>,
    ) -> std::collections::BTreeMap<String, ProviderConfig> {
        let mut cfg = std::collections::BTreeMap::new();
        cfg.insert(
            "mock".into(),
            ProviderConfig {
                endpoint: None,
                models: vec![crate::config::ModelConfig {
                    max_tokens: None,
                    id: "mock-model".into(),
                    endpoint: None,
                }],
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto,
                max_token_auto_save: true,
                plan: None,
            },
        );
        cfg
    }

    /// `max_token_auto = Some(0)` on every provider is the "off"
    /// sentinel: no table is attached, so every probe-aware path
    /// falls back to the static `max_tokens` knob.
    #[test]
    fn registry_without_probe_has_no_max_tokens_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let cfg = probe_cfg(Some(0));
        let reg =
            registry_from_config_with_home(&cfg, &CircuitBreakerConfig::default(), Some(&home))
                .expect("registry builds");
        assert!(
            reg.max_tokens_table().is_none(),
            "max_token_auto = Some(0) must not attach a table"
        );
        // `None` is the other spelling of "off".
        let cfg = probe_cfg(None);
        let reg =
            registry_from_config_with_home(&cfg, &CircuitBreakerConfig::default(), Some(&home))
                .expect("registry builds");
        assert!(
            reg.max_tokens_table().is_none(),
            "max_token_auto = None must not attach a table"
        );
    }

    /// `max_token_auto = Some(4096)` attaches a table carrying that
    /// floor. The table starts empty (nothing persisted yet) and the
    /// call returns without probing anything.
    #[test]
    fn registry_with_probe_carries_table_and_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let cfg = probe_cfg(Some(4096));
        let reg =
            registry_from_config_with_home(&cfg, &CircuitBreakerConfig::default(), Some(&home))
                .expect("registry builds");
        let table = reg
            .max_tokens_table()
            .expect("max_token_auto = Some(4096) must attach a table");
        assert_eq!(table.floor(), 4096);
        assert!(
            table.is_empty(),
            "a fresh home has nothing persisted, so the table starts empty"
        );
    }

    /// The floor below `MIN_AUTOPROBE_FLOOR` is clamped up by
    /// `MaxTokensTable`, so the shipped default of `Some(1024)`
    /// lands exactly on the minimum.
    #[test]
    fn registry_table_floor_is_clamped_to_minimum() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let cfg = probe_cfg(Some(1));
        let reg =
            registry_from_config_with_home(&cfg, &CircuitBreakerConfig::default(), Some(&home))
                .expect("registry builds");
        let table = reg.max_tokens_table().expect("table attached");
        assert_eq!(
            table.floor(),
            super::super::probe::MIN_AUTOPROBE_FLOOR,
            "a sub-minimum floor is raised to MIN_AUTOPROBE_FLOOR"
        );
    }

    /// Mixed per-provider floors collapse to the highest, because the
    /// shared table carries a single floor and the floor is a
    /// guarantee to ask for at least `n`.
    ///
    /// v0.10 pin: the legacy `mock-loud` / `mock-off` test used
    /// two distinct BTreeMap keys with the same `kind`. With the
    /// new `models[]` schema, the dispatcher builds one provider
    /// per `(section, model)` pair, so two providers under the
    /// same section require two model entries (not two keys).
    /// The "highest opted-in" contract still holds: the dispatcher
    /// walks every model, so two `max_token_auto` values inside
    /// one section land in the table.
    #[test]
    fn registry_table_floor_takes_the_highest_opted_in_provider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let mut cfg = probe_cfg(Some(2048));
        // Add a second model to the same `mock` section with a
        // larger floor. The dispatcher iterates both entries, and
        // the shared table must take the maximum.
        let mut loud = cfg["mock"].clone();
        loud.models.push(crate::config::ModelConfig {
            id: "mock-loud".into(),
            endpoint: None,
            max_tokens: None,
        });
        loud.max_token_auto = Some(16_384);
        cfg.insert("mock".into(), loud);
        // An opted-out model must not drag the floor back down.
        let mut off = cfg["mock"].clone();
        off.max_token_auto = Some(0);
        cfg.insert("mock-off".into(), off);
        let reg =
            registry_from_config_with_home(&cfg, &CircuitBreakerConfig::default(), Some(&home))
                .expect("registry builds");
        assert_eq!(
            reg.max_tokens_table().expect("table attached").floor(),
            16_384
        );
    }

    /// An unresolvable home is not fatal: the registry is built
    /// without a table and every caller falls back to the static
    /// `max_tokens` knob.
    #[test]
    fn registry_without_home_skips_the_table() {
        let cfg = probe_cfg(Some(4096));
        let reg = registry_from_config_with_home(&cfg, &CircuitBreakerConfig::default(), None)
            .expect("registry builds without a home");
        assert!(reg.max_tokens_table().is_none());
        // v0.10: the registry keys mock entries under
        // `"{section}::{model_id}"` when the two diverge, so the
        // probe fixture's `mock-model` model id produces the key
        // `"mock::mock-model"`. The section-name shortcut only
        // applies when the section name equals the model id.
        assert!(
            reg.get("mock::mock-model").is_some(),
            "providers are still wired"
        );
    }

    /// The env kill-switch reaches the registry: `MOAGAN_MAX_TOKEN_AUTO=0`
    /// rewrites every provider to the `Some(0)` sentinel, so
    /// `registry_from_config_with_home` attaches no table. Pins the
    /// config -> registry seam end-to-end.
    #[test]
    fn env_max_token_auto_zero_disables_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        // Sanity: the same config with the probe on does attach one.
        let on = probe_cfg(Some(4096));
        assert!(
            registry_from_config_with_home(&on, &CircuitBreakerConfig::default(), Some(&home))
                .expect("registry builds")
                .max_tokens_table()
                .is_some(),
            "control case must attach a table"
        );

        let mut cfg = crate::config::Config {
            providers: probe_cfg(Some(4096)),
            ..crate::config::Config::default()
        };
        // SAFETY: this test owns the MOAGAN_MAX_TOKEN_AUTO env var;
        // the `remove_var` immediately after balances the set_var.
        unsafe {
            std::env::set_var("MOAGAN_MAX_TOKEN_AUTO", "0");
        }
        cfg.apply_env_overrides();
        // SAFETY: see the matching `set_var` above.
        unsafe {
            std::env::remove_var("MOAGAN_MAX_TOKEN_AUTO");
        }
        let reg = registry_from_config_with_home(
            &cfg.providers,
            &CircuitBreakerConfig::default(),
            Some(&home),
        )
        .expect("registry builds");
        assert!(
            reg.max_tokens_table().is_none(),
            "MOAGAN_MAX_TOKEN_AUTO=0 must disable the probe end-to-end"
        );
    }

    /// The `Debug` impl reports table presence only — never the
    /// entries, which can be one per (provider, model) pair.
    #[test]
    fn debug_impl_reports_table_presence_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = MoaganHome::at(dir.path().to_path_buf());
        let off = registry_from_config_with_home(
            &probe_cfg(None),
            &CircuitBreakerConfig::default(),
            Some(&home),
        )
        .expect("registry builds");
        assert!(format!("{off:?}").contains("max_tokens_table: \"absent\""));
        let on = registry_from_config_with_home(
            &probe_cfg(Some(4096)),
            &CircuitBreakerConfig::default(),
            Some(&home),
        )
        .expect("registry builds");
        assert!(format!("{on:?}").contains("max_tokens_table: \"present\""));
    }
}
