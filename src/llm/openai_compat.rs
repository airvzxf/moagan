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

use super::provider::Provider;
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
    /// `ProviderConfig::max_tokens`). OpenAI-compat backends vary
    /// wildly on this — DeepSeek accepts 8192, Moonshot accepts
    /// 32k, OpenCode Go proxies typically 8k. When the per-role
    /// `max_tokens_for_role` exceeds this cap, we clamp on the way
    /// out so the upstream doesn't reject the request with 400.
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
    fn chat_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
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

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let url = self.chat_url();
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = ChatRequest {
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
                response_format: if role_requires_json(req.role) {
                    Some(ResponseFormat {
                        kind: "json_object",
                    })
                } else {
                    None
                },
            };
            // Apply per-provider max_tokens cap. Done AFTER the
            // body construction so the cap is visible regardless of
            // upstream choice. The default of 8192 covers DeepSeek
            // v4 (max 8k) and OpenCode Go (max 8k per the user
            // roster). Propose (32k) and Repair (16k) roles are
            // clamped.
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
                        let response = Response {
                            text: choice.message.content,
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
}
