//! `openai_compat` provider — OpenAI Responses API at
//! `https://opencode.ai/zen/go/v1/responses`.
//!
//! This is currently the only model the operator exposes on this
//! endpoint (per the 2026-08-04 model roster):
//!
//! - `gpt-5.6-luna` (OpenAI SDK, `@ai-sdk/openai`)
//!
//! The Responses API differs from Chat Completions in two ways:
//!
//! - The request body uses `input` (a string or array of messages)
//!   instead of `messages`. We pass the user prompt as a single string.
//! - The response body is `{"output": [{"content": [{"type":
//!   "output_text", "text": "..."}]}], "usage": {...}}` instead of
//!   `{"choices": [{"message": {"content": "..."}}]}`.
//!
//! No conversation history is kept; the pipeline sends one user turn
//! at a time and the system prompt is embedded in the `instructions`
//! field (Responses API convention).

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::ProviderCapabilities;
use super::wire_format::role_requires_json;
// The legacy `OpenCodeGoDispatch` sub-trait (formerly in
// `super::opencode_go`) was the dispatcher's handle for routing by
// URL. The v0.10 dispatcher picks the concrete provider from the
// wire format, not from a boxed trait object — the URL builder is now
// a public method on each provider (e.g. `OpenAICompatProvider::url`).
use super::probe::MIN_AUTOPROBE_FLOOR;
use super::probe_table::MaxTokensTable;
use super::provider::Provider;
use super::response_format_opt_out::model_skips_response_format;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::sse_parser::{SseError, SseParser};
use super::wire::{Request, Response, Usage};

/// OpenCode Go provider routed through the OpenAI Responses API.
#[derive(Clone)]
pub struct OpenAICompatProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: reqwest::Client,
    max_retries: u32,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,000,000), so the per-role runtime
    /// value normally fits under the cap. The clamp below exists
    /// for the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request.
    provider_max_tokens: Option<u32>,
    /// When `true`, omit the `max_tokens` field from the wire body
    /// entirely. Required for upstream models that reject the
    /// *presence* of the field (e.g. `gpt-5.6-luna`). Set from
    /// `ProviderConfig::omit_max_tokens`.
    omit_max_tokens: bool,
    /// Auto-probed `max_tokens` table. When `Some` the
    /// `resolve_cached(self.name(), self.model())` value joins the
    /// clamp chain as the third-highest layer (kind-level cap >
    /// operator override > table). `None` when the provider was
    /// built without going through `registry_from_config` — unit
    /// tests and legacy call paths.
    max_tokens_table: Option<Arc<MaxTokensTable>>,
}

impl OpenAICompatProvider {
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
        let client = build_client()?;
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
        Ok(Self {
            name,
            model,
            endpoint,
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens,
            omit_max_tokens: spec.omit_max_tokens,
            max_tokens_table: None,
        })
    }

    /// Attach the shared auto-probe `max_tokens` table so `send()`
    /// layers the discovered ceiling into the clamp chain. Wired by
    /// `registry_from_config` when the registry has a table.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        self.max_tokens_table = Some(table);
        self
    }

    /// Build from config, resolving the API key via the unified
    /// helper. Kept for backwards compatibility with hand-rolled
    /// callers (test fixtures); new dispatcher code goes through
    /// [`Self::from_resolved`].
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = std::env::var("OPENCODE_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::InvalidApiKey {
                message: "OPENCODE_API_KEY not set; provide via env, --api-key, or api_keys.toml"
                    .into(),
                http_status: None,
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// v0.10 dispatcher entry point. Builds an `OpenAICompatProvider`
    /// from a `ResolvedModelConfig` (one `(section, model_id)` pair),
    /// resolving the API key via the unified
    /// [`super::api_keys::lookup_key`] helper. The key lookup falls
    /// back from the section name to the canonical `kind` so a
    /// per-model alias like `gpt-5.6-luna` (kind=`"opencode"`)
    /// resolves against the `OPENCODE_API_KEY` env var rather than
    /// the non-existent `GPT-5.6-LUNA_API_KEY`. The dispatcher
    /// picks this constructor for endpoints whose path resolves to
    /// [`super::wire_format::WireFormatId::OpenAI`].
    pub fn from_resolved(resolved: &crate::config::ResolvedModelConfig) -> Result<Self> {
        let kind = super::api_keys::lookup_kind_for_resolved(resolved);
        let key = super::api_keys::lookup_key(&kind, None)
            .ok_or_else(|| Error::InvalidApiKey {
                message: format!(
                    "{}_API_KEY not set; provide via env, --api-key, or api_keys.toml",
                    kind.to_ascii_uppercase()
                ),
                http_status: None,
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
        let client = build_client()?;
        Ok(Self {
            name: resolved.section.clone(),
            model: resolved.id.clone(),
            endpoint: resolved.endpoint.clone(),
            api_key: SecretString::new(key),
            client,
            max_retries: 3,
            provider_max_tokens: resolved.max_tokens,
            omit_max_tokens: resolved.omit_max_tokens,
            max_tokens_table: None,
        })
    }

    /// Compute the URL for the responses endpoint.
    fn responses_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/responses") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/responses")
        } else {
            format!("{base}/v1/responses")
        }
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

    /// Returns `None` when `omit_max_tokens` is set (so the field is
    /// dropped from the wire body), otherwise the request's max_tokens.
    fn effective_max_tokens(&self, requested: u32) -> Option<u32> {
        if self.omit_max_tokens {
            None
        } else {
            Some(requested)
        }
    }
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: &'a str,
    /// Output token ceiling. `None` serializes as field-absent (via
    /// `skip_serializing_if`), required for providers that reject
    /// the presence of the field (e.g. `gpt-5.6-luna`).
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    /// Responses-API JSON output gate. The Responses API rejects the
    /// legacy `response_format` field and expects the same shape
    /// under `text.format` (`{"text": {"format": {"type":
    /// "json_object"}}}`). Upstream returns HTTP 400 with
    /// `Unsupported parameter: 'response_format'. In the Responses
    /// API, this parameter has moved to 'text.format'` when the
    /// legacy field is sent, so the wire builder must serialise the
    /// new location. Mirrors the OpenAI-compat gating: omitted for
    /// free-text roles (`Sketch`, `FinalReport`, etc.) and for
    /// models on the `response_format_opt_out` list.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ResponsesText>,
    stream: bool,
}

/// Wrapper struct that mirrors the Responses API's `text.format`
/// nesting. The outer field is `text`; the inner `format` carries
/// the JSON mode discriminator (`{"type": "json_object"}`).
#[derive(Debug, Serialize)]
struct ResponsesText {
    format: ResponsesTextFormat,
}

#[derive(Debug, Serialize)]
struct ResponsesTextFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Compute the gate the OpenAI-compat path uses to decide whether
/// to send `text: { format: { type: "json_object" } }` (the
/// Responses-API shape — the legacy `response_format` field is
/// rejected by the upstream). Kept local so the wire builder can
/// stay a free function and so the `#[cfg(test)]` block can drive
/// it directly.
fn wants_response_format(role: crate::llm::Role, model: &str) -> bool {
    role_requires_json(role) && !model_skips_response_format(model)
}

#[derive(Debug, Deserialize)]
struct ResponsesBody {
    #[serde(default)]
    output: Vec<ResponsesOutput>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponsesOutput {
    #[serde(default)]
    content: Vec<ResponsesContent>,
}

#[derive(Debug, Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .connect_timeout(std::time::Duration::from_secs(15))
        .user_agent(concat!("moagan/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Provider {
            message: format!("build reqwest client: {e}"),
            http_status: None,
        })
}

/// Custom Debug that masks `max_tokens_table` — `MaxTokensTable`
/// does not implement `Debug` (that lives in `probe_table.rs`,
/// outside this provider's owned files). The table is a shared
/// `Arc`, so emitting `<shared>` keeps the dump informative.
impl std::fmt::Debug for OpenAICompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAICompatProvider")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("provider_max_tokens", &self.provider_max_tokens)
            .field("omit_max_tokens", &self.omit_max_tokens)
            .field("max_tokens_table", &"<shared>")
            .finish()
    }
}

#[async_trait]
impl Provider for OpenAICompatProvider {
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
        // v0.10: the wire format is detected from the endpoint URL
        // (the dispatcher did this at construction time). For
        // `OpenAICompatProvider`, two wire formats share the
        // `/v1/responses` (Responses API) and the chat-completions
        // path — but `OpenAICompatProvider` only handles the
        // Responses path. Anything else (e.g. legacy callers that
        // constructed this provider directly) falls back to the
        // generic OpenAI-compat capability.
        if self.endpoint.ends_with("/responses") {
            ProviderCapabilities::for_opencode_go_responses()
        } else {
            ProviderCapabilities::for_openai_compat()
        }
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        // The regular `send` keeps every cap (operator override,
        // table, u32::MAX) so a stale or empty
        // table cannot leak an unbounded value into the wire body.
        // The probe path uses `send_probe` instead, which skips the
        // u32::MAX so the algorithm sees the
        // upstream's real boundary.
        self.send_with_safety_clamp(req, true).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Mirror of the clamp chain in
        // `send_with_safety_clamp(_, true)` (and `send_streaming`,
        // which applies the same chain) so the audit-log hash is
        // byte-for-byte identical to the wire body. Same ordering
        // as `send`:
        //   1. `u32::MAX` (16_384 for the
        //      2026-08-04 model roster).
        //   2. `provider_max_tokens` (operator TOML override).
        //   3. `MaxTokensTable::resolve_cached` (auto-probed value).
        // `omit_max_tokens` does NOT affect this value — it only
        // drops the field from the wire body; when the field is
        // present the value is the same clamped `u32`.
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        req.max_tokens.min(operator_cap).min(table_cap)
    }

    /// Bypass variant for the auto-probe. Skips every cap
    /// (operator override, table, u32::MAX) so
    /// the algorithm sees the upstream's real boundary.
    async fn send_probe(&self, req: &Request) -> Result<(u16, Response)> {
        self.send_with_safety_clamp(req, false).await
    }

    /// Cap the exponential probe at `u32::MAX`
    /// (16_384 for the 2026-08-04 model roster). `gpt-5.6-luna`
    /// rejects values above this with HTTP 400, so the probe must
    /// short-circuit at the smallest `2^k > 16_384` (k=15 →
    /// 32_768) rather than spend a round-trip on a value the
    /// upstream will never accept. Mirrors the wiring on
    /// `OpenAiCompatProvider` / `MinimaxProvider`.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        u32::MAX
    }
}

/// Build the wire-level request body used by `send`.
fn build_responses_body<'a>(
    req: &'a Request,
    model: &'a str,
    stream: bool,
    omit_max_tokens: bool,
) -> ResponsesRequest<'a> {
    ResponsesRequest {
        model,
        instructions: Some(&req.system),
        input: &req.user,
        max_tokens: if omit_max_tokens {
            None
        } else {
            Some(req.max_tokens)
        },
        temperature: req.temperature,
        top_p: req.top_p,
        text: responses_text_json_object(wants_response_format(req.role, model)),
        stream,
    }
}

/// Build the `text: { format: { type: "json_object" } }` payload
/// when the JSON gate fires, or `None` so the field is dropped from
/// the wire body entirely. Centralises the nested-struct build so
/// the streaming and non-streaming paths stay byte-identical.
fn responses_text_json_object(wants: bool) -> Option<ResponsesText> {
    if wants {
        Some(ResponsesText {
            format: ResponsesTextFormat {
                kind: "json_object",
            },
        })
    } else {
        None
    }
}

/// Consume an OpenAI Responses SSE wire body and return the
/// accumulated text plus the most-recent usage block.
///
/// Each `data:` payload is expected to carry a partial Responses
/// shape (`{"output": [{"content": [{"type": "output_text",
/// "text": "..."}]}]}`) plus an optional `usage` block that
/// may arrive on the terminal event. The function walks every
/// payload, concatenates `output_text` blocks in arrival order,
/// and replaces the cached usage with the most recent observation
/// (the terminal event typically carries the authoritative
/// counts).
///
/// Visible to tests so they can exercise the SSE consumer
/// without having to spin up a wiremock server.
fn accumulate_sse_responses(body: &[u8]) -> Result<(String, ResponsesUsage)> {
    let mut parser = SseParser::new(body);
    let mut text = String::new();
    let mut usage = ResponsesUsage::default();
    loop {
        match parser.next_data::<ResponsesBody>() {
            Ok(Some(delta)) => {
                for out in delta.output {
                    for c in out.content {
                        if c.kind == "output_text"
                            && let Some(t) = c.text
                        {
                            text.push_str(&t);
                        }
                    }
                }
                if let Some(u) = delta.usage {
                    usage = u;
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(match e {
                    SseError::Io(err) => Error::Provider {
                        message: format!("sse io: {err}"),
                        http_status: None,
                    },
                    SseError::Parse(err) => Error::Provider {
                        message: format!("sse parse: {err}"),
                        http_status: None,
                    },
                });
            }
        }
    }
    Ok((text, usage))
}

impl OpenAICompatProvider {
    /// Shared HTTP body between `send` and `send_probe`. When
    /// `safety_clamp = true` the wire body is capped by every layer
    /// (operator override + table + `u32::MAX`);
    /// when `false` the wire body carries `req.max_tokens` verbatim
    /// subject only to the [`MIN_AUTOPROBE_FLOOR`] minimum.
    async fn send_with_safety_clamp(
        &self,
        req: &Request,
        safety_clamp: bool,
    ) -> Result<(u16, Response)> {
        let url = self.responses_url();
        if req.stream {
            return self.send_streaming(req, &url).await;
        }
        // Probe path uses `max_retries = 0`: a 4xx IS the algorithm's
        // signal (max-tokens rejection), retrying it wastes the 5s
        // probe timeout and risks masking the boundary if a retry
        // happens to succeed. Production path keeps the existing
        // self.max_retries (3) for transient 5xx storms.
        let max_retries = if safety_clamp { self.max_retries } else { 0 };
        let mut req = req.clone();
        if safety_clamp {
            // Three-layer cap. Highest priority (smallest wins) to lowest:
            //   1. u32::MAX — documented hard ceiling
            //      for the 2026-08-04 roster (kimi-k* / gpt-5.6-luna).
            //   2. provider_max_tokens — operator TOML override.
            //   3. MaxTokensTable::resolve_cached — auto-probed value.
            let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
            let table_cap = self
                .max_tokens_table
                .as_ref()
                .and_then(|t| t.resolve_cached(self.name(), self.model()))
                .unwrap_or(u32::MAX);
            let cap = operator_cap.min(table_cap);
            req.max_tokens = req.max_tokens.min(cap);
        } else {
            // Probe path: bypass every cap. Floor ensures we
            // never ask for `max_tokens < 1024` (some upstreams
            // reject the request outright below that minimum).
            req.max_tokens = req.max_tokens.max(MIN_AUTOPROBE_FLOOR);
        }
        let max_tokens = self.effective_max_tokens(req.max_tokens);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = ResponsesRequest {
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
                provider = self.name,
                attempt,
                stage = "http.request.started",
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
                        provider = self.name,
                        attempt,
                        stage = "http.headers.received",
                        status = status_code,
                        elapsed_ms = request_started.elapsed().as_millis(),
                        "Provider HTTP stage"
                    );
                    if status.is_success() {
                        let decode_started = std::time::Instant::now();
                        let parsed: ResponsesBody =
                            resp.json().await.map_err(|e| Error::Provider {
                                message: format!("decode: {e}"),
                                http_status: None,
                            })?;
                        tracing::debug!(
                            provider = self.name,
                            attempt,
                            stage = "http.body.decoded",
                            status = status_code,
                            elapsed_ms = decode_started.elapsed().as_millis(),
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
                        let response = Response {
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
                    let body = resp.text().await.unwrap_or_default();
                    let err = match status_code {
                        401 | 403 => Error::InvalidApiKey {
                            message: format!("http {status_code}: {body}"),
                            http_status: Some(status_code),
                        },
                        429 => Error::PlanExhausted {
                            message: format!("http {status_code}: {body}"),
                            http_status: Some(status_code),
                        },
                        408 | 504 | 524 => Error::Timeout {
                            message: format!("http {status_code}: {body}"),
                            http_status: Some(status_code),
                        },
                        _ => Error::Provider {
                            message: format!("http {status_code}: {body}"),
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

    /// Streaming variant of [`Provider::send`]: sets
    /// `stream=true` on the wire body, reads the entire SSE
    /// response, and returns a single aggregated `Response` with
    /// the joined text and the terminal usage block.
    async fn send_streaming(&self, req: &Request, url: &str) -> Result<(u16, Response)> {
        // Apply three-layer max_tokens cap (same as the non-streaming
        // path and as `OpenAiCompatProvider` / `MinimaxProvider`).
        // Clamping here as well so the SSE wire body carries the same
        // value the upstream would see on the non-streaming path.
        let mut req = req.clone();
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        let cap = operator_cap.min(table_cap);
        req.max_tokens = req.max_tokens.min(cap);
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
            provider = self.name,
            status = status_code,
            elapsed_ms = request_started.elapsed().as_millis(),
            "Provider HTTP stage (sse)"
        );
        let (text, usage) = accumulate_sse_responses(&bytes)?;
        // D.29.2: enforce the centralised response cap (10 MiB)
        // on the SSE-accumulated text. SSE streams can grow
        // indefinitely if a misconfigured model keeps emitting
        // tokens; the cap turns that into a hard error.
        check_size("response", text.len(), MAX_RESPONSE_BYTES)?;
        let response = Response {
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
}

impl OpenAICompatProvider {
    /// Compute the URL the provider POSTs to (mirrors the old
    /// `OpenCodeGoDispatch::url` accessor that the v0.9 dispatcher
    /// used). Kept as a public method so external callers / tests can
    /// still inspect the routed URL.
    pub fn url(&self) -> String {
        self.responses_url()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_url_handles_known_suffixes() {
        // v0.10: the constructor reads `endpoint` (section-level
        // default) verbatim; provide a URL ending in `/v1/responses`
        // so the test exercises the "already-suffixed" branch of
        // `responses_url()`.
        let p = OpenAICompatProvider::new(
            &ProviderConfig {
                endpoint: Some("https://opencode.ai/zen/go/v1/responses".into()),
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(p.responses_url(), "https://opencode.ai/zen/go/v1/responses");
    }

    #[test]
    fn responses_url_handles_responses_suffix() {
        // v0.10: same as above — provide the section-level endpoint
        // so the constructor picks it up. The fixture mirrors a
        // production `[providers.opencode]` block whose `endpoint`
        // field already names the responses path.
        let p = OpenAICompatProvider::new(
            &ProviderConfig {
                endpoint: Some("https://opencode.ai/zen/go/v1/responses".into()),
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(p.responses_url(), "https://opencode.ai/zen/go/v1/responses");
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
        }
        let result = OpenAICompatProvider::from_config(&ProviderConfig {
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            plan: None,
        });
        assert!(matches!(result, Err(Error::InvalidApiKey { .. })));
    }

    #[test]
    fn openai_compat_streaming_consumes_sse_data_lines() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            let body = "\
data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"Hello \"}]}]}\n\n\
data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"world\"}]}]}\n\n\
data: {\"output\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":34}}\n\n\
data: [DONE]\n\n";
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body),
                )
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 256,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, response) = p.send(&req).await.unwrap();
            assert_eq!(status, 200);
            assert_eq!(response.text, "Hello world");
            assert_eq!(response.usage.input_tokens, 12);
            assert_eq!(response.usage.output_tokens, 34);
            assert!(!response.truncated);
            assert!(response.finish_reason.is_none());
        });
    }

    #[test]
    fn openai_compat_streaming_returns_error_on_mid_stream_failure() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            // First delta is well-formed; the second is intentionally
            // malformed so the SseParser surfaces a parse error.
            let body = "\
data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}\n\n\
data: {not json}\n\n";
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body),
                )
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 64,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let err = p.send(&req).await.unwrap_err();
            match err {
                Error::Provider { message, .. } => assert!(
                    message.contains("sse parse"),
                    "expected sse parse error, got {message:?}"
                ),
                other => panic!("expected Error::Provider, got {other:?}"),
            }
        });
    }

    #[test]
    fn openai_compat_non_streaming_still_works() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": [{
                        "content": [
                            {"type": "output_text", "text": "plain"}
                        ]
                    }],
                    "usage": {"input_tokens": 7, "output_tokens": 11}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 64,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, response) = p.send(&req).await.unwrap();
            assert_eq!(status, 200);
            assert_eq!(response.text, "plain");
            assert_eq!(response.usage.input_tokens, 7);
            assert_eq!(response.usage.output_tokens, 11);
        });
    }

    #[test]
    fn accumulate_sse_responses_joins_text_and_picks_last_usage() {
        let body = b"\
data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"Hello \"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n\
data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"world\"}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n\n\
data: [DONE]\n\n";
        let (text, usage) = accumulate_sse_responses(body).unwrap();
        assert_eq!(text, "Hello world");
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 4);
    }

    #[test]
    fn send_clamps_max_tokens_to_provider_cap() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{body_partial_json, method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            // body_partial_json only matches when the outbound
            // request body literally carries max_tokens:8192; if the
            // clamp regresses the upstream would see 1_000_000 and
            // wiremock would fall through to its default 404, which
            // the provider turns into an Error::Provider — failing
            // the test.
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .and(body_partial_json(serde_json::json!({
                    "max_tokens": 8192,
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": [{
                        "content": [
                            {"type": "output_text", "text": "ok"}
                        ]
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 2}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    // v0.10: the operator `max_tokens` cap lives on
                    // the per-model `ModelConfig`. Set it explicitly
                    // so the constructor sees `provider_max_tokens =
                    // Some(8192)` and the wire body is clamped.
                    models: vec![crate::config::ModelConfig {
                        id: "minimax-m3".into(),
                        endpoint: None,
                        max_tokens: Some(8192),
                    }],
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
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
        });
    }

    #[test]
    fn send_streaming_clamps_max_tokens_to_provider_cap() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{body_partial_json, method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .and(body_partial_json(serde_json::json!({
                    "max_tokens": 8192,
                    "stream": true,
                })))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            "data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n\
data: [DONE]\n\n",
                        ),
                )
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    // v0.10: set the operator cap on the per-model
                    // `ModelConfig` so the constructor wires
                    // `provider_max_tokens = Some(8192)` and the SSE
                    // body is clamped to that value.
                    models: vec![crate::config::ModelConfig {
                        id: "minimax-m3".into(),
                        endpoint: None,
                        max_tokens: Some(8192),
                    }],
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, _response) = p.send(&req).await.unwrap();
            assert_eq!(status, 200);
        });
    }

    #[test]
    fn send_omits_max_tokens_when_omit_flag_set() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{body_partial_json, method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            // Use a matcher that does NOT constrain max_tokens so the
            // mock accepts the request regardless of whether the field
            // is present. The actual assertion is on the recorded
            // body below.
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .and(body_partial_json(serde_json::json!({
                    // v0.10: the wire body carries whatever
                    // `req.model` the dispatcher resolved. The
                    // v0.9 fixture used `"gpt-5.6-luna"` because
                    // the per-model alias section was the
                    // canonical model; today the canonical
                    // `opencode` section hosts that model id and
                    // the dispatcher's section-name resolution
                    // passes it through verbatim.
                    "model": "minimax-m3",
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "output": [{
                        "content": [
                            {"type": "output_text", "text": "ok"}
                        ]
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 2}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    // v0.10: provide a per-model `id` so the
                    // constructor wires `self.model = "minimax-m3"`
                    // and the wire body carries the model id
                    // verbatim. Without it the legacy constructor
                    // leaves `self.model = ""` and the body never
                    // matches the wiremock's `body_partial_json`
                    // constraint.
                    models: vec![crate::config::ModelConfig {
                        id: "minimax-m3".into(),
                        endpoint: None,
                        max_tokens: None,
                    }],
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: true,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
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
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert!(
                body.get("max_tokens").is_none(),
                "max_tokens must be absent when omit_max_tokens=true, got body={body}"
            );
        });
    }

    #[test]
    fn send_includes_max_tokens_when_omit_flag_unset() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
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
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                // 1024 is well below u32::MAX
                // (16_384) so the cap does not engage and the
                // assertion about the field surviving the wire
                // builder stays meaningful.
                max_tokens: 1024,
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
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert_eq!(
                body.get("max_tokens").and_then(|v| v.as_u64()),
                Some(1024),
                "max_tokens must be present with the requested value, got body={body}"
            );
        });
    }

    /// v0.10 (post Phase 8): the legacy `OPENCODE_GO_MAX_TOKENS_CAP`
    /// global clamp is gone. The OpenCode Go relay on the chat-
    /// completions path no longer has a per-kind ceiling baked
    /// into the wire layer — the auto-probe discovers the real
    /// upstream boundary per `(provider, model)` and caches it in
    /// `max_tokens_auto.toml`. With no probe result and no
    /// operator override, the wire body carries the request's
    /// raw `max_tokens` unchanged. This regression guard pins
    /// that the clamp chain no longer applies a 16_384 ceiling
    /// to OpenCode Go chat-completions calls.
    #[test]
    fn send_does_not_clamp_max_tokens_when_no_probe_or_override() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
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
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(format!("{}/v1", server.uri())),
                    // None on purpose — exercise the "no TOML
                    // override, no table" path. v0.10 removed the
                    // global `OPENCODE_GO_MAX_TOKENS_CAP`; with
                    // nothing in the chain the wire body carries
                    // the REQUESTED value unchanged.
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
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
            // v0.10: the v0.9 `OPENCODE_GO_MAX_TOKENS_CAP = 16_384`
            // global ceiling is gone. Without an operator override
            // or a table entry, the wire body carries the requested
            // value verbatim — the only layers left in the clamp
            // chain are `u32::MAX` (operator cap, default) and
            // `u32::MAX` (table, missing).
            assert_eq!(
                body.get("max_tokens").and_then(|v| v.as_u64()),
                Some(1_000_000),
                "with no operator cap and no table, body must carry the requested value, got body: {body}"
            );
        });
    }

    /// Regression guard for the streaming Responses path
    /// (`send_streaming`): with no operator cap and no table entry,
    /// the SSE wire body must carry the REQUESTED `max_tokens`
    /// unchanged — the v0.10 schema removed the global
    /// `OPENCODE_GO_MAX_TOKENS_CAP` so there is no implicit
    /// ceiling. The v0.9 name "opencode_go_hard_cap" is kept
    /// only as a documentation breadcrumb.
    #[test]
    fn send_streaming_clamps_max_tokens_to_opencode_go_hard_cap() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            "data: {\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n\
data: [DONE]\n\n",
                        ),
                )
                .expect(1)
                .mount(&server)
                .await;
            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    // v0.10: no operator cap and no table — the
                    // request must reach the SSE path unchanged.
                    models: Vec::new(),
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
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
            // v0.10: the v0.9 `OPENCODE_GO_MAX_TOKENS_CAP = 16_384`
            // global ceiling is gone. With no operator override and
            // no table entry, the SSE wire body carries the requested
            // value verbatim — same contract as the non-streaming
            // path so a `stream: true` flip cannot introduce a
            // regression.
            assert_eq!(
                body.get("max_tokens").and_then(|v| v.as_u64()),
                Some(1_000_000),
                "with no operator cap and no table, SSE body must carry the requested value, got body: {body}"
            );
        });
    }

    fn json_request(role: crate::llm::Role, model: &str) -> Request {
        Request {
            role,
            model: model.into(),
            system: "system".into(),
            user: "user".into(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    /// JSON role + non-opted-out model → `text.format` must be
    /// serialised as `{"type":"json_object"}` (the Responses-API
    /// location; the legacy `response_format` field is rejected by
    /// the upstream). Pins the contract the capability matrix
    /// advertises (`supports_response_format: true`).
    #[test]
    fn responses_wire_sets_text_format_when_role_requires_json_and_model_is_not_opted_out() {
        let req = json_request(crate::llm::Role::Intake, "gpt-5.6-luna");
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value.get("text"),
            Some(&serde_json::json!({
                "format": {"type": "json_object"}
            })),
            "Intake role + gpt-5.6-luna must include text.format, got: {value}"
        );
        // Regression pin: the Responses API rejects the legacy
        // `response_format` key (HTTP 400). The wire body must
        // never carry both — the Responses shape uses
        // `text.format`, the Chat Completions shape uses
        // `response_format`, and the two paths serialise different
        // structs.
        assert!(
            value.get("response_format").is_none(),
            "wire body must not carry the legacy response_format key on the Responses path, got: {value}"
        );
    }

    /// `Sketch` is a free-text role and is NOT in `role_requires_json`,
    /// so the `text` field must stay absent even on a non-opted-out
    /// model.
    #[test]
    fn responses_wire_omits_text_format_for_role_sketch() {
        let req = json_request(crate::llm::Role::Sketch, "gpt-5.6-luna");
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert!(
            value.get("text").is_none(),
            "Sketch role must drop text.format, got: {value}"
        );
    }

    /// Model on the opt-out list + JSON role → `text` field must
    /// stay absent so the upstream returns raw markdown instead of
    /// prose-prefixed JSON.
    #[test]
    fn responses_wire_omits_text_format_for_opted_out_model() {
        let req = json_request(crate::llm::Role::Intake, "kimi-k3");
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert!(
            value.get("text").is_none(),
            "opted-out model kimi-k3 must drop text.format, got: {value}"
        );
    }

    /// The pre-existing fields (`model`, `instructions`, `input`,
    /// `max_tokens`, `temperature`, `top_p`, `stream`) are untouched
    /// regardless of the gate. Guards against a regression that
    /// would re-shape the body when adding the new field.
    #[test]
    fn responses_wire_includes_other_fields_unaffected() {
        let req = Request {
            model: "minimax-m3".into(),
            role: crate::llm::Role::Route,
            system: "sys".into(),
            user: "user".into(),
            max_tokens: 256,
            temperature: Some(0.4),
            top_p: Some(0.9),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        // The body carries whichever model id the caller passes
        // (the dispatcher resolves it from the section's
        // `models[].id`). The pre-existing comment referenced
        // `gpt-5.6-luna` from the v0.9 fixture shape; the v0.10
        // schema passes `req.model` through verbatim.
        assert_eq!(value["model"], "minimax-m3");
        assert_eq!(value["instructions"], "sys");
        assert_eq!(value["input"], "user");
        assert_eq!(value["max_tokens"], 256);
        let temp = value["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.4).abs() < 1e-6,
            "temperature must round-trip, got {temp} in {value}"
        );
        let top_p = value["top_p"].as_f64().unwrap();
        assert!(
            (top_p - 0.9).abs() < 1e-6,
            "top_p must round-trip, got {top_p} in {value}"
        );
        assert_eq!(value["stream"], false);
        // Gate was true (Route + gpt-5.6-luna), so the new field
        // is also present under the Responses-API `text.format`
        // location.
        assert_eq!(
            value["text"],
            serde_json::json!({"format": {"type": "json_object"}})
        );
    }

    /// Auto-probe table clamp contract: when
    /// `with_max_tokens_table` attaches a table carrying a
    /// discovered value smaller than the requested `max_tokens`
    /// AND smaller than the documented hard cap, the wire body
    /// must carry the discovered value on the non-streaming
    /// Responses path. Pins the v0.7 precedence order:
    /// `u32::MAX` > operator > table > req.
    #[test]
    fn send_clamps_max_tokens_to_table_value() {
        use std::sync::Arc;

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

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
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
                .expect(1)
                .mount(&server)
                .await;

            let transport: Arc<dyn ProbeTransport> = Arc::new(CappedTransport { cap: 10_000 });
            let table = Arc::new(MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR));
            // v0.10: the legacy `new()` constructor reads both
            // `name` and `model` from `models[0].id`, so the table
            // key has to match that pair. The dispatched path
            // (`from_resolved`) uses `(section_name, model_id)`;
            // this test exercises the hand-rolled constructor path
            // so we mirror that — provider name == model id ==
            // "gpt-5.6-luna".
            let discovered = table
                .probe_and_store(
                    "gpt-5.6-luna",
                    "gpt-5.6-luna",
                    transport,
                    crate::llm::probe::MAX_AUTOPROBE_CEILING,
                )
                .await
                .expect("probe_and_store");
            // The wire-body assertion below uses `discovered`
            // directly: this test pins the wiring contract
            // (table value honoured on the wire) without depending
            // on the probe algorithm's exact convergence — that
            // algorithm has a known ±N imprecision at non-trivial
            // boundaries (see pre-existing
            // `probe::tests::detect_finds_cap_at_8k`).

            let p = OpenAICompatProvider::new(
                &ProviderConfig {
                    // v0.10 canonical schema: one `models[]` entry
                    // whose `id` drives both `name` and `model`
                    // (the legacy constructor reads them from
                    // `models.first()`). `max_tokens: None` leaves
                    // `provider_max_tokens = None` so the operator
                    // cap chain stays at `u32::MAX` and the table
                    // value is the only clamp the wire body sees.
                    models: vec![crate::config::ModelConfig {
                        id: "gpt-5.6-luna".into(),
                        endpoint: None,
                        max_tokens: None,
                    }],
                    endpoint: Some(format!("{}/v1", server.uri())),
                    temperature: None,
                    top_p: None,
                    omit_max_tokens: false,
                    plan: None,
                    max_token_auto: None,
                    max_token_auto_enabled: None,
                    max_token_auto_save: true,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap()
            .with_max_tokens_table(table);

            let req = Request {
                model: "minimax-m3".into(),
                role: crate::llm::Role::Intake,
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
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
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert_eq!(
                body.get("max_tokens").and_then(|v| v.as_u64()),
                Some(discovered as u64),
                "wire body must carry the table-resolved value ({discovered}), got body: {body}"
            );
        });
    }
}
