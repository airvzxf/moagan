//! `opencode_go_anthropic` provider — Anthropic-compatible wire format
//! served by OpenCode Go at `https://opencode.ai/zen/go/v1/messages`.
//!
//! Models served at this endpoint (per the operator's 2026-08-04 model
//! roster) are:
//!
//! - `minimax-m3`, `minimax-m2.7`, `minimax-m2.5` (Anthropic SDK)
//! - `qwen3.8-max`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus` (Anthropic SDK)
//!
//! The wire format is identical to the `minimax` provider so the
//! request body (MessagesRequestBody) and response decoder
//! (MessagesResponseBody) are shared via `super::http`. The differences
//! are limited to the `name` field, the BLOCKED_MODELS gate, and the
//! API key env var (`OPENCODE_GO_API_KEY`).
//!
//! Per-model temperature overrides (Fix #5, B + A) live in
//! `super::opencode_go::MODEL_TEMPERATURE_OVERRIDES`. Unknown models
//! fall back to the per-role temperature; if the upstream rejects
//! with a 400, the retry path in `phase.rs::call_with_retry_parse`
//! surfaces the error so the operator can extend the map.

use async_trait::async_trait;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::{OPENCODE_GO_MAX_TOKENS_CAP, ProviderCapabilities};
use super::http::{body_from_request, build_client, build_headers, classify_status, retry_after};
use super::opencode_go::OpenCodeGoDispatch;
use super::provider::Provider;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::wire::{Request, Response, Usage};

/// OpenCode Go provider routed through the Anthropic-compatible
/// `/v1/messages` endpoint. Distinct from the `minimax` provider so
/// that future behavior changes (e.g. response_format, custom
/// headers) don't leak across backends.
#[derive(Debug, Clone)]
pub struct OpenCodeGoAnthropicProvider {
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
    /// request with 400.
    provider_max_tokens: Option<u32>,
}

impl OpenCodeGoAnthropicProvider {
    /// Build from a provider config and a resolved API key. The
    /// `spec.kind` must be `"opencode_go"` and the endpoint must end
    /// in `/v1/messages` (or `/v1` so we can append).
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "opencode_go" {
            return Err(Error::InvalidArgs(format!(
                "opencode_go_anthropic provider got kind '{}'",
                spec.kind
            )));
        }
        let client = build_client()?;
        Ok(Self {
            name: "opencode_go".to_owned(),
            model: spec.model.clone(),
            endpoint: spec.endpoint.clone(),
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens: spec.max_tokens,
        })
    }

    /// Build from config using `OPENCODE_GO_API_KEY`.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = std::env::var("OPENCODE_GO_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "OPENCODE_GO_API_KEY not set; provide via env, --api-key, or config file"
                        .into(),
                )
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// Compute the URL for the messages endpoint.
    pub fn messages_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
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
}

#[async_trait]
impl Provider for OpenCodeGoAnthropicProvider {
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
        ProviderCapabilities::for_opencode_go_anthropic()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let url = self.messages_url();
        // Apply two-layer max_tokens cap (mirrors OpenAiCompatProvider).
        // Layer 1: per-provider `ProviderConfig::max_tokens` (TOML
        // override). Layer 2: the OpenCode Go hard ceiling
        // (`OPENCODE_GO_MAX_TOKENS_CAP = 16_384`), which is below the
        // upstream's documented maximum of 393216 but matches the
        // smallest cap observed across the 2026-08-04 model roster.
        // Without this the upstream returns HTTP 400 when a per-role
        // ceiling (e.g. DEFAULT_MAX_TOKENS = 1_000_000) flows through.
        let mut req = req.clone();
        let cap = self
            .provider_max_tokens
            .unwrap_or(u32::MAX)
            .min(OPENCODE_GO_MAX_TOKENS_CAP);
        req.max_tokens = req.max_tokens.min(cap);
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
                        let parsed: OpenCodeGoMessagesResponseBody = resp
                            .json()
                            .await
                            .map_err(|e| Error::Provider(format!("decode response: {e}")))?;
                        tracing::debug!(
                            provider = self.name,
                            attempt,
                            stage = "http.body.decoded",
                            status = status_code,
                            elapsed_ms = decode_started.elapsed().as_millis(),
                            "Provider HTTP stage"
                        );
                        let resp = parsed.into_response();
                        // D.29.2: enforce the centralised response
                        // cap (10 MiB) on the decoded text. Done
                        // here (not inside `into_response`) so the
                        // helper signature stays total and the
                        // tests for the deserialiser don't need to
                        // stand up the cap machinery.
                        check_size("response", resp.text.len(), MAX_RESPONSE_BYTES)?;
                        return Ok((status_code, resp));
                    }
                    let body = resp.text().await.unwrap_or_default();
                    let err = classify_status(status, &body);
                    let retryable = matches!(
                        err,
                        Error::Timeout(_) | Error::PlanExhausted(_) | Error::Provider(_)
                    );
                    if !retryable || attempt >= self.max_retries {
                        return Err(err);
                    }
                    Self::sleep_with_jitter(attempt, retry_after).await;
                }
                Err(e) => {
                    if attempt >= self.max_retries {
                        return Err(Error::Provider(format!("network: {e}")));
                    }
                    Self::sleep_with_jitter(attempt, None).await;
                }
            }
        }
    }
}

impl OpenCodeGoDispatch for OpenCodeGoAnthropicProvider {
    fn url(&self) -> String {
        self.messages_url()
    }
}

/// OpenCode Go Anthropic-compat response body. Extends the canonical
/// shape with a `thinking` block fallback: some OpenCode Go models
/// (qwen3.x, plus future additions) return the response content inside
/// a `thinking` block instead of a `text` block when the prompt
/// produces a planning pass. The shared `MessagesResponseBody` in
/// `super::http` ignores `thinking` blocks; here we collect both and
/// prepend the `text` block(s) first, then append the `thinking`
/// block(s) as a fallback so the JSON parser has something to chew on.
#[derive(Debug, Deserialize)]
struct OpenCodeGoMessagesResponseBody {
    content: Vec<OpenCodeGoMessagesContent>,
    stop_reason: Option<String>,
    usage: Option<OpenCodeGoMessagesUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeGoMessagesContent {
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
struct OpenCodeGoMessagesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl OpenCodeGoMessagesResponseBody {
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
        let body = OpenCodeGoMessagesResponseBody {
            content: vec![OpenCodeGoMessagesContent {
                kind: Some("thinking".into()),
                text: None,
                thinking: Some(r#"{"mode":"fast"}"#.into()),
            }],
            stop_reason: Some("end_turn".into()),
            usage: Some(OpenCodeGoMessagesUsage {
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
        let body = OpenCodeGoMessagesResponseBody {
            content: vec![
                OpenCodeGoMessagesContent {
                    kind: Some("text".into()),
                    text: Some("plain".into()),
                    thinking: None,
                },
                OpenCodeGoMessagesContent {
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
        let body = OpenCodeGoMessagesResponseBody {
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
        let parsed: OpenCodeGoMessagesResponseBody = serde_json::from_str(body).unwrap();
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
            let p = OpenCodeGoAnthropicProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: server.uri(),
                    model: "minimax-m3".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Sketch,
                model: "minimax-m3".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1024,
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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
        let p = OpenCodeGoAnthropicProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "qwen3.7-max".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
                omit_max_tokens: false,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(p.messages_url(), "https://opencode.ai/zen/go/v1/messages");
    }

    #[test]
    fn messages_url_handles_messages_suffix() {
        let p = OpenCodeGoAnthropicProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1/messages".into(),
                model: "minimax-m3".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
                omit_max_tokens: false,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(p.messages_url(), "https://opencode.ai/zen/go/v1/messages");
    }

    #[test]
    fn from_config_errors_when_kind_mismatch() {
        let result = OpenCodeGoAnthropicProvider::new(
            &ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "x".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
                omit_max_tokens: false,
            },
            SecretString::new("dummy".into()),
        );
        assert!(matches!(result, Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
        }
        let result = OpenCodeGoAnthropicProvider::from_config(&ProviderConfig {
            kind: "opencode_go".into(),
            endpoint: "https://opencode.ai/zen/go/v1".into(),
            model: "x".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
            omit_max_tokens: false,
        });
        assert!(matches!(result, Err(Error::InvalidApiKey(_))));
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
            let p = OpenCodeGoAnthropicProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: server.uri(),
                    model: "minimax-m3".into(),
                    max_tokens: Some(8192),
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
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
                max_tokens: 1_000_000,
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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

    /// OpenCode Go hard cap (`OPENCODE_GO_MAX_TOKENS_CAP = 16_384`)
    /// must clamp the wire body BEFORE the upstream sees it. This
    /// test is the regression guard for the original bug: when the
    /// per-provider `max_tokens` is unset (or explicitly raised
    /// above the hard cap) the upstream still receives 16_384 and
    /// never returns HTTP 400.
    #[test]
    fn send_clamps_max_tokens_to_opencode_go_hard_cap() {
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
            let p = OpenCodeGoAnthropicProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: server.uri(),
                    model: "minimax-m3".into(),
                    // Deliberately None to exercise the
                    // "TOML override unset, only the hard cap
                    // applies" branch.
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Sketch,
                model: "minimax-m3".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: Some(0.7),
                top_p: Some(0.95),
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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
                serde_json::json!(super::super::capabilities::OPENCODE_GO_MAX_TOKENS_CAP),
                "opencode_go hard cap must clamp 1_000_000 → 16_384, got body: {body}"
            );
        });
    }
}
