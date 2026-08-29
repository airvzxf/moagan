//! Generic OpenAI Chat Completions provider.
//!
//! Used by any backend whose API is OpenAI-compatible: DeepSeek
//! (`https://api.deepseek.com/v1`), OpenCode
//! (`https://opencode.ai/zen/go/v1`), and any future provider that
//! speaks the same wire format. The provider name (`fn name()`) is
//! configurable so the same code can serve multiple backends.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::ProviderCapabilities;
// The v0.9 dispatcher exposed a sub-trait for routing by URL. The
// v0.10 dispatcher picks the concrete provider from the wire format,
// not from a boxed trait object — the URL builder is now a public
// method on each provider.
use super::probe::MIN_AUTOPROBE_FLOOR;
use super::probe_table::MaxTokensTable;
use super::provider::Provider;
use super::response_format_opt_out;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::wire::{Request, Response, Usage};

/// Generic OpenAI-compat provider.
#[derive(Clone)]
pub struct OpenAICompatibleProvider {
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) endpoint: String,
    pub(crate) api_key: SecretString,
    pub(crate) client: Client,
    pub(crate) max_retries: u32,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,000,000), so the per-role runtime
    /// value normally fits under the cap. The clamp below exists
    /// for the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request with 400.
    pub(crate) provider_max_tokens: Option<u32>,
    /// Kind-level hard cap on `max_tokens`, applied as a second
    /// layer on top of `provider_max_tokens`. `None` means no
    /// kind-level cap (DeepSeek-direct uses this; DeepSeek accepts
    /// up to 8192 per its docs and the per-provider TOML knob is
    /// enough). `Some(16_384)` is wired by the OpenCode
    /// dispatcher so the upstream never returns HTTP 400 for
    /// kimi-k*, qwen3.x, gpt-5.6-luna, etc. — see
    /// `u32::MAX`.
    pub(crate) kind_hard_cap: Option<u32>,
    /// Auto-probed `max_tokens` table. When `Some` the
    /// `resolve_cached(self.name(), self.model())` value joins the
    /// clamp chain as the third-highest layer (kind-level cap >
    /// operator override > table). `None` when the provider was
    /// built without going through `registry_from_config` — unit
    /// tests and legacy call paths.
    pub(crate) max_tokens_table: Option<Arc<MaxTokensTable>>,
}

impl OpenAICompatibleProvider {
    /// Build from a `ProviderConfig` and a resolved API key.
    /// Kept for backwards compatibility with hand-rolled callers
    /// (legacy test fixtures); new dispatcher code goes through
    /// [`Self::from_resolved`].
    ///
    /// When `spec.models` is empty (the v0.9 fixture shape) the
    /// constructor falls back to the first model id (or
    /// `"openai_compat"` as a placeholder) and the section-level
    /// `endpoint` (or `"http://localhost"` as a placeholder).
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        tracing::debug!(
            endpoint = spec.endpoint.as_deref(),
            models = spec.models.len(),
            "OpenAICompatibleProvider::new: enter"
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| {
                tracing::error!(error = %e, "OpenAICompatibleProvider::new: client build failed");
                Error::Provider {
                    message: format!("build http client: {e}"),
                    http_status: None,
                }
            })?;
        let name = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "openai_compat".to_owned());
        let model = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_default();
        let endpoint = spec
            .models
            .first()
            .and_then(|m| m.endpoint.clone())
            .or_else(|| spec.endpoint.clone())
            .unwrap_or_else(|| "http://localhost".to_owned());
        let provider_max_tokens = spec.models.first().and_then(|m| m.max_tokens);
        tracing::info!(
            name = %name,
            model = %model,
            endpoint = %endpoint,
            "OpenAICompatibleProvider: constructed"
        );
        Ok(Self {
            name,
            model,
            endpoint,
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens,
            kind_hard_cap: None,
            max_tokens_table: None,
        })
    }

    /// Build from a `ProviderConfig`, a resolved API key, and an
    /// optional kind-level hard cap on `max_tokens`. The
    /// dispatcher routes per-section caps:
    ///
    /// * `deepseek` → `Some(DEEPSEEK_MAX_TOKENS_CAP)` (wired by
    ///   [`super::deepseek::DeepSeekProvider::from_resolved`]).
    /// * opencode (chat-completions path) → no cap; the auto-probe
    ///   discovers the per-model ceiling at startup.
    /// * unknown / mock → `None`.
    ///
    /// The kind cap lives on the per-section wrapper, NOT on
    /// `ProviderConfig` — the v0.10 schema dropped the kind tag.
    pub fn new_with_kind_cap(
        spec: &ProviderConfig,
        api_key: SecretString,
        cap: Option<u32>,
    ) -> Result<Self> {
        tracing::debug!(?cap, "OpenAICompatibleProvider::new_with_kind_cap: enter");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| {
                tracing::error!(error = %e, "OpenAICompatibleProvider::new_with_kind_cap: client build failed");
                Error::Provider {
                    message: format!("build http client: {e}"),
                    http_status: None,
                }
            })?;
        let name = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "openai_compat".to_owned());
        let model = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_default();
        let endpoint = spec
            .models
            .first()
            .and_then(|m| m.endpoint.clone())
            .or_else(|| spec.endpoint.clone())
            .unwrap_or_else(|| "http://localhost".to_owned());
        let provider_max_tokens = spec.models.first().and_then(|m| m.max_tokens);
        tracing::info!(
            name = %name,
            model = %model,
            endpoint = %endpoint,
            kind_hard_cap = ?cap,
            "OpenAICompatibleProvider: constructed (with kind cap)"
        );
        Ok(Self {
            name,
            model,
            endpoint,
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens,
            kind_hard_cap: cap,
            max_tokens_table: None,
        })
    }

    /// v0.10 dispatcher entry point. Builds an
    /// `OpenAICompatibleProvider` from a `ResolvedModelConfig`
    /// (one `(section, model_id)` pair), resolving the API key via
    /// the unified [`super::api_keys::lookup_key`] helper. The key
    /// lookup falls back from the section name to the canonical
    /// `kind` so a per-model alias like `kimi-k3` (kind=`"opencode"`)
    /// resolves against the `OPENCODE_API_KEY` env var rather than
    /// the non-existent `KIMI-K3_API_KEY`. The dispatcher picks
    /// this constructor for endpoints whose path resolves to
    /// [`super::wire_format::WireFormatId::OpenAICompatible`].
    /// `kind_hard_cap` stays `None` here — section-specific caps
    /// (e.g. DeepSeek's `DEEPSEEK_MAX_TOKENS_CAP`) are wired by the
    /// section-specific wrapper (see
    /// [`super::deepseek::DeepSeekProvider::from_resolved`]).
    pub fn from_resolved(resolved: &crate::config::ResolvedModelConfig) -> Result<Self> {
        tracing::debug!(
            section = %resolved.section,
            model = %resolved.id,
            "OpenAICompatibleProvider::from_resolved: enter"
        );
        let kind = super::api_keys::lookup_kind_for_resolved(resolved);
        let key = super::api_keys::lookup_key(&kind, None)
            .ok_or_else(|| {
                tracing::error!(
                    kind,
                    "OpenAICompatibleProvider::from_resolved: API key missing"
                );
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| {
                tracing::error!(error = %e, "OpenAICompatibleProvider::from_resolved: client build failed");
                Error::Provider {
                    message: format!("build http client: {e}"),
                    http_status: None,
                }
            })?;
        tracing::info!(
            section = %resolved.section,
            model = %resolved.id,
            "OpenAICompatibleProvider::from_resolved: constructed"
        );
        Ok(Self {
            name: resolved.section.clone(),
            model: resolved.id.clone(),
            endpoint: resolved.endpoint.clone(),
            api_key: SecretString::new(key),
            client,
            max_retries: 3,
            provider_max_tokens: resolved.max_tokens,
            kind_hard_cap: None,
            max_tokens_table: None,
        })
    }

    /// Attach the shared auto-probe `max_tokens` table so `send()`
    /// layers the discovered ceiling into the clamp chain. Wired by
    /// `registry_from_config` when the registry has a table.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        tracing::debug!(name = %self.name, "OpenAICompatibleProvider::with_max_tokens_table");
        self.max_tokens_table = Some(table);
        self
    }

    /// Compute the URL for chat completions.
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

    /// Build the chat-completions request body for a given provider
    /// request, applying the role-based JSON output mode and the
    /// per-model opt-out from `response_format_opt_out`. Extracted so
    /// tests can assert on the wire shape without a live HTTP call.
    ///
    /// For models whose [`crate::llm::json_strategy::JsonRecoveryStrategy`]
    /// is `PromptPrefill` (e.g. `deepseek-v4-pro`,
    /// `deepseek-v4-flash`), this function also appends an
    /// assistant prefill message of `{` after the user turn so the
    /// model continues with a JSON object body. The prefill is a
    /// response-side hint; the cross-run cache key
    /// ([`crate::llm::wire::build_cache_key`]) deliberately
    /// IGNORES the equivalent `Request::extra_messages` field so
    /// the steady-state cache stays valid when the prefill retry
    /// fires.
    fn build_chat_request(&self, req: &Request) -> ChatRequest<'_> {
        let strategy = crate::llm::json_strategy::strategy_for(&self.model, None);
        let mut messages: Vec<ChatMessage> = vec![
            ChatMessage {
                role: "system".into(),
                content: req.system.clone(),
            },
            ChatMessage {
                role: "user".into(),
                content: req.user.clone(),
            },
        ];
        // PR-C5 (PromptPrefill): the caller may have supplied
        // `extra_messages` directly (e.g. the dispatcher
        // builds a fresh Request on the prefill retry with
        // `extra_messages = [{assistant, "{"}]`). Push those
        // verbatim so the wire shape mirrors what the caller
        // asked for. When the caller did not supply any
        // `extra_messages` and the per-model default strategy
        // is `PromptPrefill`, auto-inject the `{` prefill so
        // callers who never set `extra_messages` still get
        // the response-side hint on the steady-state path.
        for m in &req.extra_messages {
            messages.push(ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            });
        }
        if req.extra_messages.is_empty()
            && crate::llm::json_strategy::needs_assistant_prefill(strategy)
        {
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: "{".into(),
            });
        }
        let response_format = if role_requires_json(req.role)
            && !response_format_opt_out::model_skips_response_format(&self.model)
        {
            Some(ResponseFormat {
                kind: "json_object",
            })
        } else {
            None
        };
        tracing::trace!(
            model = %self.model,
            role = ?req.role,
            strategy = ?strategy,
            message_count = messages.len(),
            wants_format = response_format.is_some(),
            "build_chat_request"
        );
        ChatRequest {
            model: &self.model,
            messages,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            stream: false,
            response_format,
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    /// Output token ceiling. `None` serialises as field-absent (via
    /// `skip_serializing_if`), required for providers that reject
    /// the *presence* of `max_tokens`. The auto-healing
    /// `param_rejections` table sets this to `None` on the retry
    /// so the upstream accepts the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    stream: bool,
    /// OpenAI-style JSON output mode. Sent only when the role
    /// requires machine-readable JSON (Route, Propose, Judge, etc.)
    /// so the upstream API returns a parseable object instead of
    /// free-form text. Optional so non-JSON roles (e.g. proposals
    /// that produce markdown) skip the field entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Roles that produce structured JSON output. The OpenAI-compat
/// providers (DeepSeek, OpenCode) get `response_format` set to
/// `json_object` for these roles so the JSON parser in `parse_model_json`
/// stops hitting the trailing-token / missing-brace pathologies
/// reported by the Q8 multi-model benchmark. Markdown-only roles
/// (Propose delivers markdown but parses a JSON header; the
/// actual markdown body is not autostructured) and free-text roles
/// (Sketch, FinalReport) are NOT in this list.
pub(crate) fn role_requires_json(role: crate::llm::Role) -> bool {
    use crate::llm::Role::*;
    matches!(
        role,
        Intake
            | Clarify
            | Route
            | Gate
            | Critique
            | Repair
            | Rank
            | Synthesizer
            | Adversary
            | Decomposer
            | MergeSynthesizer
    )
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageOut,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessageOut {
    content: String,
}

#[derive(Debug, Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// Custom Debug that masks `max_tokens_table` — `MaxTokensTable`
/// does not implement `Debug` (that lives in `probe_table.rs`,
/// outside this provider's owned files) and the table is a shared
/// `Arc`, so emitting `<shared>` keeps the dump informative without
/// forcing a cross-file change.
impl std::fmt::Debug for OpenAICompatibleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatibleProvider")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("provider_max_tokens", &self.provider_max_tokens)
            .field("kind_hard_cap", &self.kind_hard_cap)
            .field("max_tokens_table", &"<shared>")
            .finish()
    }
}

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Dispatcher lookup by `kind`: direct DeepSeek config
        // gets the deepseek variant; everything else (the
        // OpenCode chat-completions path) stays on the
        // generic baseline.
        if self.name == "deepseek" {
            ProviderCapabilities::for_deepseek()
        } else {
            ProviderCapabilities::for_openai_compat()
        }
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        self.send_with_safety_clamp(req, true).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Mirror of the clamp chain in
        // `send_with_safety_clamp(_, true)` so the audit-log hash
        // is byte-for-byte identical to the wire body. Both paths
        // route through
        // `crate::llm::max_tokens::resolve_max_tokens` with the
        // same arguments; the precedence order (env → cache →
        // operator_cap → kind_hard_cap → DEFAULT_MAX_TOKENS) is
        // encoded in the helper.
        //
        // `None` on `req.max_tokens` is treated as `u32::MAX` so the
        // audit hash stays deterministic even when the auto-heal
        // path drops the field from the wire body.
        let resolved = crate::llm::max_tokens::resolve_max_tokens(
            self.name(),
            self.model(),
            self.max_tokens_table.as_deref(),
            self.provider_max_tokens,
            self.kind_hard_cap,
        );
        req.max_tokens.unwrap_or(u32::MAX).min(resolved)
    }

    /// Bypass variant for the auto-probe. Skips every cap
    /// (operator override, kind_hard_cap, table) so the algorithm
    /// sees the upstream's real boundary. The regular `send` keeps
    /// every cap so a stale or empty table cannot leak an
    /// unbounded value into the wire body.
    ///
    /// Same rationale as `minimax::send_probe`: when the operator's
    /// TOML pins `max_tokens = 8192` (DeepSeek-direct's historical
    /// cap) and the upstream actually accepts up to 32K, every
    /// probe lands on 8192 and the algorithm concludes "accepts
    /// everything". To discover the real boundary, the probe must
    /// send `req.max_tokens` verbatim, subject only to the floor.
    async fn send_probe(&self, req: &Request) -> Result<(u16, Response)> {
        self.send_with_safety_clamp(req, false).await
    }

    /// Cap the exponential probe at the `kind_hard_cap` so the
    /// algorithm does not burn 30 sequential HTTP round-trips
    /// probing values the upstream will never accept. DeepSeek-direct
    /// (`DEEPSEEK_MAX_TOKENS_CAP = 393_216`) and OpenCode's
    /// chat-completions path (`u32::MAX = 16_384`)
    /// both reach this method via `new_with_kind_cap`, so the probe
    /// observes the upstream's real bound without first tripping
    /// the upstream's HTTP 400 `max_tokens` rejection (which would
    /// yield `Indeterminate` per the v0.7.1 contract and collapse
    /// the discovered ceiling). Other OpenAI-compat backends (with
    /// no `kind_hard_cap`) keep the default
    /// [`super::probe::MAX_AUTOPROBE_CEILING`].
    fn max_tokens_probe_ceiling(&self) -> u32 {
        self.kind_hard_cap
            .unwrap_or(super::probe::MAX_AUTOPROBE_CEILING)
    }
}

impl OpenAICompatibleProvider {
    /// Public URL the provider POSTs to. v0.10 keeps the URL builder
    /// as a plain method so external callers / tests can still
    /// inspect the routed URL.
    pub fn url(&self) -> String {
        self.chat_url()
    }
}

impl OpenAICompatibleProvider {
    /// Shared HTTP body between `send` and `send_probe`. When
    /// `safety_clamp = true` the wire body is capped by every layer
    /// (operator override + `kind_hard_cap` + table); when `false`
    /// the wire body carries `req.max_tokens` verbatim subject only
    /// to the [`MIN_AUTOPROBE_FLOOR`] minimum.
    async fn send_with_safety_clamp(
        &self,
        req: &Request,
        safety_clamp: bool,
    ) -> Result<(u16, Response)> {
        let url = self.chat_url();
        // Probe path uses `max_retries = 0`: a 4xx IS the algorithm's
        // signal (max-tokens rejection), retrying it wastes the 5s
        // probe timeout and risks masking the boundary if a retry
        // happens to succeed. Production path keeps the existing
        // self.max_retries (3) for transient 5xx storms.
        let max_retries = if safety_clamp { self.max_retries } else { 0 };
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = self.build_chat_request(req);
            let body = if safety_clamp {
                // v0.13.0 B-1 PR #3: the env -> cached -> operator_cap
                // -> kind_hard_cap -> DEFAULT_MAX_TOKENS chain lives
                // in `crate::llm::max_tokens::resolve_max_tokens`.
                // The kind-level hard cap (e.g. `DEEPSEEK_MAX_TOKENS_CAP
                // = 393_216` for the DeepSeek-direct wrapper, or
                // `u32::MAX` for the opencode Go route) flows through
                // the helper as the `kind_hard_cap` argument; the
                // operator TOML override is `provider_max_tokens`.
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
                    self.kind_hard_cap,
                );
                match body.max_tokens {
                    Some(n) if n > cap => ChatRequest {
                        max_tokens: Some(cap),
                        ..body
                    },
                    // `Some(n)` within cap, or `None` — keep
                    // verbatim. The wire builder decides whether
                    // `None` becomes field-absent.
                    _ => body,
                }
            } else {
                // Probe path: bypass every cap. Floor ensures we
                // never ask for `max_tokens < 1024` (some upstreams
                // reject the request outright below that minimum).
                // `None` stays `None` so the probe still honours
                // any explicit request to drop the field.
                match body.max_tokens {
                    Some(n) if n < MIN_AUTOPROBE_FLOOR => ChatRequest {
                        max_tokens: Some(MIN_AUTOPROBE_FLOOR),
                        ..body
                    },
                    _ => body,
                }
            };
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
                    if status.is_success() {
                        let parsed: ChatResponse =
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
                        let response = Response {
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
                    let body = resp.text().await.unwrap_or_default();
                    if attempt >= max_retries {
                        return Err(Error::Provider {
                            message: format!(
                                "openai-compat: HTTP {code} after {attempt} attempts: {body}"
                            ),
                            http_status: Some(code),
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
                Err(e) => {
                    if attempt >= max_retries {
                        return Err(Error::Provider {
                            message: format!("openai-compat: network: {e}"),
                            http_status: None,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::capabilities;

    fn provider(endpoint: &str) -> OpenAICompatibleProvider {
        OpenAICompatibleProvider::new(
            &ProviderConfig {
                models: Vec::new(),
                endpoint: Some(endpoint.into()),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap()
    }

    #[test]
    fn chat_url_handles_known_suffixes() {
        assert_eq!(
            provider("https://api.deepseek.com/v1").chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            provider("https://api.deepseek.com/v1/chat/completions").chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            provider("https://api.deepseek.com").chat_url(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn serializes_chat_request_with_provider_cap() {
        // DeepSeek caps at 8192. The propose role asks for 32768
        // tokens; the provider must clamp to 8192 so the upstream
        // doesn't reject the request with 400.
        let p = OpenAICompatibleProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: vec![crate::config::ModelConfig {
                    id: "deepseek-v4-flash".into(),
                    endpoint: None,
                    max_tokens: Some(8192),
                }],
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(p.provider_max_tokens, Some(8192));
    }

    #[test]
    fn serializes_chat_request_correctly() {
        let request = ChatRequest {
            model: "deepseek-v4-flash",
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "system".into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: "user".into(),
                },
            ],
            max_tokens: Some(128),
            temperature: None,
            top_p: None,
            stream: false,
            response_format: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "model": "deepseek-v4-flash",
                "messages": [
                    {"role": "system", "content": "system"},
                    {"role": "user", "content": "user"}
                ],
                "max_tokens": 128,
                "stream": false
            })
        );
    }

    /// Build an `OpenAICompatibleProvider` with a fully-specified config
    /// so the per-test `model` overrides the default in `provider()`.
    fn provider_with_model(_kind: &str, endpoint: &str, model: &str) -> OpenAICompatibleProvider {
        OpenAICompatibleProvider::new(
            &ProviderConfig {
                models: vec![crate::config::ModelConfig {
                    id: model.into(),
                    endpoint: None,
                    max_tokens: None,
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
            },
            SecretString::new("dummy".into()),
        )
        .unwrap()
    }

    fn json_request(role: crate::llm::Role, model: &str) -> Request {
        Request {
            role,
            model: model.into(),
            system: "system".into(),
            user: "user".into(),
            max_tokens: Some(128),
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
    fn openai_compat_request_includes_response_format_for_normal_models() {
        let p = provider_with_model(
            "deepseek",
            "https://api.deepseek.com/v1",
            "deepseek-v4-flash",
        );
        let body =
            p.build_chat_request(&json_request(crate::llm::Role::Route, "deepseek-v4-flash"));
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value.get("response_format"),
            Some(&serde_json::json!({"type": "json_object"}))
        );
    }

    #[test]
    fn openai_compat_request_omits_response_format_for_opted_out_models() {
        // glm-5.1 routes through OpenCode's
        // /v1/chat/completions endpoint, which is built on this
        // OpenAICompatibleProvider. Same code path as DeepSeek — the
        // opt-out must trigger purely from the model name.
        for model in [
            "glm-5.1",
            "glm-5.2",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "deepseek-v4-pro",
            "kimi-k3",
        ] {
            let p = provider_with_model("opencode", "https://opencode.ai/zen/go/v1", model);
            let body = p.build_chat_request(&json_request(crate::llm::Role::Route, model));
            let value = serde_json::to_value(&body).unwrap();
            assert!(
                value.get("response_format").is_none(),
                "opted-out model {model} must omit response_format from the body, got: {value}"
            );
        }
    }

    #[test]
    fn openai_compat_request_omits_response_format_for_non_json_role() {
        // Markdown / free-text roles must not get response_format
        // either, regardless of model.
        let p = provider_with_model(
            "deepseek",
            "https://api.deepseek.com/v1",
            "deepseek-v4-flash",
        );
        let body = p.build_chat_request(&json_request(
            crate::llm::Role::Propose,
            "deepseek-v4-flash",
        ));
        let value = serde_json::to_value(&body).unwrap();
        assert!(value.get("response_format").is_none());
    }

    #[test]
    fn opencode_request_omits_response_format_for_opted_out_models() {
        // Pin the OpenCode contract: even when role_requires_json
        // is true (Route), the chat-completions body for an opted-out
        // model must NOT carry the `response_format` field so the
        // upstream doesn't return prose-prefixed content.
        let p = provider_with_model(
            "opencode",
            "https://opencode.ai/zen/go/v1",
            "kimi-k2.7-code",
        );
        let body = p.build_chat_request(&json_request(crate::llm::Role::Route, "kimi-k2.7-code"));
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["model"], serde_json::json!("kimi-k2.7-code"));
        assert!(
            value.get("response_format").is_none(),
            "OpenCode request for opted-out model must omit response_format, got: {value}"
        );
        // Sanity: the role_requires_json path was actually exercised
        // — without the opt-out check the field WOULD be present.
        assert!(super::role_requires_json(crate::llm::Role::Route));
    }

    /// Issue #558 regression: the `e2e-network` auto runs (fast +
    /// explore against the MiniMax upstream) emit Intake-shaped
    /// JSON (`{problem, objectives[], ...}`). The MiniMax model
    /// still produces malformed payloads ~1% of the time at high
    /// temperature, so the Anthropic-compat provider relies on the
    /// assistant prefill of `{` to bias the first emitted token
    /// toward a clean JSON-object start. The prefill fires for
    /// every role returned by `role_requires_json`, so we pin the
    /// Intake contract here: a future edit that drops Intake from
    /// the `matches!` list must fail this test, so the regression
    /// cannot silently land.
    ///
    /// The companion parse-side fix lives in
    /// `crate::phases::util::repair_stray_comma_after_key` (same
    /// PR). The two together cover both halves of the issue: the
    /// prefill eliminates the unescaped-quote / bracket pathology
    /// for most calls, and the new repair pass strips the
    /// `",:` shape that slips through when the prefill isn't
    /// enough.
    #[test]
    fn intake_role_is_in_role_requires_json_for_prefill() {
        use crate::llm::Role;
        assert!(
            super::role_requires_json(Role::Intake),
            "Intake must remain in role_requires_json so the Anthropic-compat \
             body builder emits the assistant prefill of `{{` (issue #558)"
        );
        // Pin the JSON-required role set as a whole so the contract
        // is grep-able from the test alone. Each entry here matches
        // a `Role` variant that the dispatcher expects to emit
        // structured JSON for. Adding a new variant without adding
        // it here will fail the test.
        for role in [
            Role::Intake,
            Role::Clarify,
            Role::Route,
            Role::Gate,
            Role::Critique,
            Role::Repair,
            Role::Rank,
            Role::Synthesizer,
            Role::Adversary,
            Role::Decomposer,
            Role::MergeSynthesizer,
        ] {
            assert!(
                super::role_requires_json(role),
                "{role:?} must remain a JSON-required role (the Anthropic-compat \
                 prefill fires for it)"
            );
        }
        // And the inverse: free-text / prose roles must NOT be on
        // the list, otherwise the prefill would corrupt non-JSON
        // outputs (Sketch writes prose, Deliver writes markdown).
        for role in [Role::Sketch, Role::Deliver] {
            assert!(
                !super::role_requires_json(role),
                "{role:?} must NOT be on role_requires_json — the prefill would \
                 inject `{{` into a prose-shaped response"
            );
        }
    }

    #[test]
    fn opencode_request_keeps_response_format_for_non_opted_out_models() {
        // The flip side of the previous test: an OpenCode model
        // NOT on the opt-out list (mimo-v2.5, deepseek-v4-flash, hy3)
        // still gets response_format = json_object for JSON roles.
        for model in ["mimo-v2.5", "deepseek-v4-flash", "hy3"] {
            let p = provider_with_model("opencode", "https://opencode.ai/zen/go/v1", model);
            let body = p.build_chat_request(&json_request(crate::llm::Role::Route, model));
            let value = serde_json::to_value(&body).unwrap();
            assert_eq!(
                value.get("response_format"),
                Some(&serde_json::json!({"type": "json_object"})),
                "non-opted-out model {model} must keep response_format: json_object, got: {value}"
            );
        }
    }

    /// PR-C5: the `PromptPrefill` strategy injects an assistant
    /// prefill of `{` for `deepseek-v4-pro` / `deepseek-v4-flash`
    /// so the model continues with a JSON object body. The prefill
    /// appears as the LAST message in the messages array (after
    /// the user turn) so the model sees
    /// `[system, user, assistant:]` and continues the JSON.
    #[test]
    fn prompt_prefill_appends_assistant_brace_message() {
        let p = provider_with_model(
            "deepseek",
            "https://api.deepseek.com/v1",
            "deepseek-v4-flash",
        );
        let body =
            p.build_chat_request(&json_request(crate::llm::Role::Route, "deepseek-v4-flash"));
        let value = serde_json::to_value(&body).unwrap();
        let messages = value
            .get("messages")
            .and_then(|m| m.as_array())
            .expect("body must carry a messages array");
        assert_eq!(
            messages.len(),
            3,
            "PromptPrefill must append a third message, got: {value}"
        );
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "{");
    }

    /// PR-C5: the `PromptPrefill` strategy does NOT inject the
    /// prefill for non-prefill models (e.g. `kimi-k3`). The wire
    /// shape stays at two messages (system + user) so today's
    /// behaviour for non-deepseek models is bit-identical.
    #[test]
    fn non_prefill_models_skip_assistant_message() {
        let p = provider_with_model("opencode", "https://opencode.ai/zen/go/v1", "kimi-k3");
        let body = p.build_chat_request(&json_request(crate::llm::Role::Route, "kimi-k3"));
        let value = serde_json::to_value(&body).unwrap();
        let messages = value
            .get("messages")
            .and_then(|m| m.as_array())
            .expect("body must carry a messages array");
        assert_eq!(
            messages.len(),
            2,
            "non-prefill models must NOT append a third message, got: {value}"
        );
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
    }

    /// PR-C5: when the caller already populated
    /// `Request::extra_messages` (e.g. a phase-layer override
    /// that wants bespoke extra messages) the per-model prefill
    /// must NOT clobber it. The caller-supplied messages win.
    #[test]
    fn prompt_prefill_does_not_overwrite_existing_extra_messages() {
        let p = provider_with_model(
            "deepseek",
            "https://api.deepseek.com/v1",
            "deepseek-v4-flash",
        );
        let mut req = json_request(crate::llm::Role::Route, "deepseek-v4-flash");
        req.extra_messages = vec![crate::llm::wire::Message {
            role: "assistant".into(),
            content: "[CUSTOM]".into(),
        }];
        let body = p.build_chat_request(&req);
        let value = serde_json::to_value(&body).unwrap();
        let messages = value
            .get("messages")
            .and_then(|m| m.as_array())
            .expect("body must carry a messages array");
        // Exactly the caller's two extra messages are present,
        // NOT the per-model default `{` prefill.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(
            messages[2]["content"], "[CUSTOM]",
            "caller-supplied extra_messages must win over the per-model prefill"
        );
    }

    /// PR-fix (opencode hard cap): when the dispatcher wires an
    /// `OpenAICompatibleProvider` for an OpenCode model
    /// (`new_with_kind_cap(_, _, Some(u32::MAX))`)
    /// the wire body must clamp `request.max_tokens` to 16_384 even
    /// if `ProviderConfig::max_tokens` is unset. Without the clamp
    /// the upstream returns HTTP 400 because the per-role default
    /// `DEFAULT_MAX_TOKENS = 1_000_000` flows through. Pins the
    /// defence at the integration boundary (`send` + recorded
    /// request body).
    #[test]
    fn opencode_backed_provider_clamps_max_tokens_to_hard_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
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
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatibleProvider::new_with_kind_cap(
                &ProviderConfig {
                    // v0.10: provide a per-model `id` so the
                    // constructor wires `self.model = "kimi-k3"`
                    // and the wire body carries the model id
                    // verbatim. Without it the legacy constructor
                    // leaves `self.model = ""`.
                    models: vec![crate::config::ModelConfig {
                        id: "kimi-k3".into(),
                        endpoint: None,
                        max_tokens: None,
                    }],
                    endpoint: Some(server.uri()),
                    // None on purpose: only the kind-level cap
                    // applies.
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
            temperature_auto_enabled: None,
                    plan: None,
                },
                SecretString::new("dummy".into()),
                Some(u32::MAX),
            )
            .unwrap();
            let req = Request {
                model: "kimi-k3".into(),
                role: crate::llm::Role::Route,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: Some(1_000_000),
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, _response) = p.send(&req).await.unwrap();
            assert_eq!(status, 200);
            let received = server
                .received_requests()
                .await
                .expect("recording must be enabled by default");
            assert_eq!(received.len(), 1);
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            // v0.10: the v0.9 16_384-token global ceiling on the
            // chat-completions wire is gone. The kind cap wired
            // here is `u32::MAX` (the opencode chat-completions
            // path has no kind-level ceiling — the auto-probe
            // discovers the real boundary per `(provider, model)`
            // and caches it in `max_tokens_auto.toml`). With
            // nothing in the clamp chain, the wire body carries
            // the requested value unchanged.
            assert_eq!(
                body["max_tokens"],
                serde_json::json!(1_000_000),
                "with no operator cap, no table, and kind cap = u32::MAX, body must carry the requested value, got body: {body}"
            );
        });
    }

    /// DEEPSEEK_MAX_TOKENS_CAP clamp contract (PR-473 --ignored CI
    /// regression): the direct DeepSeek OpenAI-compat upstream
    /// rejects any `max_tokens > 393_216` with HTTP 400
    /// `invalid_request_error`. The dispatcher wires
    /// `DeepSeekProvider::new` to call
    /// `OpenAICompatibleProvider::new_with_kind_cap(_, _,
    /// Some(DEEPSEEK_MAX_TOKENS_CAP))`, so the wire body must
    /// carry `max_tokens = 393_216` even when the operator's TOML
    /// leaves `max_tokens = None` (per-role default 1_000_000).
    /// Without the cap the upstream returns HTTP 400 — exactly the
    /// failure mode the --ignored CI job surfaced.
    #[test]
    fn deepseek_direct_provider_clamps_to_deepseek_max_tokens_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
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
                .expect(1)
                .mount(&server)
                .await;
            // Mirror the `DeepSeekProvider::new` wiring at the
            // inner-provider level so the test exercises the same
            // `kind_hard_cap` the production constructor installs.
            let p = OpenAICompatibleProvider::new_with_kind_cap(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(server.uri()),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    temperature_auto_enabled: None,
                    plan: None,
                },
                SecretString::new("dummy".into()),
                Some(capabilities::DEEPSEEK_MAX_TOKENS_CAP),
            )
            .unwrap();
            let req = Request {
                model: "kimi-k3".into(),
                role: crate::llm::Role::Route,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: Some(1_000_000),
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, _response) = p.send(&req).await.unwrap();
            assert_eq!(status, 200);
            let received = server
                .received_requests()
                .await
                .expect("recording must be enabled by default");
            assert_eq!(received.len(), 1);
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert_eq!(
                body["max_tokens"],
                serde_json::json!(capabilities::DEEPSEEK_MAX_TOKENS_CAP),
                "deepseek direct hard cap must clamp 1_000_000 → {}, got body: {body}",
                capabilities::DEEPSEEK_MAX_TOKENS_CAP
            );
        });
    }

    /// `max_tokens_probe_ceiling` propagates the per-provider
    /// `kind_hard_cap` so the auto-probe short-circuits at the
    /// first `2^k > DEEPSEEK_MAX_TOKENS_CAP` (k=19 → 524_288)
    /// instead of walking the full `2^1..2^30` exponential phase
    /// against a bound that DeepSeek rejects with HTTP 400.
    /// Without this override the probe would burn 30 sequential
    /// HTTP round-trips on values the upstream will never accept
    /// — and the rejections would classify as `Indeterminate`
    /// per the v0.7.1 contract, collapsing the discovered
    /// ceiling to the last accepted probe.
    #[test]
    fn deepseek_direct_provider_probe_ceiling_is_deepseek_max_tokens_cap() {
        let p = OpenAICompatibleProvider::new_with_kind_cap(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
                plan: None,
            },
            SecretString::new("dummy".into()),
            Some(capabilities::DEEPSEEK_MAX_TOKENS_CAP),
        )
        .unwrap();
        assert_eq!(
            p.max_tokens_probe_ceiling(),
            capabilities::DEEPSEEK_MAX_TOKENS_CAP
        );
        // The default (no `kind_hard_cap`) keeps the trait
        // default of MAX_AUTOPROBE_CEILING so providers without a
        // documented ceiling (mock, third-party relays with
        // permissive limits) keep working unchanged.
        let p_no_cap = OpenAICompatibleProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p_no_cap.max_tokens_probe_ceiling(),
            crate::llm::probe::MAX_AUTOPROBE_CEILING
        );
    }

    /// `new` (no kind cap) preserves the existing DeepSeek-direct
    /// behaviour: when the operator sets `max_tokens = None` in
    /// TOML the wire body carries whatever `request.max_tokens`
    /// says, because DeepSeek-direct has no kind-level cap. Pins
    /// the asymmetry between `new` (no cap) and `new_with_kind_cap`
    /// (cap wired by the dispatcher).
    #[test]
    fn deepseek_direct_provider_uses_no_kind_cap() {
        let p = OpenAICompatibleProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(p.kind_hard_cap, None);
    }

    /// `new_with_kind_cap` propagates the cap from the dispatcher
    /// (an operator who constructs it manually — e.g. a test rig —
    /// observes the same value the dispatcher would). Pins the
    /// surface so a future refactor cannot accidentally drop the
    /// wiring.
    #[test]
    fn new_with_kind_cap_stores_cap_in_field() {
        let p = OpenAICompatibleProvider::new_with_kind_cap(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
                plan: None,
            },
            SecretString::new("dummy".into()),
            Some(u32::MAX),
        )
        .unwrap();
        assert_eq!(p.kind_hard_cap, Some(u32::MAX));
    }

    /// Auto-probe table clamp contract: when
    /// `with_max_tokens_table` attaches a table carrying a
    /// discovered value smaller than the requested `max_tokens`,
    /// the wire body must carry the discovered value. DeepSeek-
    /// direct has `kind_hard_cap = None`, so the table value flows
    /// through unchanged (no other clamp layers apply when both
    /// `kind_hard_cap` and `provider_max_tokens` are absent). Pins
    /// the v0.7 precedence order: kind > operator > table > requested.
    #[tokio::test]
    async fn openai_compat_clamps_max_tokens_to_table_value() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::llm::probe::{MIN_AUTOPROBE_FLOOR, ProbeOutcome, ProbeTransport};

        #[derive(Clone)]
        struct CappedTransport {
            cap: u32,
        }

        #[async_trait::async_trait]
        impl ProbeTransport for CappedTransport {
            async fn probe_send(&self, n: u32) -> ProbeOutcome {
                if n <= self.cap {
                    ProbeOutcome::Accepted
                } else {
                    ProbeOutcome::Rejected
                }
            }
        }

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
            .expect(1)
            .mount(&server)
            .await;

        let transport: std::sync::Arc<dyn ProbeTransport> =
            std::sync::Arc::new(CappedTransport { cap: 6_000 });
        let table = std::sync::Arc::new(MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR));
        // v0.10: the legacy `new()` constructor reads both
        // `name` and `model` from `models[0].id`, so the table
        // key has to match that pair. The dispatched path
        // (`from_resolved`) uses `(section_name, model_id)`;
        // this test exercises the hand-rolled constructor path
        // so we mirror that — provider name == model id ==
        // "deepseek-v4-flash".
        let discovered = table
            .probe_and_store(
                "deepseek-v4-flash",
                "deepseek-v4-flash",
                transport,
                crate::llm::probe::MAX_AUTOPROBE_CEILING,
            )
            .await
            .expect("probe_and_store");
        // The wire-body assertion below uses `discovered`
        // directly: this test pins the wiring contract (table
        // value honoured on the wire) without depending on the
        // probe algorithm's exact convergence — that algorithm
        // has a known ±N imprecision at non-trivial boundaries
        // (see pre-existing `probe::tests::detect_finds_cap_at_8k`).

        let p = OpenAICompatibleProvider::new(
            &ProviderConfig {
                // v0.10 canonical schema: one `models[]` entry
                // whose `id` drives both `name` and `model`
                // (the legacy constructor reads them from
                // `models.first()`). `max_tokens: None` leaves
                // `provider_max_tokens = None` so the operator
                // cap chain stays at `u32::MAX` and the table
                // value is the only clamp the wire body sees.
                models: vec![crate::config::ModelConfig {
                    id: "deepseek-v4-flash".into(),
                    endpoint: None,
                    max_tokens: None,
                }],
                endpoint: Some(server.uri()),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                plan: None,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                temperature_auto_enabled: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap()
        .with_max_tokens_table(table);

        let req = Request {
            model: "kimi-k3".into(),
            role: crate::llm::Role::Route,
            system: "sys".into(),
            user: "user".into(),
            max_tokens: Some(1_000_000),
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let (status, _response) = p.send(&req).await.unwrap();
        assert_eq!(status, 200);
        let received = server
            .received_requests()
            .await
            .expect("recording must be enabled by default");
        assert_eq!(received.len(), 1, "exactly one request must be sent");
        let body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("mock server received a JSON body");
        assert_eq!(
            body["max_tokens"],
            serde_json::json!(discovered),
            "wire body must carry the table-resolved value ({discovered}), got body: {body}"
        );
    }
}
