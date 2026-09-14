//! [`LlmClientRegistry`] — the post-#933 replacement for the legacy
//! `crate::llm::provider::ProviderRegistry`.
//!
//! Stores SDK impls directly (`Arc<dyn LlmClient>`) keyed by the
//! operator-facing section name (or the joined `section::model` key
//! for multi-model registries). On [`Self::insert`] the wrapper
//! layer (breaker / rate-limiter / saturation-sink) is applied via
//! [`crate::llm::client::breakered::BreakeredClient`] so every
//! production dispatch goes through the breaker. Tests can opt out
//! of the wrap with [`Self::insert_raw`] so a hand-rolled stub
//! (`ScriptedLlmClient`, `TemperatureRecordingClient`, …) lands on
//! the call site without an extra wrapper layer.
//!
//! The lookups (`pick_wrapped`, `get`, `get_wrapped`) mirror the
//! shape the runtime already used on `ProviderRegistry` so the
//! call-site migration is a type swap — same method names, same
//! semantics, same panic shape when the lookup misses.

use std::collections::HashMap;
use std::sync::Arc;

use super::breakered::BreakeredClient;
use super::{LlmClient, LlmRequest, LlmResponse};

/// Registry of SDK clients by operator-facing name.
///
/// Replaces `crate::llm::provider::ProviderRegistry`. Stores
/// [`LlmClient`] trait objects keyed by `section` (or the joined
/// `section::model` key for multi-model registries).
///
/// Two lookup surfaces:
///
/// - `by_name`: the legacy `HashMap<String, Arc<dyn LlmClient>>`
///   keyed by section name (or joined key).
/// - `wrapped`: the `HashMap<String, Arc<dyn LlmClient>>` of the
///   same entries upcast to the trait object — the registry
///   intentionally stores everything as `Arc<dyn LlmClient>` so
///   `LlmClientRegistry::get` returns a trait object directly and
///   callers do not need to know whether the entry was inserted
///   raw or wrapped.
///
/// `BreakeredClient` is the only wrapper the registry applies on
/// `insert`; round-robin pools, rate limiters, and saturation sinks
/// are NOT replicated from `ProviderRegistry` because the
/// dispatcher uses `BreakeredClient::send` end-to-end and the
/// per-`(provider, role)` breaker / governor lives on
/// `RunContext::breaker_per_role` and `RunContext::throttle`.
#[derive(Clone, Default)]
pub struct LlmClientRegistry {
    by_name: HashMap<String, Arc<dyn LlmClient>>,
    /// Optional table of auto-discovered `max_tokens` per
    /// `(provider, model)`. `None` when auto-probe is disabled
    /// (`max_token_auto = None`/`Some(0)` for every provider).
    max_tokens_table: Option<Arc<crate::llm::probe_table::MaxTokensTable>>,
    /// Optional table of auto-discovered supported sampling
    /// temperatures per `(provider, model)`.
    temperature_table: Option<Arc<crate::llm::temperature_probe::TemperatureTable>>,
    /// Self-healing param-rejection table.
    param_rejections: Option<Arc<crate::llm::param_rejections::ParamRejectionsTable>>,
    /// Auto-discovered supported-`top_p` table.
    top_p_table: Option<Arc<crate::llm::top_p_probe::TopPTable>>,
    /// Auto-discovered supported-`top_k` table.
    top_k_table: Option<Arc<crate::llm::top_k_probe::TopKTable>>,
}

impl std::fmt::Debug for LlmClientRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        f.debug_struct("LlmClientRegistry")
            .field("names", &names)
            .finish()
    }
}

/// Compatibility alias — pre-#933 code path used
/// `crate::llm::ProviderRegistry`. `LlmClientRegistry` is the
/// post-#933 replacement that stores `Arc<dyn LlmClient>` instead
/// of `Arc<dyn Provider>`. The alias keeps the migration sites
/// searchable while the call-site refactor (#933) rewrites them.
pub type ProviderRegistry = LlmClientRegistry;

/// Free-standing alias for the canonical `(section, model)` joined
/// key the legacy `ProviderRegistry::registry_key` helper exposed.
/// Kept as a free function so call sites that need to compute the
/// joined key outside the registry (the dispatch fan-out
/// pre-flight filter, …) keep a single source of truth.
pub fn registry_key(section: &str, model_id: &str) -> String {
    format!("{section}::{model_id}")
}

impl LlmClientRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            max_tokens_table: None,
            temperature_table: None,
            param_rejections: None,
            top_p_table: None,
            top_k_table: None,
        }
    }

    /// Build a registry from a list of `(name, client)` pairs. Each
    /// entry is wrapped via [`Self::insert`] (auto-wraps in a
    /// [`BreakeredClient`] with a default lenient breaker).
    pub fn with_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (String, Arc<dyn LlmClient>)>,
    {
        let mut reg = Self::new();
        for (name, client) in entries {
            reg.insert(name, client);
        }
        reg
    }

    /// Insert an SDK impl under `name`. The wrapper
    /// [`BreakeredClient`] is applied automatically so every
    /// production dispatch goes through the breaker layer. Tests
    /// that need direct access to the inner SDK impl use
    /// [`Self::insert_raw`].
    pub fn insert(&mut self, name: String, client: Arc<dyn LlmClient>) {
        let wrapped = Arc::new(BreakeredClient::new(client)) as Arc<dyn LlmClient>;
        self.by_name.insert(name, wrapped);
    }

    /// Insert an SDK impl under `name` WITHOUT the
    /// [`BreakeredClient`] wrap. Use only from tests that need to
    /// inspect the inner SDK impl directly (e.g. asserting
    /// `call_count()` on a [`crate::llm::client::test_stubs::ScriptedLlmClient`]
    /// — `BreakeredClient` swallows the call count behind its
    /// adapter layer).
    pub fn insert_raw(&mut self, name: String, client: Arc<dyn LlmClient>) {
        self.by_name.insert(name, client);
    }

    /// Attach the auto-discovered `max_tokens` table so every
    /// SDK impl reads the per-`(provider, model)` ceiling through
    /// the registry handle. Mirrors the legacy
    /// `ProviderRegistry::with_max_tokens_table` builder.
    pub fn with_max_tokens_table(
        mut self,
        table: Arc<crate::llm::probe_table::MaxTokensTable>,
    ) -> Self {
        self.max_tokens_table = Some(table);
        self
    }

    /// Auto-discovered `max_tokens` table, when the auto-probe is
    /// enabled for at least one provider. `None` disables every
    /// probe-aware code path so callers fall back to the static
    /// `ProviderConfig::max_tokens` knob.
    pub fn max_tokens_table(&self) -> Option<&Arc<crate::llm::probe_table::MaxTokensTable>> {
        self.max_tokens_table.as_ref()
    }

    /// Attach the auto-discovered supported-temperatures table.
    pub fn with_temperature_table(
        mut self,
        table: Arc<crate::llm::temperature_probe::TemperatureTable>,
    ) -> Self {
        self.temperature_table = Some(table);
        self
    }

    /// Auto-discovered supported-temperatures table.
    pub fn temperature_table(
        &self,
    ) -> Option<&Arc<crate::llm::temperature_probe::TemperatureTable>> {
        self.temperature_table.as_ref()
    }

    /// Attach the self-healing param-rejection table.
    pub fn with_param_rejections(
        mut self,
        table: Arc<crate::llm::param_rejections::ParamRejectionsTable>,
    ) -> Self {
        self.param_rejections = Some(table);
        self
    }

    /// Self-healing param-rejection table.
    pub fn param_rejections(
        &self,
    ) -> Option<&Arc<crate::llm::param_rejections::ParamRejectionsTable>> {
        self.param_rejections.as_ref()
    }

    /// Attach the auto-discovered supported-`top_p` table.
    pub fn with_top_p_table(mut self, table: Arc<crate::llm::top_p_probe::TopPTable>) -> Self {
        self.top_p_table = Some(table);
        self
    }

    /// Auto-discovered supported-`top_p` table.
    pub fn top_p_table(&self) -> Option<&Arc<crate::llm::top_p_probe::TopPTable>> {
        self.top_p_table.as_ref()
    }

    /// Attach the auto-discovered supported-`top_k` table.
    pub fn with_top_k_table(mut self, table: Arc<crate::llm::top_k_probe::TopKTable>) -> Self {
        self.top_k_table = Some(table);
        self
    }

    /// Auto-discovered supported-`top_k` table.
    pub fn top_k_table(&self) -> Option<&Arc<crate::llm::top_k_probe::TopKTable>> {
        self.top_k_table.as_ref()
    }

    /// Attach a saturation sink to every `BreakeredClient` in the
    /// registry. The legacy `ProviderRegistry::with_saturation_sink`
    /// walked the wrapped map and called
    /// `BreakeredProvider::with_saturation_sink` on each entry;
    /// the post-#933 registry stores `Arc<dyn LlmClient>` so the
    /// sink goes through [`LlmClient::attach_saturation_sink`]
    /// which `BreakeredClient` overrides to forward to the inner
    /// sink slot. Empty registries are a no-op.
    pub fn attach_saturation_sink(&self, sink: Arc<dyn super::compat::SaturationSink>) {
        for client in self.by_name.values() {
            client.attach_saturation_sink(sink.clone());
        }
    }

    /// Resolve the client registered under `name`. Returns the
    /// [`Arc<dyn LlmClient>`] handle directly — for the production
    /// registry this is the `BreakeredClient`; for the test
    /// registry (built via [`Self::insert_raw`]) this is the raw
    /// SDK impl.
    pub fn get(&self, name: &str) -> Option<Arc<dyn LlmClient>> {
        self.by_name.get(name).cloned()
    }

    /// Round-robin pick. The legacy `ProviderRegistry::pick_wrapped`
    /// iterated over a per-name pool to fan out across multiple
    /// instances of the same provider kind. The new registry does
    /// not replicate that path: production SDK impls are
    /// load-balanced at the operator's reverse-proxy layer, and
    /// the `BreakeredClient` round-robin is intentionally omitted
    /// because every SDK impl now goes through the per
    /// `(provider, role)` breaker / governor on
    /// `RunContext::breaker_per_role` combined with
    /// `RunContext::throttle`, which provides the per-call
    /// isolation the old per-provider pool gave us. Returns the
    /// same entry the `get` lookup would for the default-provider
    /// path.
    pub fn pick_wrapped(&self, _force_default: bool) -> Option<Arc<dyn LlmClient>> {
        // No pool: return `None` and let the caller fall through to
        // `get` with the joined / bare section key. The pool-first
        // ordering is preserved on the call site (`phase.rs`).
        None
    }

    /// Same as [`Self::get`] but keeps the `wrapped` naming so the
    /// call-site migration is a one-line rename. `LlmClientRegistry`
    /// does not distinguish between raw and wrapped entries (every
    /// `Arc<dyn LlmClient>` the registry hands back is a
    /// `BreakeredClient` for production paths); the legacy naming
    /// is preserved as a thin alias.
    pub fn get_wrapped(&self, name: &str) -> Option<Arc<dyn LlmClient>> {
        self.get(name)
    }

    /// Resolve the client registered under the joined
    /// `(section, model_id)` key, falling back to the bare section
    /// name. The joined key is the canonical multi-model registry
    /// entry; the bare section fallback keeps the legacy
    /// single-instance registrations working.
    pub fn resolve(&self, section: &str, model_id: &str) -> Option<Arc<dyn LlmClient>> {
        let joined = registry_key(section, model_id);
        self.get(&joined).or_else(|| self.get(section))
    }

    /// Resolve the client registered under the joined
    /// `(section, model_id)` key, falling back to the bare section
    /// name. Panics with the same diagnostic shape as
    /// `ProviderRegistry`'s legacy callers when both lookups miss.
    pub fn resolve_or_panic(&self, section: &str, model_id: &str) -> Arc<dyn LlmClient> {
        self.resolve(section, model_id).unwrap_or_else(|| {
            panic!(
                "LlmClientRegistry: client for (section={section}, model={model_id}) (joined key \
                 {:?}) must be registered; use LlmClientRegistry::insert at construction",
                registry_key(section, model_id)
            )
        })
    }

    /// Build the canonical `section::model_id` registry key. Mirrors
    /// the legacy `ProviderRegistry::registry_key` helper so call
    /// sites that need to look up the joined key in advance (e.g.
    /// `has_provider_for`) keep working with a type swap.
    pub fn registry_key(section: &str, model_id: &str) -> String {
        format!("{section}::{model_id}")
    }

    /// True when `section` (or the joined `section::model_id` key)
    /// is registered.
    pub fn contains(&self, section: &str) -> bool {
        self.by_name.contains_key(section)
    }

    /// Iterate over `(name, client)` pairs. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Arc<dyn LlmClient>)> + '_ {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v.clone()))
    }

    /// Number of registered clients.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// True when the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// `LlmClient` is implemented for `Arc<dyn LlmClient>` through
/// delegation — a registered entry can be cloned and dropped
/// without losing the trait object. This blanket impl keeps the
/// `Arc<dyn LlmClient>` ergonomics compatible with the legacy
/// `Arc<dyn Provider>` lookups.
#[async_trait::async_trait]
impl LlmClient for Arc<dyn LlmClient> {
    fn sdk_type(&self) -> &'static str {
        (**self).sdk_type()
    }
    fn name(&self) -> &str {
        (**self).name()
    }
    fn model(&self) -> &str {
        (**self).model()
    }
    fn endpoint(&self) -> &str {
        (**self).endpoint()
    }
    fn capabilities(&self) -> super::LlmCapabilities {
        (**self).capabilities()
    }
    fn param_rejections_table(
        &self,
    ) -> Option<Arc<crate::llm::param_rejections::ParamRejectionsTable>> {
        (**self).param_rejections_table()
    }
    fn set_param_rejections(&self, table: Arc<crate::llm::param_rejections::ParamRejectionsTable>) {
        (**self).set_param_rejections(table)
    }
    async fn send_once(&self, req: &LlmRequest) -> crate::error::Result<LlmResponse> {
        (**self).send_once(req).await
    }
    async fn send(&self, req: &LlmRequest) -> crate::error::Result<LlmResponse> {
        (**self).send(req).await
    }
    fn body_sha256(&self, req: &LlmRequest) -> crate::error::Result<String> {
        (**self).body_sha256(req)
    }
    async fn send_probe(&self, req: &LlmRequest) -> crate::error::Result<LlmResponse> {
        (**self).send_probe(req).await
    }
    fn max_tokens_probe_ceiling(&self) -> u32 {
        (**self).max_tokens_probe_ceiling()
    }
    fn effective_max_tokens(&self, req: &LlmRequest) -> u32 {
        (**self).effective_max_tokens(req)
    }
    async fn count_tokens(&self, text: &str) -> Option<u64> {
        (**self).count_tokens(text).await
    }
}
