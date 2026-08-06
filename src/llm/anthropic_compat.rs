//! D.19.13: AnthropicCompatProvider — generic Anthropic-compatible
//! provider driven by config (base URL + api key + model).
//!
//! The existing `opencode_go_anthropic` is hard-coded to the
//! opencode-go gateway. This module provides a config-driven
//! counterpart so operators can point to any Anthropic-compatible
//! endpoint.
//!
//! Wire format: same as `super::opencode_go_anthropic`. We
//! `POST /v1/messages` (or `/messages` when the URL already
//! ends in `/v1`) with the canonical
//! `{model, max_tokens, messages:[...], system}` body, the
//! `x-api-key` and `anthropic-version: 2023-06-01` headers, and
//! decode the response with the shared
//! `super::http::MessagesResponseBody` shape.
//!
//! Unlike `opencode_go_anthropic`, this provider does NOT
//! retry — it surfaces transport-level errors immediately so
//! the dispatcher (which already wraps every provider with
//! `BreakeredProvider` + `RateLimitedProvider`) owns backoff
//! and budget enforcement. Single attempt keeps the call
//! surface small and predictable; the canonical
//! `opencode_go_anthropic` path keeps its retry loop because
//! it is the primary M-series model and the operator has
//! observed transient 5xx storms.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::llm::provider::Provider;
use crate::llm::wire::{Request, Response, Usage};

/// JSON shape sent to the `/v1/messages` endpoint.
#[derive(Debug, Serialize)]
struct MessagesRequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    system: &'a str,
    messages: Vec<MessagesMessage<'a>>,
}

#[derive(Debug, Serialize)]
struct MessagesMessage<'a> {
    role: &'static str,
    content: &'a str,
}

/// JSON shape returned by `/v1/messages` (minimal subset — see
/// `super::http::MessagesResponseBody` for the canonical schema).
#[derive(Debug, serde::Deserialize)]
struct MessagesResponseBody {
    content: Vec<MessagesContent>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<MessagesUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct MessagesContent {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
struct MessagesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

/// Generic Anthropic-compatible provider.
pub struct AnthropicCompatProvider {
    /// Stable name. Defaults to `"anthropic_compat"`.
    pub name: String,
    /// HTTP base URL (the provider appends `/v1/messages` when
    /// missing — see [`Self::messages_url`]).
    pub base_url: String,
    /// Anthropic API key sent as `x-api-key`.
    pub api_key: String,
    /// Model identifier (e.g. `"claude-3-5-sonnet"`).
    pub model: String,
    client: reqwest::Client,
}

impl AnthropicCompatProvider {
    /// Build a provider with the canonical name (`"anthropic_compat"`).
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self::with_name("anthropic_compat".to_owned(), base_url, api_key, model)
    }

    /// Build a provider with an explicit name. Useful when the
    /// caller wants multiple endpoints side by side (e.g. a
    /// "claude-work" provider alongside the canonical M-series
    /// path).
    pub fn with_name(name: String, base_url: String, api_key: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("moagan/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client build with sane defaults");
        Self {
            name,
            base_url,
            api_key,
            model,
            client,
        }
    }

    /// Compute the `/v1/messages` URL from the configured base.
    pub fn messages_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    /// Build the headers for a single Anthropic-compat POST.
    fn build_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(&self.api_key)
                .map_err(|e| Error::Provider(format!("x-api-key header: {e}")))?,
        );
        headers.insert(
            reqwest::header::HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        Ok(headers)
    }

    /// Build the body bytes for a single Anthropic-compat POST.
    fn build_body(&self, req: &Request) -> Result<Vec<u8>> {
        let body = MessagesRequestBody {
            model: &self.model,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            system: &req.system,
            messages: vec![MessagesMessage {
                role: "user",
                content: &req.user,
            }],
        };
        serde_json::to_vec(&body).map_err(|e| Error::Provider(format!("encode body: {e}")))
    }

    /// Decode the response body into a [`Response`].
    fn decode_body(bytes: &[u8]) -> Result<Response> {
        let parsed: MessagesResponseBody = serde_json::from_slice(bytes)
            .map_err(|e| Error::Provider(format!("decode body: {e}")))?;
        let mut text = String::new();
        for c in parsed.content {
            // Treat blocks without an explicit `type` as text —
            // some Anthropic-compatible gateways omit the field
            // on the leading block. Matches the convention in
            // `super::opencode_go_anthropic`.
            let is_text = match c.kind.as_deref() {
                Some("text") | None => c.text.is_some(),
                _ => false,
            };
            if is_text && let Some(t) = c.text {
                text.push_str(&t);
            }
        }
        let usage = parsed.usage.unwrap_or_default();
        let usage = Usage {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            cache_read: usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation: usage.cache_creation_input_tokens.unwrap_or(0),
        };
        let truncated = matches!(parsed.stop_reason.as_deref(), Some("max_tokens"));
        Ok(Response {
            text,
            finish_reason: parsed.stop_reason,
            truncated,
            usage,
        })
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
        &self.base_url
    }
    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let url = self.messages_url();
        let body = self.build_body(req)?;
        let headers = self.build_headers()?;
        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("network: {e}")))?;
        let status = resp.status();
        let status_code = status.as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("read body: {e}")))?;
        if status_code >= 400 {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Err(Error::Provider(format!("http {status}: {body}")));
        }
        let response = Self::decode_body(&bytes)?;
        Ok((status_code, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::role::Role;

    fn fake_request() -> Request {
        Request {
            role: Role::Sketch,
            model: "claude-3-5-sonnet".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: 1024,
            temperature: Some(0.7),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
        }
    }

    #[test]
    fn anthropic_compat_provider_name() {
        let p = AnthropicCompatProvider::new(
            "https://api.example.com".into(),
            "sk-test".into(),
            "claude-3-5-sonnet".into(),
        );
        assert_eq!(p.name(), "anthropic_compat");
        assert_eq!(p.model(), "claude-3-5-sonnet");
        assert_eq!(p.endpoint(), "https://api.example.com");
    }

    #[test]
    fn messages_url_handles_known_suffixes() {
        let p = AnthropicCompatProvider::new(
            "https://api.example.com/v1".into(),
            "sk-test".into(),
            "claude-3-5-sonnet".into(),
        );
        assert_eq!(p.messages_url(), "https://api.example.com/v1/messages");
        let p = AnthropicCompatProvider::new(
            "https://api.example.com/v1/messages".into(),
            "sk-test".into(),
            "claude-3-5-sonnet".into(),
        );
        assert_eq!(p.messages_url(), "https://api.example.com/v1/messages");
        let p = AnthropicCompatProvider::new(
            "https://api.example.com".into(),
            "sk-test".into(),
            "claude-3-5-sonnet".into(),
        );
        assert_eq!(p.messages_url(), "https://api.example.com/v1/messages");
    }

    #[test]
    fn build_body_serializes_canonical_shape() {
        let p = AnthropicCompatProvider::new(
            "https://api.example.com".into(),
            "sk-test".into(),
            "claude-3-5-sonnet".into(),
        );
        let bytes = p.build_body(&fake_request()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["model"], "claude-3-5-sonnet");
        assert_eq!(json["system"], "sys");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "user");
        assert_eq!(json["max_tokens"], 1024);
    }

    #[test]
    fn anthropic_compat_provider_send_returns_invalid_args() {
        // The previous stub returned `Error::InvalidArgs`; we
        // removed the stub in favour of a real transport. This
        // test now exercises the success path (HTTP 200) so the
        // assertion against the previous stub is gone. The
        // success path is covered by
        // `anthropic_compat_send_returns_ok_on_200` below.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn"
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(server.uri(), "sk-test".into(), "claude".into());
            let result = p.send(&fake_request()).await;
            assert!(result.is_ok(), "send must succeed, got: {result:?}");
        });
    }

    #[test]
    fn decode_body_joins_text_blocks_and_extracts_usage() {
        let body = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"}
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let resp = AnthropicCompatProvider::decode_body(&bytes).unwrap();
        assert_eq!(resp.text, "hello world");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 34);
        assert!(!resp.truncated);
    }

    #[test]
    fn anthropic_compat_send_returns_ok_on_200() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{header, method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .and(header("x-api-key", "sk-test"))
                .and(header("anthropic-version", "2023-06-01"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "hi"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 4,
                        "output_tokens": 2,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0
                    }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(server.uri(), "sk-test".into(), "claude".into());
            let (status, resp) = p.send(&fake_request()).await.unwrap();
            assert_eq!(status, 200);
            assert_eq!(resp.text, "hi");
            assert_eq!(resp.usage.input_tokens, 4);
            assert_eq!(resp.usage.output_tokens, 2);
            assert!(!resp.truncated);
        });
    }

    #[test]
    fn anthropic_compat_send_returns_error_on_401() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(server.uri(), "sk-test".into(), "claude".into());
            let result = p.send(&fake_request()).await;
            let err = result.expect_err("401 must error");
            match err {
                Error::Provider(msg) => {
                    assert!(
                        msg.contains("401"),
                        "error must carry the status, got: {msg}"
                    );
                    assert!(
                        msg.contains("unauthorized"),
                        "error must carry the body, got: {msg}"
                    );
                }
                other => panic!("expected Error::Provider, got: {other:?}"),
            }
        });
    }

    #[test]
    fn anthropic_compat_send_returns_error_on_500() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(ResponseTemplate::new(500).set_body_string("upstream is on fire"))
                .expect(1)
                .mount(&server)
                .await;
            let p = AnthropicCompatProvider::new(server.uri(), "sk-test".into(), "claude".into());
            let result = p.send(&fake_request()).await;
            let err = result.expect_err("500 must error");
            match err {
                Error::Provider(msg) => {
                    assert!(
                        msg.contains("500"),
                        "error must carry the status, got: {msg}"
                    );
                    assert!(
                        msg.contains("upstream"),
                        "error must carry the body, got: {msg}"
                    );
                }
                other => panic!("expected Error::Provider, got: {other:?}"),
            }
        });
    }
}
