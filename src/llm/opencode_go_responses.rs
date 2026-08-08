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

use super::capabilities::ProviderCapabilities;
use super::opencode_go::OpenCodeGoDispatch;
use super::provider::Provider;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::sse_parser::{SseError, SseParser};
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
    pub fn responses_url(&self) -> String {
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

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::for_opencode_go_responses()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let url = self.responses_url();
        if req.stream {
            return self.send_streaming(req, &url).await;
        }
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
                        // D.29.2: enforce the centralised response
                        // cap (10 MiB) before constructing the
                        // Response. Done on the accumulated text so
                        // the byte count is the actual payload
                        // length the pipeline will see.
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

/// Build the wire-level request body used by `send`.
fn build_responses_body<'a>(
    req: &'a Request,
    model: &'a str,
    stream: bool,
) -> ResponsesRequest<'a> {
    ResponsesRequest {
        model,
        instructions: Some(&req.system),
        input: &req.user,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stream,
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
                    SseError::Io(err) => Error::Provider(format!("sse io: {err}")),
                    SseError::Parse(err) => Error::Provider(format!("sse parse: {err}")),
                });
            }
        }
    }
    Ok((text, usage))
}

impl OpenCodeGoResponsesProvider {
    /// Streaming variant of [`Provider::send`]: sets
    /// `stream=true` on the wire body, reads the entire SSE
    /// response, and returns a single aggregated `Response` with
    /// the joined text and the terminal usage block.
    async fn send_streaming(&self, req: &Request, url: &str) -> Result<(u16, Response)> {
        let body = build_responses_body(req, &self.model, true);
        let request_started = std::time::Instant::now();
        let resp = self
            .client
            .post(url)
            .bearer_auth(self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("network: {e}")))?;
        let status = resp.status();
        let status_code = status.as_u16();
        if !status.is_success() {
            let raw = resp.text().await.unwrap_or_default();
            let err = match status_code {
                401 | 403 => Error::InvalidApiKey(format!("http {status_code}: {raw}")),
                429 => Error::PlanExhausted(format!("http {status_code}: {raw}")),
                408 | 504 | 524 => Error::Timeout(format!("http {status_code}: {raw}")),
                _ => Error::Provider(format!("http {status_code}: {raw}")),
            };
            return Err(err);
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Provider(format!("stream body read: {e}")))?;
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

impl OpenCodeGoDispatch for OpenCodeGoResponsesProvider {
    fn url(&self) -> String {
        self.responses_url()
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
        assert_eq!(p.responses_url(), "https://opencode.ai/zen/go/v1/responses");
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
        assert_eq!(p.responses_url(), "https://opencode.ai/zen/go/v1/responses");
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

    #[test]
    fn opencode_go_responses_streaming_consumes_sse_data_lines() {
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 256,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
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
    fn opencode_go_responses_streaming_returns_error_on_mid_stream_failure() {
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 64,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
            };
            let err = p.send(&req).await.unwrap_err();
            match err {
                Error::Provider(msg) => assert!(
                    msg.contains("sse parse"),
                    "expected sse parse error, got {msg:?}"
                ),
                other => panic!("expected Error::Provider, got {other:?}"),
            }
        });
    }

    #[test]
    fn opencode_go_responses_non_streaming_still_works() {
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 64,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
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
}
