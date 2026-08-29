//! `anthropic_compat` provider — Anthropic-compatible wire format
//! served at `/v1/messages`.
//!
//! v0.10: the v0.9 `opencode_go` dispatcher is gone. The historical
//! `opencode_go` → `OpenCodeDispatch` breadcrumb (rewritten in v0.13.x
//! as part of the close-out) is preserved here so the v0.9 → v0.13.x
//! lineage stays auditable — note that no `OpenCodeDispatch` symbol
//! exists in the tree today; only the breadcrumb text was renamed.
//! The provider is generic over its section name — the dispatcher routes any
//! section whose endpoint URL ends with `/v1/messages` to this
//! provider. That covers direct MiniMax
//! (`https://api.minimax.io/anthropic/v1/messages`) and the OpenCode
//! models that share the Anthropic SDK
//! (`minimax-m3`, `qwen3.8-max`, `qwen3.7-max`, etc.).
//!
//! The wire format is identical to the `minimax` provider so the
//! request body (MessagesRequestBody) and response decoder
//! (MessagesResponseBody) are shared via `super::http`. The API key
//! comes from `api_keys.toml` keyed by the section name (or the
//! matching env var fallback).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::ProviderCapabilities;
use super::http::{body_from_request, build_client, build_headers, classify_status, retry_after};
use super::probe::MIN_AUTOPROBE_FLOOR;
use super::probe_table::MaxTokensTable;
use super::provider::Provider;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::wire::{Request, Response, Usage};

/// Anthropic-compat provider routed through the
/// `/v1/messages` endpoint. Distinct from the legacy `minimax`
/// provider so future behavior changes (e.g. response_format,
/// custom headers) don't leak across backends.
#[derive(Clone)]
pub struct AnthropicCompatProvider {
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) endpoint: String,
    pub(crate) api_key: SecretString,
    pub(crate) client: reqwest::Client,
    pub(crate) max_retries: u32,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,000,000), so the per-role runtime
    /// value normally fits under the cap. The clamp below exists
    /// for the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request with 400.
    pub(crate) provider_max_tokens: Option<u32>,
    /// Auto-probed `max_tokens` table. When `Some` the
    /// `resolve_cached(self.name(), self.model())` value joins the
    /// clamp chain as the third-highest layer (kind-level cap >
    /// operator override > table). `None` when the provider was
    /// built without going through `registry_from_config` — unit
    /// tests and legacy call paths.
    pub(crate) max_tokens_table: Option<Arc<MaxTokensTable>>,
}

impl AnthropicCompatProvider {
    /// Build from a `ProviderConfig` and a resolved API key.
    /// Kept for backwards compatibility with hand-rolled callers
    /// (legacy test fixtures); new dispatcher code goes through
    /// [`Self::from_resolved`].
    ///
    /// When `spec.models` is empty (the v0.9 fixture shape) the
    /// constructor falls back to the section name for the lookup,
    /// the first model id for the model id, and the section-level
    /// `endpoint` for the URL.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        tracing::debug!(
            models = spec.models.len(),
            endpoint = spec.endpoint.as_deref(),
            "AnthropicCompatProvider::new: enter"
        );
        let client = build_client()?;
        let name = spec
            .models
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "anthropic_compat".to_owned());
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
            "AnthropicCompatProvider: constructed"
        );
        Ok(Self {
            name,
            model,
            endpoint,
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens,
            max_tokens_table: None,
        })
    }

    /// Attach the shared auto-probe `max_tokens` table so `send()`
    /// layers the discovered ceiling into the clamp chain. Wired by
    /// `registry_from_config` when the registry has a table.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        tracing::debug!(name = %self.name, "AnthropicCompatProvider::with_max_tokens_table");
        self.max_tokens_table = Some(table);
        self
    }

    /// Build from config, resolving the API key via the unified
    /// helper. Kept for backwards compatibility with hand-rolled
    /// callers (test fixtures); new dispatcher code goes through
    /// [`Self::from_resolved`].
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        tracing::debug!("AnthropicCompatProvider::from_config: enter");
        let key = std::env::var("OPENCODE_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                tracing::error!("AnthropicCompatProvider::from_config: OPENCODE_API_KEY missing");
                Error::InvalidApiKey {
                    message:
                        "OPENCODE_API_KEY not set; provide via env, --api-key, or api_keys.toml"
                            .into(),
                    http_status: None,
                }
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// v0.10 dispatcher entry point. Builds an `AnthropicCompatProvider`
    /// from a `ResolvedModelConfig` (one `(section, model_id)` pair),
    /// resolving the API key via the unified
    /// [`super::api_keys::lookup_key`] helper. The key lookup falls
    /// back from the section name to the canonical `kind` so a
    /// per-model alias like `minimax-m3` (kind=`"opencode"`)
    /// resolves against the `OPENCODE_API_KEY` env var rather than
    /// the non-existent `MINIMAX-M3_API_KEY`. The dispatcher picks
    /// this constructor for endpoints whose path resolves to
    /// [`super::wire_format::WireFormatId::Anthropic`].
    pub fn from_resolved(resolved: &crate::config::ResolvedModelConfig) -> Result<Self> {
        tracing::debug!(
            section = %resolved.section,
            model = %resolved.id,
            "AnthropicCompatProvider::from_resolved: enter"
        );
        let kind = super::api_keys::lookup_kind_for_resolved(resolved);
        let key = super::api_keys::lookup_key(&kind, None)
            .ok_or_else(|| {
                tracing::error!(
                    kind,
                    "AnthropicCompatProvider::from_resolved: API key missing"
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
        let client = build_client()?;
        tracing::info!(
            section = %resolved.section,
            model = %resolved.id,
            "AnthropicCompatProvider::from_resolved: constructed"
        );
        Ok(Self {
            name: resolved.section.clone(),
            model: resolved.id.clone(),
            endpoint: resolved.endpoint.clone(),
            api_key: SecretString::new(key),
            client,
            max_retries: 3,
            provider_max_tokens: resolved.max_tokens,
            max_tokens_table: None,
        })
    }

    /// Compute the URL for the messages endpoint.
    pub fn messages_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        let url = if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        tracing::trace!(endpoint = %self.endpoint, url = %url, "messages_url");
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

/// Custom Debug that masks `max_tokens_table` — `MaxTokensTable`
/// does not implement `Debug` (that lives in `probe_table.rs`,
/// outside this provider's owned files). The table is a shared
/// `Arc`, so emitting `<shared>` is enough to identify the instance.
impl std::fmt::Debug for AnthropicCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicCompatProvider")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("provider_max_tokens", &self.provider_max_tokens)
            .field("max_tokens_table", &"<shared>")
            .finish()
    }
}

#[async_trait]
impl Provider for AnthropicCompatProvider {
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
        ProviderCapabilities::for_anthropic_compat()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        self.send_with_safety_clamp(req, true).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Mirror of the clamp chain in
        // `send_with_safety_clamp(_, true)` so the audit-log hash is
        // byte-for-byte identical to the wire body. Same ordering as
        // `send`:
        //   1. `u32::MAX` (16_384 for the
        //      2026-08-04 model roster).
        //   2. `provider_max_tokens` (operator TOML override).
        //   3. `MaxTokensTable::resolve_cached` (auto-probed value).
        //
        // `None` on `req.max_tokens` is treated as `u32::MAX` so the
        // audit hash stays deterministic when the auto-heal path
        // drops the field from the wire body.
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        req.max_tokens
            .unwrap_or(u32::MAX)
            .min(operator_cap)
            .min(table_cap)
    }

    /// Bypass variant for the auto-probe. Skips every cap
    /// (operator override, table, u32::MAX) so
    /// the algorithm sees the upstream's real boundary. The
    /// regular `send` keeps every cap so a stale or empty table
    /// cannot leak an unbounded value into the wire body.
    async fn send_probe(&self, req: &Request) -> Result<(u16, Response)> {
        self.send_with_safety_clamp(req, false).await
    }

    /// Cap the exponential probe at `u32::MAX`
    /// (16_384 for the 2026-08-04 model roster). Several upstreams
    /// (qwen3.x, gpt-5.6-luna, etc.) reject values above this
    /// with HTTP 400, so the probe must short-circuit at the
    /// smallest `2^k > 16_384` (k=15 → 32_768) rather than spend a
    /// round-trip on a value the upstream will never accept.
    /// Mirrors the wiring on `OpenAiCompatProvider` /
    /// `MinimaxProvider`.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        u32::MAX
    }
}

impl AnthropicCompatProvider {
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
        let url = self.messages_url();
        let mut req = req.clone();
        // Probe path uses `max_retries = 0`: a 4xx IS the algorithm's
        // signal (max-tokens rejection), retrying it wastes the 5s
        // probe timeout and risks masking the boundary if a retry
        // happens to succeed. Production path keeps the existing
        // self.max_retries (3) for transient 5xx storms.
        let max_retries = if safety_clamp { self.max_retries } else { 0 };
        if safety_clamp {
            // Three-layer cap. Highest priority (smallest wins) to lowest:
            //   1. u32::MAX — documented hard ceiling
            //      for the 2026-08-04 model roster.
            //   2. provider_max_tokens — operator TOML override.
            //   3. MaxTokensTable::resolve_cached — auto-probed value.
            //
            // `req.max_tokens = None` (set by the auto-healing
            // `param_rejections` path) is preserved through the
            // chain: the wire body omits the field so the upstream
            // accepts the request without the cap.
            let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
            let table_cap = self
                .max_tokens_table
                .as_ref()
                .and_then(|t| t.resolve_cached(self.name(), self.model()))
                .unwrap_or(u32::MAX);
            let cap = operator_cap.min(table_cap);
            if let Some(n) = req.max_tokens {
                req.max_tokens = Some(n.min(cap));
            }
        } else {
            // Probe path: bypass every cap. Floor ensures we
            // never ask for `max_tokens < 1024` (some upstreams
            // reject the request outright below that minimum).
            // `None` stays `None` so the probe honours any explicit
            // request to drop the field.
            if let Some(n) = req.max_tokens {
                req.max_tokens = Some(n.max(MIN_AUTOPROBE_FLOOR));
            }
        }
        let body = body_from_request(&req);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let headers = build_headers(self.api_key.expose(), &[])?;
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
                .headers(headers)
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
                    let retry_after = retry_after(&resp);
                    if status.is_success() {
                        let decode_started = std::time::Instant::now();
                        let parsed: OpenCodeMessagesResponseBody =
                            resp.json().await.map_err(|e| Error::Provider {
                                message: format!("decode response: {e}"),
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
                        let resp = parsed.into_response();
                        check_size("response", resp.text.len(), MAX_RESPONSE_BYTES)?;
                        return Ok((status_code, resp));
                    }
                    let body = resp.text().await.unwrap_or_default();
                    let err = classify_status(status, &body);
                    // `Throttled` is retryable: the upstream said
                    // "slow down" — the throttle governor outside
                    // this loop will shape role-level concurrency,
                    // but the per-attempt sleep here honours the
                    // `Retry-After` header when the upstream set one.
                    let retryable = matches!(
                        err,
                        Error::Timeout { .. }
                            | Error::PlanExhausted { .. }
                            | Error::Throttled { .. }
                            | Error::Provider { .. }
                    );
                    if !retryable || attempt >= max_retries {
                        return Err(err);
                    }
                    Self::sleep_with_jitter(attempt, retry_after).await;
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
}

impl AnthropicCompatProvider {
    /// Public URL the provider POSTs to. The legacy
    /// `OpenCodeGoDispatch::url` accessor lived on the v0.9
    /// dispatcher (no longer in the tree since v0.10; this comment
    /// preserves the v0.9 → v0.13.x lineage breadcrumb). v0.10
    /// keeps the URL builder as a plain method so external callers
    /// / tests can still inspect the routed URL.
    pub fn url(&self) -> String {
        self.messages_url()
    }
}

/// OpenCode Anthropic-compat response body. Extends the canonical
/// shape with a `thinking` block fallback: some OpenCode models
/// (qwen3.x, plus future additions) return the response content inside
/// a `thinking` block instead of a `text` block when the prompt
/// produces a planning pass. The shared `MessagesResponseBody` in
/// `super::http` ignores `thinking` blocks; here we collect both and
/// prepend the `text` block(s) first, then append the `thinking`
/// block(s) as a fallback so the JSON parser has something to chew on.
/// (`OpenCodeGoMessagesResponseBody` → `OpenCodeMessagesResponseBody`,
/// renamed in v0.13.x.)
#[derive(Debug, Deserialize)]
struct OpenCodeMessagesResponseBody {
    content: Vec<OpenCodeMessagesContent>,
    stop_reason: Option<String>,
    usage: Option<OpenCodeMessagesUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeMessagesContent {
    /// Block type from the response. Some OpenCode Go models
    /// (qwen3.7-max confirmed on 2026-08-04) omit the `type` field
    /// on the leading `thinking` block — only subsequent blocks
    /// carry `type: "text"`. Treat as optional and infer from the
    /// presence of `text` vs `thinking` when missing.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    text: Option<String>,
    /// Some OpenCode Go models put the response payload inside a
    /// `thinking` block. Captured here so we can fall back to it when
    /// no `text` block is present.
    thinking: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenCodeMessagesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl OpenCodeMessagesResponseBody {
    fn into_response(self) -> Response {
        let mut text = String::new();
        let mut thinking = String::new();
        for c in self.content {
            // Some OpenCode Go models omit `type` on the leading
            // thinking block. Infer the kind from the body's actual
            // fields so we don't drop the response.
            let kind = c.kind.as_deref().or_else(|| {
                if c.text.is_some() {
                    Some("text")
                } else if c.thinking.is_some() {
                    Some("thinking")
                } else {
                    None
                }
            });
            match kind {
                Some("text") => {
                    if let Some(t) = c.text {
                        text.push_str(&t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = c.thinking {
                        thinking.push_str(&t);
                    }
                }
                _ => {}
            }
        }
        // Fall back to thinking when no text block was produced.
        if text.is_empty() && !thinking.is_empty() {
            text = thinking;
        }
        let usage = self.usage.unwrap_or_default();
        let usage = Usage {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            cache_read: usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation: usage.cache_creation_input_tokens.unwrap_or(0),
        };
        let truncated = matches!(self.stop_reason.as_deref(), Some("max_tokens"));
        Response {
            text,
            finish_reason: self.stop_reason,
            truncated,
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_only_response_is_recovered() {
        let body = OpenCodeMessagesResponseBody {
            content: vec![OpenCodeMessagesContent {
                kind: Some("thinking".into()),
                text: None,
                thinking: Some(r#"{"mode":"fast"}"#.into()),
            }],
            stop_reason: Some("end_turn".into()),
            usage: Some(OpenCodeMessagesUsage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                ..Default::default()
            }),
        };
        let resp = body.into_response();
        assert_eq!(resp.text, r#"{"mode":"fast"}"#);
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 50);
        assert!(!resp.truncated);
    }

    #[test]
    fn text_response_takes_precedence() {
        let body = OpenCodeMessagesResponseBody {
            content: vec![
                OpenCodeMessagesContent {
                    kind: Some("text".into()),
                    text: Some("plain".into()),
                    thinking: None,
                },
                OpenCodeMessagesContent {
                    kind: Some("thinking".into()),
                    text: None,
                    thinking: Some("should be ignored".into()),
                },
            ],
            stop_reason: None,
            usage: None,
        };
        let resp = body.into_response();
        assert_eq!(resp.text, "plain");
    }

    #[test]
    fn empty_response_yields_empty_text() {
        let body = OpenCodeMessagesResponseBody {
            content: vec![],
            stop_reason: None,
            usage: None,
        };
        let resp = body.into_response();
        assert_eq!(resp.text, "");
        assert_eq!(resp.usage.input_tokens, 0);
    }

    #[test]
    fn real_response_with_signature_only_first_block() {
        // Mirror the actual upstream response shape observed on 2026-08-04
        // for qwen3.7-max: the first content block has no `type` field
        // (just `signature` + `thinking`), the second has `type: "text"`.
        let body = r#"{"content":[{"signature":"","thinking":"Thinking Process..."},{"text":"{\"mode\":\"fast\",\"reason\":\"x\",\"sketches\":0,\"proposals\":3,\"judges\":3}","type":"text"}],"stop_reason":"end_turn","usage":{"input_tokens":165,"output_tokens":1760}}"#;
        let parsed: OpenCodeMessagesResponseBody = serde_json::from_str(body).unwrap();
        let resp = parsed.into_response();
        assert_eq!(resp.usage.input_tokens, 165);
        assert!(
            resp.text.contains("\"mode\":\"fast\""),
            "got text: {:?}",
            resp.text
        );
    }

    /// D.29.2: the response cap (`MAX_RESPONSE_BYTES`, 10 MiB)
    /// is enforced at the `send` boundary, not inside
    /// `into_response` (the deserialiser stays total and pure).
    /// The test pins the contract end-to-end: a response whose
    /// joined text exceeds the cap surfaces as
    /// `Error::PayloadTooLarge` so a runaway provider cannot
    /// force the dispatcher to hold a 100 MiB string in memory.
    #[test]
    fn send_rejects_payloads_over_response_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let oversized = "a".repeat(super::super::size_limits::MAX_RESPONSE_BYTES + 1);
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": oversized}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(
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
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Sketch,
                model: "minimax-m3".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: Some(1024),
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let err = p
                .send(&req)
                .await
                .expect_err("oversized body must fail with PayloadTooLarge");
            match err {
                Error::PayloadTooLarge(msg) => {
                    assert!(
                        msg.contains("response"),
                        "label must propagate, got {msg:?}"
                    );
                    assert!(
                        msg.contains(
                            &(super::super::size_limits::MAX_RESPONSE_BYTES + 1).to_string()
                        ),
                        "byte count must propagate, got {msg:?}"
                    );
                }
                other => panic!("expected Error::PayloadTooLarge, got {other:?}"),
            }
        });
    }

    #[test]
    fn messages_url_handles_known_suffixes() {
        // v0.10: the constructor reads `endpoint` (section-level
        // default) verbatim; provide a URL ending in `/v1/messages`
        // so the test exercises the "already-suffixed" branch of
        // `messages_url()`.
        let p = AnthropicCompatProvider::new(
            &ProviderConfig {
                endpoint: Some("https://opencode.ai/zen/go/v1/messages".into()),
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
        assert_eq!(p.messages_url(), "https://opencode.ai/zen/go/v1/messages");
    }

    #[test]
    fn messages_url_handles_messages_suffix() {
        // v0.10: same as above — provide the section-level endpoint
        // so the constructor picks it up. The fixture mirrors a
        // production `[providers.opencode]` block whose `endpoint`
        // field already names the messages path.
        let p = AnthropicCompatProvider::new(
            &ProviderConfig {
                endpoint: Some("https://opencode.ai/zen/go/v1/messages".into()),
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
        assert_eq!(p.messages_url(), "https://opencode.ai/zen/go/v1/messages");
    }

    #[test]
    fn new_accepts_any_kind_v0_10_dispatcher_decides_wire() {
        // v0.10: the kind check is gone — the dispatcher picks
        // the wire format from the URL, not from a `kind` tag.
        // Verify `AnthropicCompatProvider::new` accepts any
        // canonical schema and the model id round-trips through
        // `Provider::model()`. The fixture uses a single
        // `models[]` entry named `x` so the constructor picks the
        // id up verbatim.
        let p = AnthropicCompatProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: vec![crate::config::ModelConfig {
                    id: "x".into(),
                    endpoint: None,
                    max_tokens: None,
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
        .expect("v0.10: kind check removed; new() must accept any kind");
        assert_eq!(p.model(), "x");
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
        }
        let result = AnthropicCompatProvider::from_config(&ProviderConfig {
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
        });
        assert!(matches!(result, Err(Error::InvalidApiKey { .. })));
    }

    /// Per-provider `max_tokens` cap (e.g. DeepSeek-style `8192`) must
    /// clamp the wire body before the upstream sees it. The default
    /// `DEFAULT_MAX_TOKENS` (1,000,000) does not clamp any role under
    /// normal configuration; this test exercises the TOML-override
    /// branch where a smaller cap is set.
    #[test]
    fn send_clamps_max_tokens_to_provider_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(
                &ProviderConfig {
                    models: vec![crate::config::ModelConfig {
                        id: "minimax-m3".into(),
                        endpoint: None,
                        max_tokens: Some(8192),
                    }],
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
            )
            .unwrap();
            assert_eq!(p.provider_max_tokens, Some(8192));
            let req = Request {
                role: crate::llm::Role::Sketch,
                model: "minimax-m3".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: Some(1_000_000),
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, _resp) = p
                .send(&req)
                .await
                .expect("send must succeed against the mock");
            assert_eq!(status, 200);
            let received = server
                .received_requests()
                .await
                .expect("recording must be enabled by default");
            assert_eq!(received.len(), 1, "exactly one request must be sent");
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert_eq!(
                body["max_tokens"],
                serde_json::json!(8192),
                "per-provider cap must clamp 1_000_000 → 8192, got body: {body}"
            );
        });
    }

    /// v0.10 (post Phase 8): the legacy `OPENCODE_MAX_TOKENS_CAP`
    /// global clamp is gone. With no probe result and no operator
    /// override, the wire body carries the request's raw
    /// `max_tokens` unchanged. The auto-probe discovers the real
    /// upstream boundary per `(provider, model)` and caches it
    /// in `max_tokens_auto.toml`; this regression guard pins
    /// that the Anthropic-compat path no longer applies a 16_384
    /// ceiling to OpenCode calls.
    /// (`OPENCODE_GO_MAX_TOKENS_CAP` → `OPENCODE_MAX_TOKENS_CAP`,
    /// renamed in v0.13.x.)
    #[test]
    fn send_does_not_clamp_max_tokens_when_no_probe_or_override() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(
                &ProviderConfig {
                    models: Vec::new(),
                    endpoint: Some(server.uri()),
                    // Deliberately None to exercise the
                    // "TOML override unset, only the hard cap
                    // applies" branch.
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
            let req = Request {
                role: crate::llm::Role::Sketch,
                model: "minimax-m3".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: Some(1_000_000),
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, _resp) = p
                .send(&req)
                .await
                .expect("send must succeed against the mock");
            assert_eq!(status, 200);
            let received = server
                .received_requests()
                .await
                .expect("recording must be enabled by default");
            assert_eq!(received.len(), 1, "exactly one request must be sent");
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert_eq!(
                body["max_tokens"],
                serde_json::json!(1_000_000),
                "Anthropic-compat path must NOT clamp 1_000_000 → 16_384 anymore; \
                 the v0.9 `OPENCODE_GO_MAX_TOKENS_CAP` global cap is gone \
                 (`OPENCODE_GO_MAX_TOKENS_CAP` → `OPENCODE_MAX_TOKENS_CAP`, \
                 renamed in v0.13.x as part of the broader rename). Got body: {body}"
            );
        });
    }

    /// Auto-probe table clamp contract: when
    /// `with_max_tokens_table` attaches a table carrying a
    /// discovered value smaller than the requested `max_tokens`
    /// AND smaller than the documented hard cap, the wire body
    /// must carry the discovered value. Pins the v0.7 precedence
    /// order: `u32::MAX` > operator > table > req.
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
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;

            let transport: Arc<dyn ProbeTransport> = Arc::new(CappedTransport { cap: 5_000 });
            let table = Arc::new(MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR));
            // v0.10: the legacy constructor reads `(name, model)`
            // from `models[0].id` for both slots, so the table key
            // has to match. The dispatched path (`from_resolved`)
            // uses `(section_name, model_id)`; this test exercises
            // the hand-rolled constructor path so we mirror that
            // — provider name == model id == "minimax-m3".
            let discovered = table
                .probe_and_store(
                    "minimax-m3",
                    "minimax-m3",
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

            let p = AnthropicCompatProvider::new(
                &ProviderConfig {
                    // v0.10 canonical schema: one `models[]` entry
                    // whose `id` drives both `name` and `model`
                    // (the legacy constructor reads them from
                    // `models.first()`). `max_tokens: None` leaves
                    // `provider_max_tokens = None` so the operator
                    // cap chain stays at `u32::MAX` and the table
                    // value is the only clamp the wire body sees.
                    models: vec![crate::config::ModelConfig {
                        id: "minimax-m3".into(),
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
                role: crate::llm::Role::Sketch,
                model: "minimax-m3".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: Some(1_000_000),
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
                attachments: vec![],
                tool_choice: None,
            };
            let (status, _resp) = p
                .send(&req)
                .await
                .expect("send must succeed against the mock");
            assert_eq!(status, 200);
            let received = server
                .received_requests()
                .await
                .expect("recording must be enabled by default");
            assert_eq!(received.len(), 1, "exactly one request must be sent");
            let body: serde_json::Value = serde_json::from_slice(&received[0].body)
                .expect("mock server received a JSON body");
            assert_eq!(
                body["max_tokens"],
                serde_json::json!(discovered),
                "wire body must carry the table-resolved value ({discovered}), got body: {body}"
            );
        });
    }
}
