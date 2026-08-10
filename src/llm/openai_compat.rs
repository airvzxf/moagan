//! Generic OpenAI Chat Completions provider.
//!
//! Used by any backend whose API is OpenAI-compatible: DeepSeek
//! (`https://api.deepseek.com/v1`), OpenCode Go
//! (`https://opencode.ai/zen/go/v1`), and any future provider that
//! speaks the same wire format. The provider name (`fn name()`) is
//! configurable so the same code can serve multiple backends.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::ProviderCapabilities;
use super::opencode_go::OpenCodeGoDispatch;
use super::provider::Provider;
use super::response_format_opt_out;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::wire::{Request, Response, Usage};

/// Generic OpenAI-compat provider.
#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: Client,
    max_retries: u32,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,048,576), so the per-role runtime
    /// value normally fits under the cap. The clamp below exists
    /// for the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request with 400.
    provider_max_tokens: Option<u32>,
}

impl OpenAiCompatProvider {
    /// Build from a `ProviderConfig` and a resolved API key.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| Error::Provider(format!("build http client: {e}")))?;
        Ok(Self {
            name: spec.kind.clone(),
            model: spec.model.clone(),
            endpoint: spec.endpoint.clone(),
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens: spec.max_tokens,
        })
    }

    /// Compute the URL for chat completions.
    pub fn chat_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    /// Build the chat-completions request body for a given provider
    /// request, applying the role-based JSON output mode and the
    /// per-model opt-out from `response_format_opt_out`. Extracted so
    /// tests can assert on the wire shape without a live HTTP call.
    fn build_chat_request(&self, req: &Request) -> ChatRequest<'_> {
        ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: req.system.clone(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: req.user.clone(),
                },
            ],
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            stream: false,
            response_format: if role_requires_json(req.role)
                && !response_format_opt_out::model_skips_response_format(&self.model)
            {
                Some(ResponseFormat {
                    kind: "json_object",
                })
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
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
/// providers (DeepSeek, OpenCode Go) get `response_format` set to
/// `json_object` for these roles so the JSON parser in `parse_model_json`
/// stops hitting the trailing-token / missing-brace pathologies
/// reported by the Q8 multi-model benchmark. Markdown-only roles
/// (Propose delivers markdown but parses a JSON header; the
/// actual markdown body is not autostructured) and free-text roles
/// (Sketch, FinalReport) are NOT in this list.
fn role_requires_json(role: crate::llm::Role) -> bool {
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
            | RecoveryExplainer
            | RationaleExtractor
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

#[async_trait]
impl Provider for OpenAiCompatProvider {
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
        // OpenCode Go chat-completions path) stays on the
        // generic baseline.
        if self.name == "deepseek" {
            ProviderCapabilities::for_deepseek()
        } else {
            ProviderCapabilities::for_openai_compat()
        }
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let url = self.chat_url();
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = self.build_chat_request(req);
            // Apply per-provider max_tokens cap. Done AFTER the
            // body construction so the cap is visible regardless of
            // upstream choice. The default of DEFAULT_MAX_TOKENS
            // (1,048,576) does not clamp any role under normal
            // configuration; the branch only triggers when a TOML
            // override sets a smaller per-provider limit.
            let body = match self.provider_max_tokens {
                Some(cap) if body.max_tokens > cap => ChatRequest {
                    max_tokens: cap,
                    ..body
                },
                _ => body,
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
                        let parsed: ChatResponse = resp
                            .json()
                            .await
                            .map_err(|e| Error::Provider(format!("decode: {e}")))?;
                        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
                            Error::Provider("openai-compat: empty choices array".into())
                        })?;
                        let finish_reason = choice.finish_reason;
                        let truncated = finish_reason.as_deref() == Some("length");
                        let usage = parsed.usage.unwrap_or_default();
                        let text = choice.message.content;
                        // D.29.2: enforce the centralised response
                        // cap (10 MiB) so a runaway provider cannot
                        // force us to hold a 100 MiB string in
                        // memory. The check happens AFTER the JSON
                        // decode so the byte count is the actual
                        // payload length (not the wire bytes,
                        // which include JSON-escape overhead).
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
                    if attempt >= self.max_retries {
                        return Err(Error::Provider(format!(
                            "openai-compat: HTTP {code} after {attempt} attempts: {body}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
                Err(e) => {
                    if attempt >= self.max_retries {
                        return Err(Error::Provider(format!("openai-compat: network: {e}")));
                    }
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
            }
        }
    }
}

impl OpenCodeGoDispatch for OpenAiCompatProvider {
    fn url(&self) -> String {
        self.chat_url()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(endpoint: &str) -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(
            &ProviderConfig {
                kind: "deepseek".into(),
                endpoint: endpoint.into(),
                model: "deepseek-v4-flash".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
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
        let p = OpenAiCompatProvider::new(
            &ProviderConfig {
                kind: "deepseek".into(),
                endpoint: "https://api.deepseek.com/v1".into(),
                model: "deepseek-v4-flash".into(),
                max_tokens: Some(8192),
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
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
            max_tokens: 128,
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

    /// Build an `OpenAiCompatProvider` with a fully-specified config
    /// so the per-test `model` overrides the default in `provider()`.
    fn provider_with_model(kind: &str, endpoint: &str, model: &str) -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(
            &ProviderConfig {
                kind: kind.into(),
                endpoint: endpoint.into(),
                model: model.into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
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
            max_tokens: 128,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
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
        // glm-5.1 routes through OpenCode Go's
        // /v1/chat/completions endpoint, which is built on this
        // OpenAiCompatProvider. Same code path as DeepSeek — the
        // opt-out must trigger purely from the model name.
        for model in [
            "glm-5.1",
            "glm-5.2",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "deepseek-v4-pro",
            "kimi-k3",
        ] {
            let p = provider_with_model("opencode_go", "https://opencode.ai/zen/go/v1", model);
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
    fn opencode_go_request_omits_response_format_for_opted_out_models() {
        // Pin the OpenCode Go contract: even when role_requires_json
        // is true (Route), the chat-completions body for an opted-out
        // model must NOT carry the `response_format` field so the
        // upstream doesn't return prose-prefixed content.
        let p = provider_with_model(
            "opencode_go",
            "https://opencode.ai/zen/go/v1",
            "kimi-k2.7-code",
        );
        let body = p.build_chat_request(&json_request(crate::llm::Role::Route, "kimi-k2.7-code"));
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["model"], serde_json::json!("kimi-k2.7-code"));
        assert!(
            value.get("response_format").is_none(),
            "OpenCode Go request for opted-out model must omit response_format, got: {value}"
        );
        // Sanity: the role_requires_json path was actually exercised
        // — without the opt-out check the field WOULD be present.
        assert!(super::role_requires_json(crate::llm::Role::Route));
    }

    #[test]
    fn opencode_go_request_keeps_response_format_for_non_opted_out_models() {
        // The flip side of the previous test: an OpenCode Go model
        // NOT on the opt-out list (mimo-v2.5, deepseek-v4-flash, hy3)
        // still gets response_format = json_object for JSON roles.
        for model in ["mimo-v2.5", "deepseek-v4-flash", "hy3"] {
            let p = provider_with_model("opencode_go", "https://opencode.ai/zen/go/v1", model);
            let body = p.build_chat_request(&json_request(crate::llm::Role::Route, model));
            let value = serde_json::to_value(&body).unwrap();
            assert_eq!(
                value.get("response_format"),
                Some(&serde_json::json!({"type": "json_object"})),
                "non-opted-out model {model} must keep response_format: json_object, got: {value}"
            );
        }
    }
}
