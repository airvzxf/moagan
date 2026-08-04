//! `opencode_go_responses` provider — OpenAI Responses API at
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

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::provider::Provider;
use super::wire::{Request, Response, Usage};

/// OpenCode Go provider routed through the OpenAI Responses API.
#[derive(Debug, Clone)]
pub struct OpenCodeGoResponsesProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: reqwest::Client,
    max_retries: u32,
}

impl OpenCodeGoResponsesProvider {
    /// Build from a provider config and a resolved API key.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "opencode_go" {
            return Err(Error::InvalidArgs(format!(
                "opencode_go_responses provider got kind '{}'",
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
}

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    stream: bool,
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
        .map_err(|e| Error::Provider(format!("build reqwest client: {e}")))
}

#[async_trait]
impl Provider for OpenCodeGoResponsesProvider {
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
        let url = self.responses_url();
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let body = ResponsesRequest {
                model: &self.model,
                instructions: Some(&req.system),
                input: &req.user,
                max_tokens: req.max_tokens,
                temperature: req.temperature,
                top_p: req.top_p,
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
                        let parsed: ResponsesBody = resp
                            .json()
                            .await
                            .map_err(|e| Error::Provider(format!("decode: {e}")))?;
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
                        401 | 403 => Error::InvalidApiKey(format!("http {status_code}: {body}")),
                        429 => Error::PlanExhausted(format!("http {status_code}: {body}")),
                        408 | 504 | 524 => Error::Timeout(format!("http {status_code}: {body}")),
                        _ => Error::Provider(format!("http {status_code}: {body}")),
                    };
                    let retryable = matches!(
                        err,
                        Error::Timeout(_) | Error::PlanExhausted(_) | Error::Provider(_)
                    );
                    if !retryable || attempt >= self.max_retries {
                        return Err(err);
                    }
                    Self::sleep_with_jitter(attempt, None).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_url_handles_known_suffixes() {
        let p = OpenCodeGoResponsesProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "gpt-5.6-luna".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p.responses_url(),
            "https://opencode.ai/zen/go/v1/responses"
        );
    }

    #[test]
    fn responses_url_handles_responses_suffix() {
        let p = OpenCodeGoResponsesProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1/responses".into(),
                model: "gpt-5.6-luna".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p.responses_url(),
            "https://opencode.ai/zen/go/v1/responses"
        );
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
        }
        let result = OpenCodeGoResponsesProvider::from_config(&ProviderConfig {
            kind: "opencode_go".into(),
            endpoint: "https://opencode.ai/zen/go/v1".into(),
            model: "gpt-5.6-luna".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        });
        assert!(matches!(result, Err(Error::InvalidApiKey(_))));
    }
}
