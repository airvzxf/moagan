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
use parking_lot::Mutex;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::llm::param_rejections::ParamRejectionsTable;
use crate::secret::SecretString;

use super::openai_body::{
    build_chat_request_body, build_responses_body, responses_text_json_object,
    wants_response_format,
};
use super::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse, Usage};
use crate::llm::capabilities::ProviderCapabilities;
use crate::llm::size_limits::{MAX_RESPONSE_BYTES, check_size};

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
///
/// #932 (D9) — `OpenAIClient` carries the run-level
/// param-rejections table behind interior mutability so the cascade
/// default impl of [`LlmClient::send`] can consult it via
/// [`LlmClient::param_rejections_table`]. The dispatcher injects the
/// table via [`LlmClient::set_param_rejections`] before the first
/// `send`. Wrapping the `Mutex` in an `Arc` keeps
/// `#[derive(Clone)]` sound (the lock itself is not `Clone`).
#[derive(Clone)]
pub struct OpenAIClient {
    variant: OpenAIVariant,
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: reqwest::Client,
    max_retries: u32,
    /// Optional param-rejection table the cascade default impl
    /// consults. `None` keeps the SDK on the pre-#932
    /// straight-send path. The `Arc<Mutex<...>>` is interior-
    /// mutable so the dispatcher's setter can write without
    /// `&mut self` while the surrounding struct stays `Clone`.
    param_rejections: Arc<Mutex<Option<Arc<ParamRejectionsTable>>>>,
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
    /// Mirrors [`super::openai_body::OpenAICompatibleProvider::new`]
    /// and
    /// [`super::openai_body::OpenAICompatProvider::new`] on
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
            param_rejections: Arc::new(Mutex::new(None)),
            omit_max_tokens,
        })
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
        // Only the Responses variant honours `omit_max_tokens` on
        // the wire (`gpt-5.6-luna` rejects the *presence* of the
        // field). The chat variant carries `false` regardless of
        // the operator flag.
        let omit_max_tokens = resolved.omit_max_tokens && variant == OpenAIVariant::Responses;
        tracing::info!(
            section = %resolved.section,
            model = %resolved.id,
            variant = ?variant,
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
            param_rejections: Arc::new(Mutex::new(None)),
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
/// [`crate::llm::client::WireFormatId::from_url`] heuristic but
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
/// [`WireFormatId`]. Mirrors the URL detection in [`detect_variant`];
/// the dispatcher prefers this path because it has the wire format
/// resolved already.
fn variant_from_wire_format(id: super::dispatcher::WireFormatId) -> Result<OpenAIVariant> {
    match id {
        super::dispatcher::WireFormatId::OpenAICompatible => Ok(OpenAIVariant::Chat),
        super::dispatcher::WireFormatId::OpenAI => Ok(OpenAIVariant::Responses),
        super::dispatcher::WireFormatId::Anthropic => Err(Error::InvalidArgs(
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
            .field("omit_max_tokens", &self.omit_max_tokens)
            .finish()
    }
}

/// Clone the request so the safety-clamp / `omit_param` mutations
/// do not leak into the caller's copy. The OpenAI-compat wire
/// bodies do not expose `top_k` directly — the chat-completions
/// body builder ignores it (no field on `ChatRequest`), and the
/// Responses wire does not advertise it either — so the field
/// carries through but never reaches the wire.
fn clone_for_wire(req: &LlmRequest) -> LlmRequest {
    req.clone()
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
            OpenAIVariant::Chat => super::dispatcher::WireFormatId::OpenAICompatible.as_str(),
            OpenAIVariant::Responses => super::dispatcher::WireFormatId::OpenAI.as_str(),
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

    async fn send_once(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let legacy_req = clone_for_wire(req);
        let (status, resp) = self.send_with_safety_clamp(&legacy_req, true).await?;
        Ok(LlmResponse {
            http_status: status,
            ..resp
        })
    }

    fn param_rejections_table(&self) -> Option<Arc<ParamRejectionsTable>> {
        self.param_rejections.lock().clone()
    }

    fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>) {
        *self.param_rejections.lock() = Some(table);
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // D8 invariant (phase 2 / CAP removal): the wire body `send`
        // transmits `req.max_tokens` verbatim — no clamp chain.
        // The audit hash is the SHA of the exact bytes
        // `build_chat_request_body` / `build_responses_body` will
        // emit. Same SHA path the production `send` takes.
        let legacy_req = clone_for_wire(req);
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

    fn effective_max_tokens(&self, req: &LlmRequest) -> u32 {
        // Post-CAP-removal: pass the caller's value through
        // verbatim. The audit-log hash matches the wire body
        // because `body_sha256` no longer applies any clamp. The
        // trait default (`req.max_tokens.unwrap_or(u32::MAX)`)
        // covers every variant; we keep the override only to
        // centralise the `body_sha256` ↔ `effective_max_tokens`
        // audit-hash contract.
        req.max_tokens.unwrap_or(u32::MAX)
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Probe path: skip the floor guard so the auto-probe sees
        // the upstream's real boundary. Mirrors the legacy
        // `OpenAICompatibleProvider::send_probe` semantics.
        let legacy_req = clone_for_wire(req);
        let (status, resp) = self.send_with_safety_clamp(&legacy_req, false).await?;
        Ok(LlmResponse {
            http_status: status,
            ..resp
        })
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Post-CAP-removal: no kind-level cap is enforced anymore.
        // The auto-probe walks the full `2^1..2^30` exponential
        // phase against both variants (the Responses upstream
        // has no documented wire-side ceiling; the chat upstream
        // is probed up to whatever it accepts).
        crate::llm::probe::MAX_AUTOPROBE_CEILING
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
    /// **Phase 2 (CAP removal)**: the wire body now carries
    /// `req.max_tokens` verbatim — `Some(n)` is sent as `n`,
    /// `None` is preserved as field-absent. The cascade auto-heal
    /// in [`crate::llm::client::LlmClient::send`] handles upstream
    /// rejection of `max_tokens` via the param-rejections table
    /// (see pattern #4C in `param_rejections.rs`).
    async fn send_with_safety_clamp(
        &self,
        req: &LlmRequest,
        safety_clamp: bool,
    ) -> Result<(u16, LlmResponse)> {
        match self.variant {
            OpenAIVariant::Chat => self.send_chat_with_safety_clamp(req, safety_clamp).await,
            OpenAIVariant::Responses => {
                self.send_responses_with_safety_clamp(req, safety_clamp)
                    .await
            }
        }
    }

    /// Chat-completions HTTP transport. Lifted byte-for-byte from
    /// [`super::openai_body::OpenAICompatibleProvider::send_with_safety_clamp`]
    /// so the SDK is self-contained (the legacy provider becomes
    /// a thin dispatcher entry point in #922 and the legacy impl
    /// stays untouched until #933 deletes it).
    async fn send_chat_with_safety_clamp(
        &self,
        req: &LlmRequest,
        safety_clamp: bool,
    ) -> Result<(u16, LlmResponse)> {
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
            let body = if !safety_clamp {
                // Probe path: floor ensures we never ask for
                // `max_tokens < 1024` (some upstreams reject the
                // request outright below that minimum). `None`
                // stays `None` so the probe still honours any
                // explicit request to drop the field.
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
            } else {
                body
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
                        let response = LlmResponse {
                            text,
                            finish_reason,
                            truncated,
                            usage: Usage {
                                input_tokens: usage.prompt_tokens,
                                output_tokens: usage.completion_tokens,
                                cache_read: 0,
                                cache_creation: 0,
                            },
                            http_status: code,
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
    /// [`super::openai_body::OpenAICompatProvider::send_with_safety_clamp`]
    /// so the SDK is self-contained. Handles both the streaming
    /// (`req.stream == true`) and non-streaming paths; the
    /// streaming variant threads the SSE response through
    /// [`accumulate_sse_responses`].
    async fn send_responses_with_safety_clamp(
        &self,
        req: &LlmRequest,
        safety_clamp: bool,
    ) -> Result<(u16, LlmResponse)> {
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
        if !safety_clamp {
            // Probe path: floor ensures we never ask for
            // `max_tokens < 1024` (some upstreams reject the request
            // outright below that minimum). `None` stays `None`
            // so the probe honours any explicit request to drop
            // the field.
            if let Some(n) = req.max_tokens {
                req.max_tokens = Some(n.max(crate::llm::probe::MIN_AUTOPROBE_FLOOR));
            }
        }
        let max_tokens = self.wire_max_tokens(req.max_tokens);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = super::openai_body::ResponsesRequest {
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
                        let parsed: super::openai_body::ResponsesBody =
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
                        let response = LlmResponse {
                            text,
                            finish_reason: None,
                            truncated: false,
                            usage: Usage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read: 0,
                                cache_creation: 0,
                            },
                            http_status: status_code,
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
    /// [`LlmResponse`] with the joined text and the terminal
    /// usage block.
    async fn send_responses_streaming(
        &self,
        req: &LlmRequest,
        url: &str,
    ) -> Result<(u16, LlmResponse)> {
        let req = req.clone();
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
        let (text, usage) = super::openai_body::accumulate_sse_responses(&bytes)?;
        check_size("response", text.len(), MAX_RESPONSE_BYTES)?;
        let response = LlmResponse {
            text,
            finish_reason: None,
            truncated: false,
            usage: Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read: 0,
                cache_creation: 0,
            },
            http_status: status_code,
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
    /// Mirrors [`super::openai_body::OpenAICompatProvider::wire_max_tokens`].
    fn wire_max_tokens(&self, requested: Option<u32>) -> Option<u32> {
        if requested.is_none() || self.omit_max_tokens {
            None
        } else {
            requested
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
    use crate::llm::role::Role;
    use sha2::{Digest, Sha256};

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
            top_p_auto_enabled: None,
            top_k_auto_enabled: None,
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
            top_p_auto_enabled: None,
            top_k_auto_enabled: None,
            plan: None,
        }
    }

    fn legacy_req_for(req: &LlmRequest) -> LlmRequest {
        LlmRequest {
            role: req.role,
            model: req.model.clone(),
            system: req.system.clone(),
            user: req.user.clone(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            top_k: req.top_k,
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
        let sdk_client = OpenAIClient::new(
            &chat_cfg("https://opencode.ai/zen/go/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new chat");
        assert_eq!(sdk_client.variant(), OpenAIVariant::Chat);
        assert_eq!(sdk_client.sdk_type(), "openai_compatible");
    }

    /// `/responses` → `OpenAIVariant::Responses`.
    #[test]
    fn variant_picked_from_url_responses() {
        let sdk_client = OpenAIClient::new(
            &responses_cfg("https://opencode.ai/zen/go/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new responses");
        assert_eq!(sdk_client.variant(), OpenAIVariant::Responses);
        assert_eq!(sdk_client.sdk_type(), "openai");
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
    /// request that mirrors the `LlmRequest`. Pins the D8 invariant
    /// post-CAP-removal: no clamp chain, the wire body carries the
    /// caller's value verbatim.
    #[test]
    fn body_sha256_matches_send_wire_body_chat() {
        let sdk_client = OpenAIClient::new(
            &chat_cfg("http://localhost/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new chat");
        let req = llm_req("hello world");
        let legacy_req = legacy_req_for(&req);
        let wire_body = build_chat_request_body(&sdk_client.model, &legacy_req);
        let bytes = serde_json::to_vec(&wire_body).expect("build_chat_request_body serialises");
        let expected = sha256_hex(&bytes);
        let got = sdk_client.body_sha256(&req).expect("body_sha256");
        assert_eq!(
            got, expected,
            "chat body_sha256 must equal sha256(wire body)"
        );
        assert_eq!(got.len(), 64, "SHA-256 hex is 64 lowercase chars");
        assert!(got.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Responses variant: `body_sha256` matches the SHA-256 of the
    /// wire body `build_responses_body` produces. Pins the D8
    /// invariant post-CAP-removal: no clamp chain, the wire body
    /// carries the caller's value verbatim (with the `omit_max_tokens`
    /// translation the field-level flag controls).
    #[test]
    fn body_sha256_matches_send_wire_body_responses() {
        let sdk_client = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new responses");
        let req = llm_req("hello responses");
        let legacy_req = legacy_req_for(&req);
        let wire_body = build_responses_body(
            &legacy_req,
            &sdk_client.model,
            false,
            sdk_client.omit_max_tokens,
        );
        let bytes = serde_json::to_vec(&wire_body).expect("build_responses_body serialises");
        let expected = sha256_hex(&bytes);
        let got = sdk_client.body_sha256(&req).expect("body_sha256");
        assert_eq!(
            got, expected,
            "responses body_sha256 must equal sha256(wire body)"
        );
        assert_eq!(got.len(), 64);
    }

    /// Phase 2 (CAP removal): the chat variant does NOT clamp
    /// `max_tokens`. The caller's value flows through verbatim
    /// (the cascade auto-heal handles upstream rejection). Pins
    /// the new contract: the wire body carries `max_tokens = 1_000_000`
    /// literally, no kind cap.
    #[test]
    fn body_sha256_does_not_clamp_max_tokens_chat() {
        let sdk_client = OpenAIClient::new(
            &chat_cfg("https://api.deepseek.com/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new deepseek");
        let mut req = llm_req("clamp me");
        req.max_tokens = Some(1_000_000);
        let legacy_req = legacy_req_for(&req);
        let wire_body = build_chat_request_body(&sdk_client.model, &legacy_req);
        let json: serde_json::Value = serde_json::to_value(&wire_body).expect("body serialises");
        assert_eq!(
            json.get("max_tokens"),
            Some(&serde_json::json!(1_000_000)),
            "wire body must carry the caller's exact value (no clamp); got: {json}"
        );
        let expected = sha256_hex(&serde_json::to_vec(&wire_body).expect("vec"));
        let got = sdk_client.body_sha256(&req).expect("body_sha256");
        assert_eq!(got, expected, "SHA must mirror the wire body verbatim");
    }

    /// `body_sha256` honours the `omit_max_tokens` field on the
    /// Responses variant: when the operator pinned the flag, the
    /// wire body drops `max_tokens` regardless of the request's
    /// value. The SHA hashes the body that omits the field.
    #[test]
    fn body_sha256_drops_omitted_params() {
        let sdk_client = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("OpenAIClient::new responses")
        .with_omit_max_tokens(true);
        assert!(sdk_client.omit_max_tokens);

        let mut req = llm_req("drop max_tokens");
        req.max_tokens = Some(2048);
        let legacy_req = legacy_req_for(&req);
        let wire_body = build_responses_body(
            &legacy_req,
            &sdk_client.model,
            false,
            sdk_client.omit_max_tokens,
        );
        let json: serde_json::Value = serde_json::to_value(&wire_body).expect("body serialises");
        assert!(
            json.get("max_tokens").is_none(),
            "wire body must omit max_tokens when omit_max_tokens=true, got: {json}"
        );
        let expected = sha256_hex(&serde_json::to_vec(&wire_body).expect("vec"));
        let got = sdk_client.body_sha256(&req).expect("body_sha256");
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
        let cap = LlmClient::capabilities(&chat);
        assert_eq!(cap.wire_format_id(), "openai_compatible");
        assert!(cap.prefers_openai_wire);
        assert!(!cap.prefers_responses_wire);
        let _inner: &ProviderCapabilities = &cap.0;

        let responses = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("responses");
        let cap = LlmClient::capabilities(&responses);
        assert_eq!(cap.wire_format_id(), "openai");
        assert!(cap.prefers_responses_wire);
        assert!(!cap.prefers_openai_wire);
    }

    /// `max_tokens_probe_ceiling` returns the post-CAP-removal
    /// constant: every variant now reports the global
    /// `MAX_AUTOPROBE_CEILING` so the auto-probe walks the full
    /// `2^1..2^30` exponential phase against the upstream.
    #[test]
    fn max_tokens_probe_ceiling_uses_global_cap() {
        let chat = OpenAIClient::new(
            &chat_cfg("http://localhost/v1/chat/completions"),
            SecretString::new("dummy".into()),
        )
        .expect("chat");
        assert_eq!(
            LlmClient::max_tokens_probe_ceiling(&chat),
            crate::llm::probe::MAX_AUTOPROBE_CEILING
        );
        let responses = OpenAIClient::new(
            &responses_cfg("http://localhost/v1/responses"),
            SecretString::new("dummy".into()),
        )
        .expect("responses");
        assert_eq!(
            LlmClient::max_tokens_probe_ceiling(&responses),
            crate::llm::probe::MAX_AUTOPROBE_CEILING
        );
    }

    /// `from_resolved` derives the variant from the dispatcher's
    /// `wire_format` field. The DeepSeek-specific kind cap that
    /// the legacy `from_resolved` wired is gone (post-CAP-removal);
    /// the chat variant now reports the global probe ceiling.
    #[test]
    fn from_resolved_wires_global_cap_for_deepseek() {
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
            wire_format: crate::llm::client::WireFormatId::OpenAICompatible,
            omit_max_tokens: false,
        };
        let sdk_client = OpenAIClient::from_resolved(&resolved).expect("from_resolved");
        assert_eq!(sdk_client.variant(), OpenAIVariant::Chat);
        // Post-CAP-removal: `from_resolved` does NOT wire a kind cap
        // for DeepSeek anymore. The auto-probe walks the global
        // ceiling and the cascade auto-heal drops `max_tokens` on
        // upstream rejection.
        assert_eq!(
            LlmClient::max_tokens_probe_ceiling(&sdk_client),
            crate::llm::probe::MAX_AUTOPROBE_CEILING
        );
    }

    /// `clone_for_wire` is lossless for every wire-side field,
    /// including all of `extra_messages` / `attachments` /
    /// `tool_choice`. Pins the wire-body contract the SDK
    /// dispatcher relies on. Mirrors the equivalent test on
    /// `AnthropicClient`.
    #[test]
    fn clone_for_wire_is_lossless() {
        use crate::llm::client::{Attachment, Message, ToolChoice};
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
        let wire = clone_for_wire(&req);
        assert_eq!(wire.role, Role::Sketch);
        assert_eq!(wire.model, "m");
        assert_eq!(wire.system, "s");
        assert_eq!(wire.user, "u");
        assert_eq!(wire.max_tokens, Some(16));
        assert_eq!(wire.temperature, Some(0.5));
        assert_eq!(wire.top_p, Some(0.9));
        // The OpenAI-compat wire bodies do not advertise `top_k`;
        // the field carries through but the body builders ignore it.
        assert_eq!(wire.response_schema, req.response_schema);
        assert!(wire.stream);
        assert_eq!(wire.extra_messages.len(), 1);
        assert_eq!(wire.extra_messages[0].role, "assistant");
        assert_eq!(wire.extra_messages[0].content, "{");
        assert_eq!(wire.attachments.len(), 1);
        assert_eq!(wire.attachments[0].mime, "image/png");
        assert_eq!(wire.attachments[0].modality, "image");
        assert_eq!(wire.attachments[0].data, vec![0x89, 0x50, 0x4e, 0x47]);
        assert!(matches!(wire.tool_choice, Some(ToolChoice::Required)));
    }
}
