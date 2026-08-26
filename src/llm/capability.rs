//! Resolved capability for a `(provider, model)` pair.
//!
//! [`CapabilityResolver`] answers "what knobs does this model
//! actually honour?" by merging three sources in this priority order:
//!
//! 1. **Config overrides** — entries the operator pinned in
//!    `<MOAGAN_HOME>/config.toml`. Highest priority because the
//!    operator's explicit choice beats any inferred signal.
//! 2. **Catalog** — rows loaded from the upstream `models.dev`
//!    catalog via [`crate::llm::models_dev::load_or_fetch`].
//! 3. **Hardcoded default** — the conservative baseline used when
//!    nothing else applies (text in / text out, temperature
//!    supported, no reasoning / tools).
//!
//! PR-3 only implements config + catalog + hardcoded. A future PR
//! will add a runtime probe layer (`Probed`) that wins over every
//! other source because the operator's measured signal is closer to
//! ground truth than the upstream-published row.
//!
//! [`ResolvedCapability::gate_request`] applies a resolver to a
//! [`crate::llm::wire::Request`] and drops the fields the model
//! would reject. PR-3 only gates `temperature` because every
//! concrete provider already omits `top_p` on `None`; PR-4 / PR-5
//! will add `tool_call`, `reasoning`, and `attachment` knobs.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::llm::models_dev::ModelsDevCatalog;
use crate::llm::wire::Request;

/// Origin of a [`ResolvedCapability`] field. Surfaced in `source`
/// so an operator can tell, from the audit log, whether the
/// resolver pinned the value from the catalog, an override, or the
/// hardcoded baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilitySource {
    /// Discovered at runtime via the auto-probe path. Reserved for
    /// the future "probed wins" PR; not produced by PR-3.
    Probed,
    /// Pulled from the models.dev catalog (PR-1).
    Catalog,
    /// Set in the user's `<MOAGAN_HOME>/config.toml`.
    Config,
    /// Hardcoded constant in this module. Used as the fallback when
    /// nothing else applies.
    Hardcoded,
    /// Combined from multiple sources (PR-4 / PR-5 territory).
    Merged,
}

/// Resolved capability matrix for a single `(provider, model)` pair.
///
/// All boolean fields default to the conservative "the upstream does
/// the right thing without us setting it" baseline; the resolver
/// overrides them as soon as a more authoritative source appears.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedCapability {
    /// `true` when the upstream honours a `temperature` knob. PR-3
    /// only acts on this field; other flags are kept here so the
    /// future gating PRs can branch on them without re-plumbing the
    /// struct shape.
    pub temperature: bool,
    /// `true` when the upstream exposes a separate reasoning
    /// trace (`reasoning_content`, `thinking`, ...).
    pub reasoning: bool,
    /// `true` when the upstream accepts OpenAI-style tool /
    /// function-call definitions.
    pub tool_call: bool,
    /// `true` when the upstream accepts binary file attachments.
    pub attachment: bool,
    /// Input modality list (`text`, `image`, `pdf`, ...). Empty
    /// vector when unknown.
    pub modalities_in: Vec<String>,
    /// Output modality list. Empty vector when unknown.
    pub modalities_out: Vec<String>,
    /// Total context window in tokens. `None` when no source
    /// published the cap.
    pub max_tokens_input: Option<u32>,
    /// Maximum output tokens per response. `None` when no source
    /// published the cap.
    pub max_tokens_output: Option<u32>,
    /// Per-million-token input price (USD). `None` when unknown.
    pub cost_input: Option<f64>,
    /// Per-million-token output price (USD). `None` when unknown.
    pub cost_output: Option<f64>,
    /// Cached-input read price (USD per million tokens). `None`
    /// when unknown.
    pub cost_cache_read: Option<f64>,
    /// Cached-input write price (USD per million tokens). `None`
    /// when unknown.
    pub cost_cache_write: Option<f64>,
    /// Family bucket (e.g. `minimax`, `kimi`). `None` when the
    /// catalog did not publish one.
    pub family: Option<String>,
    /// Source of the dominant field. PR-3 always produces one of
    /// `Config`, `Catalog`, or `Hardcoded`; `Merged` lands with the
    /// future "probed wins" PR.
    pub source: CapabilitySource,
}

impl ResolvedCapability {
    /// Conservative baseline returned when no probe, no catalog,
    /// and no config override can answer. The temperature knob is
    /// ON by default because the OpenAI-compat baseline does honour
    /// it; reasoning / tool / attachment are OFF because they are
    /// upstream-specific and silently enabling them would mask
    /// schema-drift bugs.
    fn conservative_default() -> Self {
        Self {
            temperature: true,
            reasoning: false,
            tool_call: false,
            attachment: false,
            modalities_in: vec!["text".to_owned()],
            modalities_out: vec!["text".to_owned()],
            max_tokens_input: None,
            max_tokens_output: None,
            cost_input: None,
            cost_output: None,
            cost_cache_read: None,
            cost_cache_write: None,
            family: None,
            source: CapabilitySource::Hardcoded,
        }
    }

    /// Build a `ResolvedCapability` from a catalog row. The mapper
    /// is private because the only legitimate source of an entry is
    /// the upstream `models.dev` document — there is no other
    /// caller for this transformation.
    fn from_catalog_entry(entry: &crate::llm::models_dev::ModelsDevEntry) -> Self {
        Self {
            temperature: entry.temperature,
            reasoning: entry.reasoning,
            tool_call: entry.tool_call,
            attachment: entry.attachment,
            modalities_in: entry.modalities.input.clone(),
            modalities_out: entry.modalities.output.clone(),
            max_tokens_input: Some(entry.limit.context as u32),
            max_tokens_output: Some(entry.limit.output as u32),
            cost_input: Some(entry.cost.input),
            cost_output: Some(entry.cost.output),
            cost_cache_read: Some(entry.cost.cache_read),
            cost_cache_write: Some(entry.cost.cache_write),
            family: entry.family.clone(),
            source: CapabilitySource::Catalog,
        }
    }
}

/// Operator-supplied overrides keyed by `"<provider>/<model>"`. The
/// forward-slash join avoids tuple keys (which `BTreeMap<String, ...>`
/// cannot express without `String::from(("a", "b"))` ceremony) and
/// matches the way upstream providers display the same pair in their
/// dashboards.
#[derive(Debug, Clone, Default)]
pub struct ResolvedConfig {
    /// Per-pair overrides. The key format is
    /// `"{provider}/{model}"`; both segments must match the upstream
    /// spelling exactly (the catalog is internally consistent).
    pub overrides: BTreeMap<String, ResolvedCapability>,
}

impl ResolvedConfig {
    /// Look up an override for `(provider, model)`. The lookup is
    /// case-sensitive, matching [`crate::llm::models_dev::lookup`].
    pub fn lookup(&self, provider: &str, model: &str) -> Option<&ResolvedCapability> {
        self.overrides.get(&pair_key(provider, model))
    }
}

fn pair_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// Single point of truth for "what knobs does this model honour?".
///
/// Construct via [`CapabilityResolver::new`] (catalog-only). The
/// resolver is `Send + Sync` once wrapped in `Arc` because every
/// field is either `None`, `Arc<...>`, or `BTreeMap<...>`; the
/// typical use site stores it on [`crate::phases::phase::RunContext`].
pub struct CapabilityResolver {
    catalog: Option<Arc<ModelsDevCatalog>>,
    config: ResolvedConfig,
}

impl std::fmt::Debug for CapabilityResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityResolver")
            .field(
                "catalog",
                &if self.catalog.is_some() {
                    "present"
                } else {
                    "absent"
                },
            )
            .field("config_overrides", &self.config.overrides.len())
            .finish()
    }
}

impl CapabilityResolver {
    /// Build a resolver with an optional catalog handle. Passing
    /// `None` makes every lookup fall through to the hardcoded
    /// default; useful for tests and for runs that disabled the
    /// catalog loader (`--offline-catalog`).
    pub fn new(catalog: Option<Arc<ModelsDevCatalog>>) -> Self {
        tracing::debug!(
            catalog_attached = catalog.is_some(),
            "CapabilityResolver: constructed"
        );
        Self {
            catalog,
            config: ResolvedConfig::default(),
        }
    }

    /// The catalog handle, when one was attached. Exposed for
    /// telemetry so an operator can confirm at runtime which
    /// source the resolver consulted.
    pub fn catalog(&self) -> Option<&Arc<ModelsDevCatalog>> {
        self.catalog.as_ref()
    }

    /// Resolve the capability for `(provider, model)`. The lookup
    /// order is config → catalog → hardcoded default; the returned
    /// `source` field records which tier answered.
    pub fn resolve(&self, provider: &str, model: &str) -> ResolvedCapability {
        if let Some(override_cap) = self.config.lookup(provider, model) {
            tracing::trace!(
                provider,
                model,
                source = "config",
                "CapabilityResolver::resolve"
            );
            return override_cap.clone();
        }
        if let Some(catalog) = self.catalog.as_ref()
            && let Some(entry) = catalog
                .providers
                .get(provider)
                .and_then(|p| p.models.get(model))
        {
            tracing::trace!(
                provider,
                model,
                source = "catalog",
                "CapabilityResolver::resolve"
            );
            return ResolvedCapability::from_catalog_entry(entry);
        }
        tracing::trace!(
            provider,
            model,
            source = "hardcoded",
            "CapabilityResolver::resolve"
        );
        ResolvedCapability::conservative_default()
    }

    /// Apply the resolver to a [`Request`], returning a new
    /// `Request` with the gated fields removed. PR-3 only acts on
    /// the `temperature` knob; future PRs will extend this with
    /// reasoning / tool / attachment / modality gating without
    /// changing the function signature.
    ///
    /// The returned request is always a fresh `Request` (the input
    /// is never mutated). Callers that already cloned the request
    /// for `effective_max_tokens` can reuse that clone instead of
    /// letting `gate_request` allocate again.
    pub fn gate_request(&self, provider: &str, model: &str, req: &Request) -> Request {
        let capability = self.resolve(provider, model);
        let mut gated = req.clone();
        if !capability.temperature {
            tracing::debug!(
                provider,
                model,
                "gate_request: dropping temperature (capability disabled)"
            );
            gated.temperature = None;
        }
        gated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::models_dev::{
        Cost, Limits, Modalities, ModelsDevCatalog, ModelsDevEntry, ModelsDevProvider,
    };
    use crate::llm::role::Role;
    use std::collections::BTreeMap;

    fn sample_request(temperature: Option<f32>) -> Request {
        Request {
            role: Role::Intake,
            model: "kimi-k3".to_owned(),
            system: "sys".to_owned(),
            user: "user".to_owned(),
            max_tokens: Some(1024),
            temperature,
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    fn catalog_with_kimi() -> Arc<ModelsDevCatalog> {
        let mut models = BTreeMap::new();
        models.insert(
            "kimi-k3".to_owned(),
            ModelsDevEntry {
                id: "kimi-k3".to_owned(),
                name: "Kimi k3".to_owned(),
                family: Some("kimi".to_owned()),
                attachment: false,
                reasoning: true,
                reasoning_options: vec![],
                tool_call: true,
                temperature: false,
                interleaved: None,
                modalities: Modalities {
                    input: vec!["text".to_owned()],
                    output: vec!["text".to_owned()],
                },
                limit: Limits {
                    context: 131_072,
                    output: 8_192,
                },
                cost: Cost {
                    input: 0.6,
                    output: 2.4,
                    cache_read: 0.06,
                    cache_write: 0.75,
                },
                open_weights: false,
                release_date: None,
                last_updated: None,
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            "kimi".to_owned(),
            ModelsDevProvider {
                id: "kimi".to_owned(),
                name: "Kimi".to_owned(),
                api: None,
                doc: None,
                models,
            },
        );
        Arc::new(ModelsDevCatalog {
            schema_version: crate::llm::models_dev::CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 1_700_000_000,
            providers,
        })
    }

    /// Empty resolver (no catalog, no overrides) returns the
    /// conservative default. `source` is `Hardcoded`.
    #[test]
    fn resolver_with_no_catalog_returns_conservative_default() {
        let resolver = CapabilityResolver::new(None);
        let cap = resolver.resolve("minimax", "MiniMax-M3");
        assert!(cap.temperature);
        assert!(!cap.reasoning);
        assert!(!cap.tool_call);
        assert_eq!(cap.source, CapabilitySource::Hardcoded);
        assert_eq!(cap.modalities_in, vec!["text".to_owned()]);
        assert_eq!(cap.modalities_out, vec!["text".to_owned()]);
    }

    /// Resolver with a catalog containing the pair returns the
    /// catalog's values verbatim. `source` is `Catalog`.
    #[test]
    fn resolver_with_catalog_returns_catalog_values() {
        let resolver = CapabilityResolver::new(Some(catalog_with_kimi()));
        let cap = resolver.resolve("kimi", "kimi-k3");
        assert!(!cap.temperature, "kimi-k3 must report temperature=false");
        assert!(cap.reasoning, "kimi-k3 must report reasoning=true");
        assert!(cap.tool_call, "kimi-k3 must report tool_call=true");
        assert_eq!(cap.source, CapabilitySource::Catalog);
        assert_eq!(cap.max_tokens_input, Some(131_072));
        assert_eq!(cap.max_tokens_output, Some(8_192));
        assert_eq!(cap.family.as_deref(), Some("kimi"));
    }

    /// Resolver with a catalog that does NOT contain the pair
    /// falls through to the hardcoded default. This is the
    /// "operator never saw the model on models.dev" case.
    #[test]
    fn resolver_unknown_provider_returns_default() {
        let resolver = CapabilityResolver::new(Some(catalog_with_kimi()));
        let cap = resolver.resolve("minimax", "MiniMax-M3");
        assert_eq!(cap.source, CapabilitySource::Hardcoded);
        assert!(cap.temperature);
        assert!(!cap.reasoning);
    }

    /// Gating drops `temperature` when the capability says so. The
    /// other fields (system, user, top_p) are preserved verbatim.
    #[test]
    fn gate_request_drops_temperature_when_false() {
        let resolver = CapabilityResolver::new(Some(catalog_with_kimi()));
        let req = sample_request(Some(0.6));
        let gated = resolver.gate_request("kimi", "kimi-k3", &req);
        assert!(gated.temperature.is_none(), "temperature must be None");
        assert_eq!(gated.system, "sys");
        assert_eq!(gated.user, "user");
        assert_eq!(gated.max_tokens, Some(1024));
        assert_eq!(gated.top_p, Some(0.95), "top_p must NOT be touched");
        assert_eq!(gated.model, "kimi-k3");
    }

    /// Gating preserves `temperature` when the capability allows
    /// it. The output request is a verbatim clone (modulo
    /// intentional gating — there is none here).
    #[test]
    fn gate_request_keeps_temperature_when_true() {
        let resolver = CapabilityResolver::new(None);
        let req = sample_request(Some(0.6));
        let gated = resolver.gate_request("minimax", "MiniMax-M3", &req);
        assert_eq!(
            gated.temperature,
            Some(0.6),
            "temperature must round-trip when the capability allows it"
        );
        assert_eq!(gated.top_p, req.top_p);
        assert_eq!(gated.system, req.system);
        assert_eq!(gated.user, req.user);
    }

    /// `ResolvedConfig::lookup` returns None for an unknown pair and
    /// `Some(cap)` for a known pair. The key format is the
    /// documented `"{provider}/{model}"` join.
    #[test]
    fn resolved_config_lookup_is_case_sensitive_and_keyed_by_pair() {
        let key = pair_key("kimi", "kimi-k3");
        let cap = ResolvedCapability {
            temperature: false,
            ..ResolvedCapability::conservative_default()
        };
        let mut cfg = ResolvedConfig::default();
        cfg.overrides.insert(key, cap);
        assert!(cfg.lookup("kimi", "kimi-k3").is_some());
        assert!(cfg.lookup("Kimi", "kimi-k3").is_none(), "case-sensitive");
        assert!(cfg.lookup("kimi", "K3").is_none(), "model mismatch");
        assert!(
            cfg.lookup("other", "kimi-k3").is_none(),
            "provider mismatch"
        );
    }

    /// `CapabilityResolver::gate_request` returns a fresh request
    /// that does NOT mutate the input. The pin matters because
    /// the call site in `phases::phase::dispatch_to_provider`
    /// reuses the input request for the actual send.
    #[test]
    fn gate_request_does_not_mutate_input() {
        let resolver = CapabilityResolver::new(Some(catalog_with_kimi()));
        let req = sample_request(Some(0.6));
        let original_temperature = req.temperature;
        let _ = resolver.gate_request("kimi", "kimi-k3", &req);
        assert_eq!(
            req.temperature, original_temperature,
            "input request must remain intact"
        );
    }

    /// Resolver exposes `catalog()` so a run-time observability
    /// surface can confirm whether the catalog was attached. This
    /// is the cheap, "is the catalog present?" check — the deeper
    /// resolve() lookups happen on the request hot path.
    #[test]
    fn resolver_exposes_catalog_handle() {
        let resolver = CapabilityResolver::new(Some(catalog_with_kimi()));
        assert!(resolver.catalog().is_some());
        let resolver = CapabilityResolver::new(None);
        assert!(resolver.catalog().is_none());
    }
}
