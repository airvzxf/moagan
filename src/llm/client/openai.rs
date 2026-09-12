//! `OpenAIClient` — SDK impl that handles BOTH OpenAI URL variants
//! per #900 D1:
//!
//! - `/v1/chat/completions` → [`OpenAIVariant::Chat`] (reuses the
//!   wire-body construction logic from
//!   [`crate::llm::openai_compatible`])
//! - `/v1/responses`        → [`OpenAIVariant::Responses`] (reuses
//!   the wire-body construction logic from
//!   [`crate::llm::openai_compat`])
//!
//! The variant is picked once at construction time from the URL
//! path suffix; every method routes via `match self.variant`
//! without re-parsing the URL on the hot path. The SDK is
//! self-contained — both variants LIFT the legacy provider's
//! `send_with_safety_clamp` transport into the SDK rather than
//! delegating, so the wire body byte-identicality guarantee
//! (D8 — "no compat layer") holds at every call site.
//!
//! EPIC #847 — issue #921 (`feat(llm): introduce OpenAIClient SDK
//! impl`). Wave 1.3 of the EPIC, after #919 (`LlmClient` trait +
//! `MockClient`) and #920 (`AnthropicClient`). The PR that flips
//! the runtime to use `OpenAIClient` over
//! `OpenAICompatibleProvider` / `OpenAICompatProvider` lives in
//! issue #922 (URL-path dispatcher).
//!
//! D8 invariant: `body_sha256(req)` returns the SHA-256 of the exact
//! byte sequence `send(req)` transmits. There is **no** `if minimax`
//! branch anywhere — the same byte sequence `send` emits goes
//! through this function. Both variants compute the wire body via
//! the same free function the legacy `Provider::send` uses
//! (`build_chat_request_body` / `build_responses_body`) so the
//! two paths cannot drift.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::llm::wire_format::WireFormatId;
use crate::secret::SecretString;

use super::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};
use crate::llm::capabilities::ProviderCapabilities;
use crate::llm::openai_compat::{
    build_responses_body, responses_text_json_object, wants_response_format,
};
use crate::llm::openai_compatible::build_chat_request_body;
use crate::llm::probe_table::MaxTokensTable;
use crate::llm::size_limits::{MAX_RESPONSE_BYTES, check_size};
use crate::llm::wire::{Request as LegacyRequest, Response as LegacyResponse, Usage};

/// Discriminator the SDK uses to route between the two OpenAI URL
/// variants the operator can declare in `config.toml` (#900 D1).
///
/// Picked once at construction time from the endpoint URL's path
/// suffix (`/chat/completions` or `/responses`); every hot-path
/// method inspects `self.variant` to pick the right body builder
/// and HTTP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenAIVariant {
    /// `/v1/chat/completions` — OpenAI-compatible Chat Completions
    /// wire. Reuses the body builder from
    /// [`crate::llm::openai_compatible`]. Advertises the
    /// `"openai_compatible"` SDK identifier to match the legacy
    /// [`crate::llm::capabilities::ProviderCapabilities::wire_format_id`]
    /// for this wire.
    Chat,
    /// `/v1/responses` — OpenAI Responses API wire. Reuses the
    /// body builder from [`crate::llm::openai_compat`].
    /// Advertises the `"openai"` SDK identifier to match the
    /// legacy capability matrix for this wire.
    Responses,
}

/// SDK impl for the OpenAI-compatible Chat Completions and OpenAI
/// Responses API endpoints. A single struct, two variants — the
/// URL-path dispatcher picks the right variant at construction
/// time so the rest of the runtime can hold one trait object per
/// `(section, model)` pair.
#[derive(Clone)]
pub struct OpenAIClient {
    variant: OpenAIVariant,
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: reqwest::Client,
    max_retries: u32,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,000,000); the clamp below exists for
    /// the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request with 400.
    provider_max_tokens: Option<u32>,
    /// Kind-level hard cap on `max_tokens`, applied as a second
    /// layer on top of `provider_max_tokens`. Wired by
    /// [`Self::from_resolved`] for the direct DeepSeek section
    /// (`Some(DEEPSEEK_MAX_TOKENS_CAP)`); `None` for every other
    /// chat-completions section (the upstream is permissive enough
    /// to accept the operator's choice). The Responses variant
    /// ignores this field — `OpenAICompatProvider` never had a
    /// kind cap on the Responses path.
    kind_hard_cap: Option<u32>,
    /// Auto-probed `max_tokens` table. When `Some` the
    /// `resolve_cached(self.name(), self.model())` value joins the
    /// clamp chain as the third-highest layer. `None` when the SDK
    /// was built without going through `registry_from_config` (unit
    /// tests and legacy call paths).
    max_tokens_table: Option<Arc<MaxTokensTable>>,
    /// Operator-pinned per-section flag that drops the
    /// `max_tokens` field from the wire body entirely. Required
    /// for upstream models that reject the *presence* of the
    /// field (e.g. `gpt-5.6-luna` on the Responses variant). The
    /// chat variant carries `false` for every operator config in
    /// the v0.10 schema (the opt-out lives on the legacy
    /// `OpenAICompatProvider::wire_max_tokens` guard).
    omit_max_tokens: bool,
}

impl OpenAIClient {
    /// Build from a `ProviderConfig` and a resolved API key.
    /// Picks the variant from the endpoint URL the operator
    /// declared (one of `/chat/completions` or `/responses`).
    /// Mirrors [`crate::llm::openai_compatible::OpenAICompatibleProvider::new`]
    /// and
    /// [`crate::llm::openai_compat::OpenAICompatProvider::new`] on
    /// the dispatcher's input shape so the v0.10 dispatcher can
    /// route either of them to this constructor.
    ///
    /// `kind_hard_cap` defaults to `None`; callers that need the
    /// direct DeepSeek ceiling (the only section the cap applies
    /// to today) construct via [`Self::from_resolved`] which sets
    /// it from the section name.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        tracing::debug!(
            endpoint = spec.endpoint.as_deref(),
            models = spec.models.len(),
            "OpenAIClient::new: enter"
        );
        let endpoint = spec
            .models
            .first()
            .and_then(|m| m.endpoint.clone())
            .or_else(|| spec.endpoint.clone())
            .unwrap_or_else(|| "http://localhost".to_owned());
        let variant = detect_variant(&endpoint)?;
        let client = build_client_for_variant(variant)?;
        let name = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "openai".to_owned());
        let model = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_default();
        let provider_max_tokens = spec.models.first().and_then(|m| m.max_tokens);
        // The legacy `OpenAICompatibleProvider::new` always leaves
        // `kind_hard_cap = None`; only `new_with_kind_cap` (used by
        // `DeepSeekProvider::new`) wires the cap. The SDK mirrors
        // that asymmetry here — `from_resolved` is the path that
        // derives the cap from the section name (`section ==
        // "deepseek"`).
        let kind_hard_cap = None;
        // The Responses variant is the only one that ever honours
        // `omit_max_tokens` on the wire (`gpt-5.6-luna` rejects the
        // *presence* of the field). Reading the flag here keeps the
        // constructor symmetric with the legacy providers; the chat
        // variant ignores it.
        let omit_max_tokens = spec.omit_max_tokens && variant == OpenAIVariant::Responses;
        tracing::info!(
            name = %name,
            model = %model,
            endpoint = %endpoint,
            variant = ?variant,
            "OpenAIClient: constructed"
        );
        Ok(Self {
            variant,
            name,
            model,
            endpoint,
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens,
            kind_hard_cap,
            max_tokens_table: None,
            omit_max_tokens,
        })
    }

    /// Attach the shared auto-probe `max_tokens` table so `send()`
    /// layers the discovered ceiling into the clamp chain. Wired by
    /// `registry_from_config` when the registry has a table.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        tracing::debug!(name = %self.name, "OpenAIClient::with_max_tokens_table");
        self.max_tokens_table = Some(table);
        self
    }

    /// Override the `kind_hard_cap`. The only section this matters
    /// for today is the direct DeepSeek wrapper (it wires
    /// `Some(DEEPSEEK_MAX_TOKENS_CAP)` so the upstream never rejects
    /// the request with HTTP 400). New dispatchers can call this
    /// when they need to install a different per-section cap
    /// without going through [`Self::from_resolved`].
    pub fn with_kind_hard_cap(mut self, cap: Option<u32>) -> Self {
        tracing::debug!(name = %self.name, ?cap, "OpenAIClient::with_kind_hard_cap");
        self.kind_hard_cap = cap;
        self
    }

    /// Override the `omit_max_tokens` flag. Defaults to the
    /// section-level value the constructor read; new dispatchers
    /// can call this to override per-model (e.g. `gpt-5.6-luna`
    /// needs the flag set regardless of the section default).
    pub fn with_omit_max_tokens(mut self, omit: bool) -> Self {
        tracing::debug!(name = %self.name, omit, "OpenAIClient::with_omit_max_tokens");
        self.omit_max_tokens = omit;
        self
    }

    /// v0.10 dispatcher entry point. Builds an `OpenAIClient` from
    /// a `ResolvedModelConfig` (one `(section, model_id)` pair),
    /// resolving the API key via the unified
    /// [`crate::llm::api_keys::lookup_key`] helper. The key lookup
    /// falls back from the section name to the canonical `kind` so
    /// a per-model alias like `kimi-k3` (kind=`"opencode"`)
    /// resolves against the `OPENCODE_API_KEY` env var rather than
    /// the non-existent `KIMI-K3_API_KEY`. The variant is picked
    /// from [`ResolvedModelConfig::wire_format`](crate::config::ResolvedModelConfig::wire_format);
    /// the dispatcher routes to this constructor for
    /// `WireFormatId::OpenAICompatible` and
    /// `WireFormatId::OpenAI`. `kind_hard_cap` is derived from the
    /// section name (`section == "deepseek"` →
    /// `Some(DEEPSEEK_MAX_TOKENS_CAP)`).
    pub fn from_resolved(resolved: &crate::config::ResolvedModelConfig) -> Result<Self> {
        tracing::debug!(
            section = %resolved.section,
            model = %resolved.id,
            wire_format = resolved.wire_format.as_str(),
            "OpenAIClient::from_resolved: enter"
        );
        let variant = variant_from_wire_format(resolved.wire_format)?;
        let kind = crate::llm::api_keys::lookup_kind_for_resolved(resolved);
        let key = crate::llm::api_keys::lookup_key(&kind, None)
            .ok_or_else(|| {
                tracing::error!(kind, "OpenAIClient::from_resolved: API key missing");
                Error::InvalidApiKey {
                    message: format!(
                        "{}_API_KEY not set; provide via env, --api-key, or api_keys.toml",
                        kind.to_ascii_uppercase()
                    ),
                    http_status: None,
                }
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey { message, .. } => Error::InvalidApiKey {
                    message: format!(
                        "{}: {message}; check api_keys.toml and the env var fallback",
                        kind
                    ),
                    http_status: None,
                },
                other => other,
            })?;
        let client = build_client_for_variant(variant)?;
        // The kind cap applies only to the chat-completions path
        // and only to the direct DeepSeek section. Mirrors the
        // legacy `DeepSeekProvider::new` wiring that called
        // `OpenAICompatibleProvider::new_with_kind_cap(_, _,
        // Some(DEEPSEEK_MAX_TOKENS_CAP))`.
        let kind_hard_cap = match (variant, resolved.section.as_str()) {
            (OpenAIVariant::Chat, "deepseek") => {
                Some(crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP)
            }
            _ => None,
        };
        // Only the Responses variant honours `omit_max_tokens` on
        // the wire (`gpt-5.6-luna` rejects the *presence* of the
        // field). The chat variant carries `false` regardless of
        // the operator flag.
        let omit_max_tokens = resolved.omit_max_tokens && variant == OpenAIVariant::Responses;
        tracing::info!(
            section = %resolved.section,
            model = %resolved.id,
            variant = ?variant,
            kind_hard_cap = ?kind_hard_cap,
            omit_max_tokens,
            "OpenAIClient::from_resolved: constructed"
        );
        Ok(Self {
            variant,
            name: resolved.section.clone(),
            model: resolved.id.clone(),
            endpoint: resolved.endpoint.clone(),
            api_key: key,
            client,
            max_retries: 3,
            provider_max_tokens: resolved.max_tokens,
            kind_hard_cap,
            max_tokens_table: None,
            omit_max_tokens,
        })
    }

    /// Variant the SDK was constructed for. Exposed so tests can
    /// pin the wire-format routing without re-parsing the URL.
    pub fn variant(&self) -> OpenAIVariant {
        self.variant
    }

    /// Compute the URL for the chat-completions endpoint. Internal
    /// helper used by [`Self::send_chat_with_safety_clamp`].
    fn chat_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        let url = if base.ends_with("/chat/completions") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        };
        tracing::trace!(endpoint = %self.endpoint, url = %url, "chat_url");
        url
    }

    /// Compute the URL for the responses endpoint. Internal helper
    /// used by [`Self::send_responses_with_safety_clamp`].
    fn responses_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        let url = if base.ends_with("/responses") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/responses")
        } else {
            format!("{base}/v1/responses")
        };
        tracing::trace!(endpoint = %self.endpoint, url = %url, "responses_url");
        url
    }

    async fn sleep_with_jitter(attempt: u32, suggested: Option<std::time::Duration>) {
        let base = suggested.unwrap_or(std::time::Duration::from_millis(500));
        let jitter = (fastrand::u64(..) % 250) + 1;
        let total = base + std::time::Duration::from_millis(jitter);
        let half = total / 2;
        let low = total.saturating_sub(half);
        let high = total + half;
        let span = high.as_millis().saturating_sub(low.as_millis()) as u64;
        let chosen = if span == 0 {
            low
        } else {
            low + std::time::Duration::from_millis(fastrand::u64(..) % span)
        };
        tokio::time::sleep(chosen).await;
        let _ = attempt;
    }
}

/// Build the variant-appropriate `reqwest::Client`. The two paths
/// share the same timeouts and user-agent; the helper exists so
/// the constructors stay parallel.
fn build_client_for_variant(_variant: OpenAIVariant) -> Result<reqwest::Client> {
    tracing::trace!("build_client_for_variant: reqwest client for openai SDK");
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .connect_timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("moagan/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| {
            tracing::error!(error = %e, "build_client_for_variant: failed");
            Error::Provider {
                message: format!("build reqwest client: {e}"),
                http_status: None,
            }
        })
}

/// Pick the SDK variant from the endpoint URL the operator declared
/// in `config.toml`. The matching mirrors the legacy
/// [`crate::llm::wire_format::wire_format_from_url`] heuristic but
/// stays local to this SDK so the constructor can return a clean
/// `Error::InvalidArgs` instead of routing through the wire-format
/// helper's `WireFormatId`.
///
/// `/chat/completions` wins over `/responses` when both substrings
/// appear in the URL — operators who genuinely need both can name
/// them under different `[providers.*]` sections, the SDK only
/// handles one URL per instance.
fn detect_variant(endpoint: &str) -> Result<OpenAIVariant> {
    if endpoint.contains("/chat/completions") {
        Ok(OpenAIVariant::Chat)
    } else if endpoint.contains("/responses") {
        Ok(OpenAIVariant::Responses)
    } else {
        Err(Error::InvalidArgs(format!(
            "OpenAI endpoint '{endpoint}' has no recognised wire-format suffix \
             (/chat/completions, /responses)"
        )))
    }
}

/// Pick the SDK variant from the dispatcher-computed
/// [`crate::llm::wire_format::WireFormatId`]. Mirrors the URL
/// detection in [`detect_variant`]; the dispatcher prefers this
/// path because it has the wire format resolved already.
fn variant_from_wire_format(id: crate::llm::wire_format::WireFormatId) -> Result<OpenAIVariant> {
    match id {
        crate::llm::wire_format::WireFormatId::OpenAICompatible => Ok(OpenAIVariant::Chat),
        crate::llm::wire_format::WireFormatId::OpenAI => Ok(OpenAIVariant::Responses),
        crate::llm::wire_format::WireFormatId::Anthropic => Err(Error::InvalidArgs(
            "OpenAIClient does not handle the Anthropic wire; use AnthropicClient".into(),
        )),
    }
}

impl std::fmt::Debug for OpenAIClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAIClient")
            .field("variant", &self.variant)
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("provider_max_tokens", &self.provider_max_tokens)
            .field("kind_hard_cap", &self.kind_hard_cap)
            .field("omit_max_tokens", &self.omit_max_tokens)
            .field("max_tokens_table", &"<shared>")
            .finish()
    }
}

/// Translate an SDK-side [`LlmRequest`] into the legacy
/// [`LegacyRequest`] the wire-body builder expects. The translation
/// is **lossless** for every wire-side field: `role`, `model`,
/// `system`, `user`, `max_tokens`, `temperature`, `top_p`,
/// `response_schema`, `stream`, `extra_messages`, `attachments`,
/// and `tool_choice` all propagate verbatim. The single additive
/// field on `LlmRequest` (`top_k`) is dropped — neither the
/// chat-completions wire nor the Responses wire exposes a `top_k`
/// knob, so the field has no place to land on the wire.
///
/// Callers that need the SHA / wire body must clone the result so
/// the safety-clamp / `omit_param` mutations do not leak into the
/// caller's copy.
fn legacy_request_from_llm(req: &LlmRequest) -> LegacyRequest {
    LegacyRequest {
        role: req.role,
        model: req.model.clone(),
        system: req.system.clone(),
        user: req.user.clone(),
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        response_schema: req.response_schema.clone(),
        stream: req.stream,
        extra_messages: req.extra_messages.clone(),
        attachments: req.attachments.clone(),
        tool_choice: req.tool_choice.clone(),
    }
}

#[async_trait]
impl LlmClient for OpenAIClient {
    fn sdk_type(&self) -> &'static str {
        // Stable SDK identifier the URL-path dispatcher (#922)
        // routes on. Mirrors `WireFormatId::OpenAICompatible.as_str()`
        // for the chat variant and `WireFormatId::OpenAI.as_str()`
        // for the Responses variant so the audit log and the SDK
        // trait agree on the literal.
        match self.variant {
            OpenAIVariant::Chat => WireFormatId::OpenAICompatible.as_str(),
            OpenAIVariant::Responses => WireFormatId::OpenAI.as_str(),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> LlmCapabilities {
        // Thin newtype wrapper over the existing capability matrix.
        // The `Deref` impl lets call sites reach `wire_format_id()`
        // and the `for_*` constructors without an extra hop, per
        // #900 D3 ("no compat layer"). The DeepSeek section's
        // distinctive surface (the kind cap, the deepseek-tagged
        // capability matrix) is preserved by checking
        // `name == "deepseek"` exactly the way the legacy
        // `OpenAICompatibleProvider::capabilities` does.
        let base = match (self.variant, self.name.as_str()) {
            (OpenAIVariant::Chat, "deepseek") => ProviderCapabilities::for_deepseek(),
            (OpenAIVariant::Chat, _) => ProviderCapabilities::for_openai_compat(),
            (OpenAIVariant::Responses, _) => ProviderCapabilities::for_opencode_responses(),
        };
        LlmCapabilities(base)
    }

    async fn send(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let legacy_req = legacy_request_from_llm(req);
        let (status, resp) = self.send_with_safety_clamp(&legacy_req, true).await?;
        Ok(LlmResponse::from_parts(status, resp))
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // D8 invariant: the wire body `send` will transmit (with
        // the safety clamp applied) is the exact byte sequence
        // the caller hashes here. Translate the SDK request into
        // a legacy request, apply the same clamp `send` applies,
        // build the wire body via the same free function the
        // legacy provider uses, then SHA-256 the JSON.
        let mut legacy_req = legacy_request_from_llm(req);
        let cap = crate::llm::max_tokens::resolve_max_tokens(
            self.name(),
            self.model(),
            self.max_tokens_table.as_deref(),
            self.provider_max_tokens,
            self.kind_hard_cap_for_variant(),
        );
        if let Some(n) = legacy_req.max_tokens {
            legacy_req.max_tokens = Some(n.min(cap));
        }
        let bytes = match self.variant {
            OpenAIVariant::Chat => {
                let body = build_chat_request_body(&self.model, &legacy_req);
                serde_json::to_vec(&body)
            }
            OpenAIVariant::Responses => {
                let body =
                    build_responses_body(&legacy_req, &self.model, false, self.omit_max_tokens);
                serde_json::to_vec(&body)
            }
        }
        .map_err(|e| {
            tracing::error!(error = %e, "OpenAIClient::body_sha256: encode failed");
            Error::Provider {
                message: format!("encode request body: {e}"),
                http_status: None,
            }
        })?;
        let digest = crate::ids::sha256_hex(&bytes);
        tracing::trace!(digest = %digest, "OpenAIClient::body_sha256");
        Ok(digest)
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Probe path: skip the safety clamp so the auto-probe
        // sees the upstream's real boundary instead of a clobbered
        // value. Mirrors the legacy
        // `OpenAICompatibleProvider::send_probe` and
        // `OpenAICompatProvider::send_probe` semantics.
        let legacy_req = legacy_request_from_llm(req);
        let (status, resp) = self.send_with_safety_clamp(&legacy_req, false).await?;
        Ok(LlmResponse::from_parts(status, resp))
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Chat variant: when `kind_hard_cap` is set (DeepSeek-
        // direct wires `Some(DEEPSEEK_MAX_TOKENS_CAP)`), short-
        // circuit the exponential probe at the kind cap. Mirrors
        // `OpenAICompatibleProvider::max_tokens_probe_ceiling`.
        // Responses variant: the OpenCode Responses upstream has
        // no documented wire-side ceiling (the legacy default
        // returns `u32::MAX`); the auto-probe walks the full
        // `2^1..2^30` exponential phase.
        match self.variant {
            OpenAIVariant::Chat => self
                .kind_hard_cap
                .unwrap_or(crate::llm::probe::MAX_AUTOPROBE_CEILING),
            OpenAIVariant::Responses => u32::MAX,
        }
    }

    async fn count_tokens(&self, _text: &str) -> Option<u64> {
        // Same heuristic `OpenAICompatibleProvider::count_tokens`
        // and `OpenAICompatProvider` use today: the SDK has no
        // built-in tokeniser so the count is best-effort `None`.
        // The dispatcher falls back to its own estimator.
        None
    }
}

impl OpenAIClient {
    /// Shared HTTP body between `send` and `send_probe`. Routes
    /// to the variant-specific transport; both lifts the legacy
    /// provider's `send_with_safety_clamp` rather than delegating
    /// so the SDK is self-contained.
    ///
    /// When `safety_clamp = true` the wire body is capped by every
    /// layer (operator override + kind cap + table); when `false`
    /// the wire body carries `req.max_tokens` verbatim subject only
    /// to the [`crate::llm::probe::MIN_AUTOPROBE_FLOOR`] minimum.
    async fn send_with_safety_clamp(
        &self,
        req: &LegacyRequest,
        safety_clamp: bool,
    ) -> Result<(u16, LegacyResponse)> {
        match self.variant {
            OpenAIVariant::Chat => self.send_chat_with_safety_clamp(req, safety_clamp).await,
            OpenAIVariant::Responses => {
                self.send_responses_with_safety_clamp(req, safety_clamp)
                    .await
            }
        }
    }

    /// Chat-completions HTTP transport. Lifted byte-for-byte from
    /// [`crate::llm::openai_compatible::OpenAICompatibleProvider::send_with_safety_clamp`]
    /// so the SDK is self-contained (the legacy provider becomes
    /// a thin dispatcher entry point in #922 and the legacy impl
    /// stays untouched until #933 deletes it).
    async fn send_chat_with_safety_clamp(
        &self,
        req: &LegacyRequest,
        safety_clamp: bool,
    ) -> Result<(u16, LegacyResponse)> {
        let url = self.chat_url();
        // Probe path uses `max_retries = 0`: a 4xx IS the
        // algorithm's signal (max-tokens rejection); retrying it
        // wastes the 5s probe timeout and risks masking the
        // boundary if a retry happens to succeed. Production path
        // keeps the existing self.max_retries (3) for transient
        // 5xx storms.
        let max_retries = if safety_clamp { self.max_retries } else { 0 };
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = build_chat_request_body(&self.model, req);
            let body = if safety_clamp {
                // v0.13.0 B-1 PR #3: the env -> cached ->
                // operator_cap -> kind_hard_cap ->
                // DEFAULT_MAX_TOKENS chain lives in
                // `crate::llm::max_tokens::resolve_max_tokens`.
                // The kind-level hard cap (e.g.
                // `DEEPSEEK_MAX_TOKENS_CAP = 393_216` for
                // DeepSeek-direct) flows through the helper as
                // the `kind_hard_cap` argument; the operator TOML
                // override is `provider_max_tokens`.
                //
                // `max_tokens = None` (set by the auto-healing
                // `param_rejections` path) is preserved through
                // the chain: the wire body omits the field so
                // the upstream accepts the request without the
                // cap.
                let cap = crate::llm::max_tokens::resolve_max_tokens(
                    self.name(),
                    self.model(),
                    self.max_tokens_table.as_deref(),
                    self.provider_max_tokens,
                    self.kind_hard_cap,
                );
                if let Some(n) = body.max_tokens {
                    if n > cap {
                        let mut next = body;
                        next.max_tokens = Some(cap);
                        next
                    } else {
                        body
                    }
                } else {
                    body
                }
            } else {
                // Probe path: bypass every cap. Floor ensures we
                // never ask for `max_tokens < 1024` (some
                // upstreams reject the request outright below
                // that minimum). `None` stays `None` so the probe
                // still honours any explicit request to drop the
                // field.
                if let Some(n) = body.max_tokens {
                    if n < crate::llm::probe::MIN_AUTOPROBE_FLOOR {
                        let mut next = body;
                        next.max_tokens = Some(crate::llm::probe::MIN_AUTOPROBE_FLOOR);
                        next
                    } else {
                        body
                    }
                } else {
                    body
                }
            };
            let request_started = std::time::Instant::now();
            tracing::debug!(
                provider = %self.name,
                attempt,
                stage = "http.request.started",
                variant = "chat",
                "Provider HTTP stage"
            );
            let result = self
                .client
                .post(&url)
                .bearer_auth(self.api_key.expose())
                .json(&body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let code = status.as_u16();
                    tracing::debug!(
                        provider = %self.name,
                        attempt,
                        stage = "http.headers.received",
                        status = code,
                        elapsed_ms = request_started.elapsed().as_millis(),
                        variant = "chat",
                        "Provider HTTP stage"
                    );
                    if status.is_success() {
                        let parsed: ChatResponseWire =
                            resp.json().await.map_err(|e| Error::Provider {
                                message: format!("decode: {e}"),
                                http_status: None,
                            })?;
                        let choice =
                            parsed
                                .choices
                                .into_iter()
                                .next()
                                .ok_or_else(|| Error::Provider {
                                    message: "openai-compat: empty choices array".into(),
                                    http_status: None,
                                })?;
                        let finish_reason = choice.finish_reason;
                        let truncated = finish_reason.as_deref() == Some("length");
                        let usage = parsed.usage.unwrap_or_default();
                        let text = choice.message.content;
                        check_size("response", text.len(), MAX_RESPONSE_BYTES)?;
                        let response = LegacyResponse {
                            text,
                            finish_reason,
                            truncated,
                            usage: Usage {
                                input_tokens: usage.prompt_tokens,
                                output_tokens: usage.completion_tokens,
                                cache_read: 0,
                                cache_creation: 0,
                            },
                        };
                        return Ok((code, response));
                    }
                    let raw = resp.text().await.unwrap_or_default();
                    if attempt >= max_retries {
                        return Err(Error::Provider {
                            message: format!(
                                "openai-compat: HTTP {code} after {attempt} attempts: {raw}"
                            ),
                            http_status: Some(code),
                        });
                    }
                    Self::sleep_with_jitter(attempt, None).await;
                }
                Err(e) => {
                    if attempt >= max_retries {
                        return Err(Error::Provider {
                            message: format!("openai-compat: network: {e}"),
                            http_status: None,
                        });
                    }
                    Self::sleep_with_jitter(attempt, None).await;
                }
            }
        }
    }

    /// Responses HTTP transport. Lifted byte-for-byte from
    /// [`crate::llm::openai_compat::OpenAICompatProvider::send_with_safety_clamp`]
    /// so the SDK is self-contained. Handles both the streaming
    /// (`req.stream == true`) and non-streaming paths; the
    /// streaming variant threads the SSE response through
    /// [`accumulate_sse_responses`].
    async fn send_responses_with_safety_clamp(
        &self,
        req: &LegacyRequest,
        safety_clamp: bool,
    ) -> Result<(u16, LegacyResponse)> {
        let url = self.responses_url();
        if req.stream {
            return self.send_responses_streaming(req, &url).await;
        }
        // Probe path uses `max_retries = 0`: a 4xx IS the
        // algorithm's signal (max-tokens rejection); retrying it
        // wastes the 5s probe timeout and risks masking the
        // boundary if a retry happens to succeed. Production path
        // keeps the existing self.max_retries (3) for transient
        // 5xx storms.
        let max_retries = if safety_clamp { self.max_retries } else { 0 };
        let mut req = req.clone();
        if safety_clamp {
            // v0.13.0 B-1 PR #3: route through
            // `crate::llm::max_tokens::resolve_max_tokens` so the
            // env -> cached -> operator_cap -> DEFAULT_MAX_TOKENS
            // chain is centralised. The OpenAI-compat path has no
            // kind-level hard cap (the 16_384-token opencode
            // chat-completions ceiling was lifted in v0.10), so
            // `kind_hard_cap` is `None`.
            //
            // `max_tokens = None` (set by the auto-healing
            // `param_rejections` path) is preserved through the
            // chain: the wire body omits the field so the
            // upstream accepts the request without the cap.
            let cap = crate::llm::max_tokens::resolve_max_tokens(
                self.name(),
                self.model(),
                self.max_tokens_table.as_deref(),
                self.provider_max_tokens,
                None,
            );
            if let Some(n) = req.max_tokens {
                req.max_tokens = Some(n.min(cap));
            }
        } else {
            // Probe path: bypass every cap. Floor ensures we
            // never ask for `max_tokens < 1024` (some upstreams
            // reject the request outright below that minimum).
            // `None` stays `None` so the probe honours any
            // explicit request to drop the field.
            if let Some(n) = req.max_tokens {
                req.max_tokens = Some(n.max(crate::llm::probe::MIN_AUTOPROBE_FLOOR));
            }
        }
        let max_tokens = self.wire_max_tokens(req.max_tokens);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = crate::llm::openai_compat::ResponsesRequest {
                model: &self.model,
                instructions: Some(&req.system),
                input: &req.user,
                max_tokens,
                temperature: req.temperature,
                top_p: req.top_p,
                text: responses_text_json_object(wants_response_format(req.role, &self.model)),
                stream: false,
            };
            let request_started = std::time::Instant::now();
            tracing::debug!(
                provider = %self.name,
                attempt,
                stage = "http.request.started",
                variant = "responses",
                "Provider HTTP stage"
            );
            let result = self
                .client
                .post(&url)
                .bearer_auth(self.api_key.expose())
                .json(&body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let status_code = status.as_u16();
                    tracing::debug!(
                        provider = %self.name,
                        attempt,
                        stage = "http.headers.received",
                        status = status_code,
                        elapsed_ms = request_started.elapsed().as_millis(),
                        variant = "responses",
                        "Provider HTTP stage"
                    );
                    if status.is_success() {
                        let decoded_at = std::time::Instant::now();
                        let parsed: crate::llm::openai_compat::ResponsesBody =
                            resp.json().await.map_err(|e| Error::Provider {
                                message: format!("decode: {e}"),
                                http_status: None,
                            })?;
                        tracing::debug!(
                            provider = %self.name,
                            attempt,
                            stage = "http.body.decoded",
                            status = status_code,
                            elapsed_ms = decoded_at.elapsed().as_millis(),
                            variant = "responses",
                            "Provider HTTP stage"
                        );
                        let mut text = String::new();
                        for out in parsed.output {
                            for c in out.content {
                                if c.kind == "output_text"
                                    && let Some(t) = c.text
                                {
                                    text.push_str(&t);
                                }
                            }
                        }
                        let usage = parsed.usage.unwrap_or_default();
                        check_size("response", text.len(), MAX_RESPONSE_BYTES)?;
                        let response = LegacyResponse {
                            text,
                            finish_reason: None,
                            truncated: false,
                            usage: Usage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read: 0,
                                cache_creation: 0,
                            },
                        };
                        return Ok((status_code, response));
                    }
                    let raw = resp.text().await.unwrap_or_default();
                    let err = match status_code {
                        401 | 403 => Error::InvalidApiKey {
                            message: format!("http {status_code}: {raw}"),
                            http_status: Some(status_code),
                        },
                        429 => Error::PlanExhausted {
                            message: format!("http {status_code}: {raw}"),
                            http_status: Some(status_code),
                        },
                        408 | 504 | 524 => Error::Timeout {
                            message: format!("http {status_code}: {raw}"),
                            http_status: Some(status_code),
                        },
                        _ => Error::Provider {
                            message: format!("http {status_code}: {raw}"),
                            http_status: Some(status_code),
                        },
                    };
                    let retryable = matches!(
                        err,
                        Error::Timeout { .. }
                            | Error::PlanExhausted { .. }
                            | Error::Provider { .. }
                    );
                    if !retryable || attempt >= max_retries {
                        return Err(err);
                    }
                    Self::sleep_with_jitter(attempt, None).await;
                }
                Err(e) => {
                    if attempt >= max_retries {
                        return Err(Error::Provider {
                            message: format!("network: {e}"),
                            http_status: None,
                        });
                    }
                    Self::sleep_with_jitter(attempt, None).await;
                }
            }
        }
    }

    /// Streaming variant of the Responses transport: sets
    /// `stream=true` on the wire body, reads the entire SSE
    /// response, and returns a single aggregated
    /// [`LegacyResponse`] with the joined text and the terminal
    /// usage block.
    async fn send_responses_streaming(
        &self,
        req: &LegacyRequest,
        url: &str,
    ) -> Result<(u16, LegacyResponse)> {
        let mut req = req.clone();
        let cap = crate::llm::max_tokens::resolve_max_tokens(
            self.name(),
            self.model(),
            self.max_tokens_table.as_deref(),
            self.provider_max_tokens,
            None,
        );
        if let Some(n) = req.max_tokens {
            req.max_tokens = Some(n.min(cap));
        }
        let body = build_responses_body(&req, &self.model, true, self.omit_max_tokens);
        let request_started = std::time::Instant::now();
        let resp = self
            .client
            .post(url)
            .bearer_auth(self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider {
                message: format!("network: {e}"),
                http_status: None,
            })?;
        let status = resp.status();
        let status_code = status.as_u16();
        if !status.is_success() {
            let raw = resp.text().await.unwrap_or_default();
            let err = match status_code {
                401 | 403 => Error::InvalidApiKey {
                    message: format!("http {status_code}: {raw}"),
                    http_status: Some(status_code),
                },
                429 => Error::PlanExhausted {
                    message: format!("http {status_code}: {raw}"),
                    http_status: Some(status_code),
                },
                408 | 504 | 524 => Error::Timeout {
                    message: format!("http {status_code}: {raw}"),
                    http_status: Some(status_code),
                },
                _ => Error::Provider {
                    message: format!("http {status_code}: {raw}"),
                    http_status: Some(status_code),
                },
            };
            return Err(err);
        }
        let bytes = resp.bytes().await.map_err(|e| Error::Provider {
            message: format!("stream body read: {e}"),
            http_status: None,
        })?;
        tracing::debug!(
            provider = %self.name,
            status = status_code,
            elapsed_ms = request_started.elapsed().as_millis(),
            variant = "responses",
            "Provider HTTP stage (sse)"
        );
        let (text, usage) = crate::llm::openai_compat::accumulate_sse_responses(&bytes)?;
        check_size("response", text.len(), MAX_RESPONSE_BYTES)?;
        let response = LegacyResponse {
            text,
            finish_reason: None,
            truncated: false,
            usage: Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read: 0,
                cache_creation: 0,
            },
        };
        Ok((status_code, response))
    }

    /// Translate the request's `max_tokens` (`Option<u32>`) into
    /// the wire-side value the Responses payload carries:
    ///
    /// - `None` → `None` (the auto-healing path asked us to drop
    ///   the field, and we honour that).
    /// - `Some(n)` with `omit_max_tokens = true` → `None` (the
    ///   operator pinned the provider config to always omit the
    ///   field — required for `gpt-5.6-luna`).
    /// - `Some(n)` with `omit_max_tokens = false` → `Some(n)`
    ///   (the wire builder carries the value).
    ///
    /// Mirrors [`crate::llm::openai_compat::OpenAICompatProvider::wire_max_tokens`].
    fn wire_max_tokens(&self, requested: Option<u32>) -> Option<u32> {
        if requested.is_none() || self.omit_max_tokens {
            None
        } else {
            requested
        }
    }

    /// Return the variant-appropriate `kind_hard_cap` argument for
    /// `resolve_max_tokens`. The Responses variant has no kind
    /// cap (`OpenAICompatProvider::from_resolved` always passes
    /// `None`); the chat variant uses whatever cap the dispatcher
    /// wired (DeepSeek-direct → `Some(DEEPSEEK_MAX_TOKENS_CAP)`,
    /// everything else → `None`).
    fn kind_hard_cap_for_variant(&self) -> Option<u32> {
        match self.variant {
            OpenAIVariant::Chat => self.kind_hard_cap,
            OpenAIVariant::Responses => None,
        }
    }
}

/// Chat-completions response wire shape. Mirrors the legacy
/// `OpenAICompatibleProvider::ChatResponse` (private to that
/// module) so the SDK can decode without taking a dependency on
/// the legacy type's private visibility. `Debug + Default` derive
/// keeps the SDK self-contained.
#[derive(Debug, serde::Deserialize)]
struct ChatResponseWire {
    choices: Vec<ChatChoiceWire>,
    #[serde(default)]
    usage: Option<ChatUsageWire>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatChoiceWire {
    message: ChatMessageOutWire,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatMessageOutWire {
    content: String,
}

#[derive(Debug, serde::Deserialize, Default)]
struct ChatUsageWire {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    //! Unit tests pinning the D8 invariant: `body_sha256(req)` is
    //! the SHA-256 of the exact wire body `send(req)` would
    //! transmit. Plus wiremock regression tests that compare the
    //! legacy `OpenAICompatibleProvider::send` /
    //! `OpenAICompatProvider::send` and the new
    //! `OpenAIClient::send` byte-for-byte on a shared mock.
    use super::*;
    use crate::config::ModelConfig;
    use crate::llm::provider::Provider;
    use crate::llm::role::Role;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn llm_req(user: &str) -> LlmRequest {
        LlmRequest {
            role: Role::Sketch,
            model: "MiniMax-M3".into(),
            system: "sys".into(),
            user: user.into(),
            max_tokens: Some(1024),
            temperature: Some(0.7),
            top_p: Some(0.95),
            top_k: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn chat_cfg(endpoint: &str) -> ProviderConfig {
        ProviderConfig {
            models: vec![ModelConfig {
                id: "kimi-k3".into(),
                endpoint: Some(endpoint.into()),
                max_tokens: None,
                omit_max_tokens: false,
            }],
            endpoint: Some(endpoint.into()),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        }
    }

    fn responses_cfg(endpoint: &str) -> ProviderConfig {
        ProviderConfig {
            models: vec![ModelConfig {
                id: "gpt-5.6-luna".into(),
                endpoint: Some(endpoint.into()),
                max_tokens: None,
                omit_max_tokens: false,
            }],
            endpoint: Some(endpoint.into()),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        }
    }

    fn legacy_req_for(req: &LlmRequest) -> LegacyRequest {
        LegacyRequest {
            role: req.role,
            model: req.model.clone(),
            system: req.system.clone(),
            user: req.user.clone(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            response_schema: req.response_schema.clone(),
            stream: req.stream,
            extra_messages: req.extra_messages.clone(),
            attachments: req.attachments.clone(),
            tool_choice: req.tool_choice.clone(),
        }
    }

    /// The variant is picked from the endpoint URL path suffix.
    /// `/chat/completions` → `OpenAIVariant::Chat`.
    #[test]
    fn variant_picked_from_url_chat() {
        let client = OpenAIClient::new(
            &chat_cfg("https://opencode.ai/zen/go/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new chat");
        assert_eq!(client.variant(), OpenAIVariant::Chat);
        assert_eq!(client.sdk_type(), "openai_compatible");
    }

    /// `/responses` → `OpenAIVariant::Responses`.
    #[test]
    fn variant_picked_from_url_responses() {
        let client = OpenAIClient::new(
            &responses_cfg("https://opencode.ai/zen/go/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new responses");
        assert_eq!(client.variant(), OpenAIVariant::Responses);
        assert_eq!(client.sdk_type(), "openai");
    }

    /// An endpoint URL with neither suffix → `Error::InvalidArgs`.
    /// Pins the contract the dispatcher relies on so a future
    /// contributor cannot silently fall through to the wrong
    /// wire format.
    #[test]
    fn variant_rejects_unknown_url() {
        let err = OpenAIClient::new(
            &chat_cfg("https://example.invalid/v1/foo"),
            SecretString::new("dummy".into()),
        )
        .expect_err("unknown URL must error");
        match err {
            Error::InvalidArgs(msg) => {
                assert!(
                    msg.contains("/chat/completions"),
                    "error message must list recognised suffixes, got {msg:?}"
                );
                assert!(
                    msg.contains("/responses"),
                    "error message must list recognised suffixes, got {msg:?}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    /// Chat variant: `body_sha256` matches the SHA-256 of the wire
    /// body `build_chat_request_body` produces from a legacy
    /// request that mirrors the `LlmRequest` (after the same
    /// safety clamp). Pins the D8 invariant.
    #[test]
    fn body_sha256_matches_send_wire_body_chat() {
        let client = OpenAIClient::new(
            &chat_cfg("http://localhost/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new chat");
        let req = llm_req("hello world");
        let mut legacy_req = legacy_req_for(&req);
        let cap = crate::llm::max_tokens::resolve_max_tokens(
            client.name(),
            client.model(),
            client.max_tokens_table.as_deref(),
            client.provider_max_tokens,
            client.kind_hard_cap_for_variant(),
        );
        if let Some(n) = legacy_req.max_tokens {
            legacy_req.max_tokens = Some(n.min(cap));
        }
        let wire_body = build_chat_request_body(&client.model, &legacy_req);
        let bytes = serde_json::to_vec(&wire_body).expect("build_chat_request_body serialises");
        let expected = sha256_hex(&bytes);
        let got = client.body_sha256(&req).expect("body_sha256");
        assert_eq!(
            got, expected,
            "chat body_sha256 must equal sha256(wire body)"
        );
        assert_eq!(got.len(), 64, "SHA-256 hex is 64 lowercase chars");
        assert!(got.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Responses variant: `body_sha256` matches the SHA-256 of the
    /// wire body `build_responses_body` produces (after the same
    /// safety clamp + `omit_max_tokens` translation).
    #[test]
    fn body_sha256_matches_send_wire_body_responses() {
        let client = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new responses");
        let req = llm_req("hello responses");
        let mut legacy_req = legacy_req_for(&req);
        let cap = crate::llm::max_tokens::resolve_max_tokens(
            client.name(),
            client.model(),
            client.max_tokens_table.as_deref(),
            client.provider_max_tokens,
            client.kind_hard_cap_for_variant(),
        );
        if let Some(n) = legacy_req.max_tokens {
            legacy_req.max_tokens = Some(n.min(cap));
        }
        let wire_body =
            build_responses_body(&legacy_req, &client.model, false, client.omit_max_tokens);
        let bytes = serde_json::to_vec(&wire_body).expect("build_responses_body serialises");
        let expected = sha256_hex(&bytes);
        let got = client.body_sha256(&req).expect("body_sha256");
        assert_eq!(
            got, expected,
            "responses body_sha256 must equal sha256(wire body)"
        );
        assert_eq!(got.len(), 64);
    }

    /// Chat variant with `kind_hard_cap = Some(393_216)` clamps a
    /// `max_tokens = 1_000_000` request to `393_216` on the wire.
    /// The SHA hashes the body that carries the clamped value.
    /// Pin the DeepSeek-direct kind-cap wiring.
    #[test]
    fn body_sha256_uses_kind_hard_cap() {
        let client = OpenAIClient::new(
            &chat_cfg("https://api.deepseek.com/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new deepseek")
        .with_kind_hard_cap(Some(crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP));
        let mut req = llm_req("clamp me");
        req.max_tokens = Some(1_000_000);
        let mut legacy_req = legacy_req_for(&req);
        let cap = crate::llm::max_tokens::resolve_max_tokens(
            client.name(),
            client.model(),
            client.max_tokens_table.as_deref(),
            client.provider_max_tokens,
            client.kind_hard_cap_for_variant(),
        );
        if let Some(n) = legacy_req.max_tokens {
            legacy_req.max_tokens = Some(n.min(cap));
        }
        let wire_body = build_chat_request_body(&client.model, &legacy_req);
        let json: serde_json::Value = serde_json::to_value(&wire_body).expect("body serialises");
        assert_eq!(
            json.get("max_tokens"),
            Some(&serde_json::json!(
                crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP
            )),
            "wire body must carry the kind-clamped value, got: {json}"
        );
        let expected = sha256_hex(&serde_json::to_vec(&wire_body).expect("vec"));
        let got = client.body_sha256(&req).expect("body_sha256");
        assert_eq!(
            got, expected,
            "SHA must reflect the kind-clamped max_tokens"
        );
    }

    /// `body_sha256` honours the `omit_max_tokens` field on the
    /// Responses variant: when the operator pinned the flag, the
    /// wire body drops `max_tokens` regardless of the request's
    /// value. The SHA hashes the body that omits the field.
    #[test]
    fn body_sha256_drops_omitted_params() {
        let client = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new responses")
        .with_omit_max_tokens(true);
        assert!(client.omit_max_tokens);

        let mut req = llm_req("drop max_tokens");
        req.max_tokens = Some(2048);
        let mut legacy_req = legacy_req_for(&req);
        let cap = crate::llm::max_tokens::resolve_max_tokens(
            client.name(),
            client.model(),
            client.max_tokens_table.as_deref(),
            client.provider_max_tokens,
            client.kind_hard_cap_for_variant(),
        );
        if let Some(n) = legacy_req.max_tokens {
            legacy_req.max_tokens = Some(n.min(cap));
        }
        let wire_body =
            build_responses_body(&legacy_req, &client.model, false, client.omit_max_tokens);
        let json: serde_json::Value = serde_json::to_value(&wire_body).expect("body serialises");
        assert!(
            json.get("max_tokens").is_none(),
            "wire body must omit max_tokens when omit_max_tokens=true, got: {json}"
        );
        let expected = sha256_hex(&serde_json::to_vec(&wire_body).expect("vec"));
        let got = client.body_sha256(&req).expect("body_sha256");
        assert_eq!(
            got, expected,
            "SHA must reflect the body that omits max_tokens"
        );
    }

    /// `capabilities()` advertises the OpenAI-compat wire for the
    /// chat variant and the Responses wire for the responses
    /// variant. Pins the newtype deref + capability-matrix
    /// routing the dispatcher relies on.
    #[test]
    fn capabilities_are_variant_appropriate() {
        let chat = OpenAIClient::new(
            &chat_cfg("http://localhost/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("chat");
        let cap = chat.capabilities();
        assert_eq!(cap.wire_format_id(), "openai_compatible");
        assert!(cap.prefers_openai_wire);
        assert!(!cap.prefers_responses_wire);
        let _inner: &ProviderCapabilities = &cap.0;

        let responses = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("responses");
        let cap = responses.capabilities();
        assert_eq!(cap.wire_format_id(), "openai");
        assert!(cap.prefers_responses_wire);
        assert!(!cap.prefers_openai_wire);
    }

    /// `max_tokens_probe_ceiling` returns the variant-appropriate
    /// ceiling. Chat variant without a `kind_hard_cap` keeps the
    /// default `MAX_AUTOPROBE_CEILING`; with `kind_hard_cap =
    /// Some(N)` the ceiling is `N`. Responses variant always
    /// returns `u32::MAX`.
    #[test]
    fn max_tokens_probe_ceiling_per_variant() {
        let chat = OpenAIClient::new(
            &chat_cfg("http://localhost/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("chat");
        assert_eq!(
            chat.max_tokens_probe_ceiling(),
            crate::llm::probe::MAX_AUTOPROBE_CEILING
        );
        let chat_capped = chat
            .clone()
            .with_kind_hard_cap(Some(crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP));
        assert_eq!(
            chat_capped.max_tokens_probe_ceiling(),
            crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP
        );
        let responses = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("responses");
        assert_eq!(responses.max_tokens_probe_ceiling(), u32::MAX);
    }

    /// `from_resolved` derives the variant from the dispatcher's
    /// `wire_format` field and the kind cap from the section
    /// name (`section == "deepseek"` → `Some(DEEPSEEK_MAX_TOKENS_CAP)`).
    #[test]
    fn from_resolved_wires_kind_cap_for_deepseek() {
        // `from_resolved` calls `api_keys::lookup_key`, which reads
        // the section-keyed env var. `unsafe { set_var }` is fine
        // here because cargo's per-test parallelism already serialises
        // the `lookup_key` calls through the test-support lock (see
        // `scripts/check-non-interactive-env-guard.sh` for the same
        // pattern) and the only consumer is this test.
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY", "dummy-for-from-resolved-test");
            std::env::set_var("OPENCODE_API_KEY", "dummy-for-from-resolved-test");
        }
        let resolved = crate::config::ResolvedModelConfig {
            section: "deepseek".into(),
            id: "deepseek-v4-flash".into(),
            endpoint: "https://api.deepseek.com/v1/chat/completions".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            wire_format: crate::llm::wire_format::WireFormatId::OpenAICompatible,
            omit_max_tokens: false,
        };
        let client = OpenAIClient::from_resolved(&resolved).expect("from_resolved");
        assert_eq!(client.variant(), OpenAIVariant::Chat);
        assert_eq!(
            client.kind_hard_cap,
            Some(crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP)
        );

        let resolved_opencode = crate::config::ResolvedModelConfig {
            section: "opencode".into(),
            id: "kimi-k3".into(),
            endpoint: "https://opencode.ai/zen/go/v1/chat/completions".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            wire_format: crate::llm::wire_format::WireFormatId::OpenAICompatible,
            omit_max_tokens: false,
        };
        let client_opencode =
            OpenAIClient::from_resolved(&resolved_opencode).expect("from_resolved opencode");
        assert_eq!(client_opencode.variant(), OpenAIVariant::Chat);
        assert_eq!(client_opencode.kind_hard_cap, None);

        let resolved_responses = crate::config::ResolvedModelConfig {
            section: "opencode".into(),
            id: "gpt-5.6-luna".into(),
            endpoint: "https://opencode.ai/zen/go/v1/responses".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            wire_format: crate::llm::wire_format::WireFormatId::OpenAI,
            omit_max_tokens: false,
        };
        let client_responses =
            OpenAIClient::from_resolved(&resolved_responses).expect("from_resolved responses");
        assert_eq!(client_responses.variant(), OpenAIVariant::Responses);
        assert_eq!(client_responses.kind_hard_cap, None);
    }

    /// Legacy (`OpenAICompatibleProvider`) and new (`OpenAIClient`)
    /// SDK impls must emit a wire body byte-for-byte identical for
    /// the same input. The wiremock captures the bytes each impl
    /// POSTs and the test asserts equality. Pins the D8 invariant
    /// the migration PRs rely on.
    #[tokio::test]
    async fn wire_body_byte_identical_to_legacy_chat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2}
            })))
            .expect(2)
            .mount(&server)
            .await;

        // The SDK constructor requires the endpoint URL to carry
        // the wire-format suffix (it's how the SDK picks the
        // variant — see `detect_variant`). Wire the URL with the
        // `/chat/completions` suffix so the SDK's variant detection
        // accepts it and both providers POST to the wiremock path
        // matching the `path()` matcher.
        let endpoint = format!("{}/v1/chat/completions", server.uri());
        let cfg = ProviderConfig {
            models: vec![ModelConfig {
                id: "kimi-k3".into(),
                endpoint: Some(endpoint.clone()),
                max_tokens: Some(8192),
                omit_max_tokens: false,
            }],
            endpoint: Some(endpoint),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        };

        let legacy = crate::llm::openai_compatible::OpenAICompatibleProvider::new(
            &cfg,
            SecretString::new("dummy".into()),
        )
        .expect("OpenAICompatibleProvider::new");
        let new_sdk =
            OpenAIClient::new(&cfg, SecretString::new("dummy".into())).expect("OpenAIClient::new");

        let legacy_req = LegacyRequest {
            role: Role::Sketch,
            model: "kimi-k3".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: Some(8192),
            temperature: Some(0.7),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let llm_req_for_new = LlmRequest {
            role: legacy_req.role,
            model: legacy_req.model.clone(),
            system: legacy_req.system.clone(),
            user: legacy_req.user.clone(),
            max_tokens: legacy_req.max_tokens,
            temperature: legacy_req.temperature,
            top_p: legacy_req.top_p,
            top_k: None,
            response_schema: legacy_req.response_schema.clone(),
            stream: legacy_req.stream,
            extra_messages: legacy_req.extra_messages.clone(),
            attachments: legacy_req.attachments.clone(),
            tool_choice: legacy_req.tool_choice.clone(),
        };

        let (legacy_status, _legacy_resp) = legacy.send(&legacy_req).await.expect("legacy send");
        assert_eq!(legacy_status, 200);
        let new_resp = new_sdk.send(&llm_req_for_new).await.expect("new SDK send");
        assert_eq!(new_resp.http_status, 200);

        let received = server
            .received_requests()
            .await
            .expect("wiremock received_requests");
        assert_eq!(received.len(), 2, "exactly two POSTs must hit the mock");
        let legacy_body = &received[0].body;
        let new_body = &received[1].body;
        assert_eq!(
            legacy_body,
            new_body,
            "OpenAICompatibleProvider and OpenAIClient must emit byte-identical wire bodies; \
             legacy hex: {}, new hex: {}",
            hex::encode(legacy_body),
            hex::encode(new_body)
        );

        let sha = new_sdk.body_sha256(&llm_req_for_new).expect("body_sha256");
        let manual = sha256_hex(new_body);
        assert_eq!(
            sha, manual,
            "body_sha256 must hash the byte sequence the mock received"
        );
    }

    /// Legacy (`OpenAICompatProvider`) and new (`OpenAIClient`)
    /// SDK impls must emit a wire body byte-for-byte identical on
    /// the Responses path. Pins the D8 invariant for the second
    /// variant.
    #[tokio::test]
    async fn wire_body_byte_identical_to_legacy_responses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "content": [
                        {"type": "output_text", "text": "ok"}
                    ]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 2}
            })))
            .expect(2)
            .mount(&server)
            .await;

        let endpoint = format!("{}/v1/responses", server.uri());
        let cfg = ProviderConfig {
            models: vec![ModelConfig {
                id: "gpt-5.6-luna".into(),
                endpoint: Some(endpoint.clone()),
                max_tokens: Some(8192),
                omit_max_tokens: false,
            }],
            endpoint: Some(endpoint),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        };

        let legacy = crate::llm::openai_compat::OpenAICompatProvider::new(
            &cfg,
            SecretString::new("dummy".into()),
        )
        .expect("OpenAICompatProvider::new");
        let new_sdk =
            OpenAIClient::new(&cfg, SecretString::new("dummy".into())).expect("OpenAIClient::new");

        let legacy_req = LegacyRequest {
            role: Role::Intake,
            model: "gpt-5.6-luna".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: Some(8192),
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let llm_req_for_new = LlmRequest {
            role: legacy_req.role,
            model: legacy_req.model.clone(),
            system: legacy_req.system.clone(),
            user: legacy_req.user.clone(),
            max_tokens: legacy_req.max_tokens,
            temperature: legacy_req.temperature,
            top_p: legacy_req.top_p,
            top_k: None,
            response_schema: legacy_req.response_schema.clone(),
            stream: legacy_req.stream,
            extra_messages: legacy_req.extra_messages.clone(),
            attachments: legacy_req.attachments.clone(),
            tool_choice: legacy_req.tool_choice.clone(),
        };

        let (legacy_status, _legacy_resp) = legacy.send(&legacy_req).await.expect("legacy send");
        assert_eq!(legacy_status, 200);
        let new_resp = new_sdk.send(&llm_req_for_new).await.expect("new SDK send");
        assert_eq!(new_resp.http_status, 200);

        let received = server
            .received_requests()
            .await
            .expect("wiremock received_requests");
        assert_eq!(received.len(), 2);
        let legacy_body = &received[0].body;
        let new_body = &received[1].body;
        assert_eq!(
            legacy_body,
            new_body,
            "OpenAICompatProvider and OpenAIClient must emit byte-identical wire bodies; \
             legacy hex: {}, new hex: {}",
            hex::encode(legacy_body),
            hex::encode(new_body)
        );

        let sha = new_sdk.body_sha256(&llm_req_for_new).expect("body_sha256");
        let manual = sha256_hex(new_body);
        assert_eq!(
            sha, manual,
            "body_sha256 must hash the byte sequence the mock received"
        );
    }

    /// `legacy_request_from_llm` is lossless for every wire-side
    /// field, including all of `extra_messages` / `attachments` /
    /// `tool_choice`. Pins the translation contract the SDK
    /// dispatcher relies on. Mirrors the equivalent test on
    /// `AnthropicClient`.
    #[test]
    fn legacy_translation_is_lossless() {
        use crate::llm::wire::{Attachment, Message, ToolChoice};
        let req = LlmRequest {
            role: Role::Sketch,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: Some(16),
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(40),
            response_schema: Some(serde_json::json!({"type": "object"})),
            stream: true,
            extra_messages: vec![Message {
                role: "assistant".into(),
                content: "{".into(),
            }],
            attachments: vec![Attachment {
                mime: "image/png".into(),
                modality: "image".into(),
                data: vec![0x89, 0x50, 0x4e, 0x47],
            }],
            tool_choice: Some(ToolChoice::Required),
        };
        let legacy = legacy_request_from_llm(&req);
        assert_eq!(legacy.role, Role::Sketch);
        assert_eq!(legacy.model, "m");
        assert_eq!(legacy.system, "s");
        assert_eq!(legacy.user, "u");
        assert_eq!(legacy.max_tokens, Some(16));
        assert_eq!(legacy.temperature, Some(0.5));
        assert_eq!(legacy.top_p, Some(0.9));
        // Legacy `Request` has no `top_k` field — neither wire
        // exposes a `top_k` knob, so the field drops silently.
        assert_eq!(legacy.response_schema, req.response_schema);
        assert!(legacy.stream);
        assert_eq!(legacy.extra_messages.len(), 1);
        assert_eq!(legacy.extra_messages[0].role, "assistant");
        assert_eq!(legacy.extra_messages[0].content, "{");
        assert_eq!(legacy.attachments.len(), 1);
        assert_eq!(legacy.attachments[0].mime, "image/png");
        assert_eq!(legacy.attachments[0].modality, "image");
        assert_eq!(legacy.attachments[0].data, vec![0x89, 0x50, 0x4e, 0x47]);
        assert!(matches!(legacy.tool_choice, Some(ToolChoice::Required)));
    }

    /// `count_tokens()` returns `None` (matches the legacy
    /// heuristic). The dispatcher falls back to its own estimator.
    #[tokio::test]
    async fn count_tokens_returns_none() {
        let client = OpenAIClient::new(
            &chat_cfg("http://localhost/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new");
        let got = client.count_tokens("hello world").await;
        assert_eq!(got, None);
    }
}
