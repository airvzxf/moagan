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

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::{OPENCODE_GO_MAX_TOKENS_CAP, ProviderCapabilities};
use super::openai_compat::role_requires_json;
use super::opencode_go::OpenCodeGoDispatch;
use super::probe_table::MaxTokensTable;
use super::provider::Provider;
use super::response_format_opt_out::model_skips_response_format;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::sse_parser::{SseError, SseParser};
use super::wire::{Request, Response, Usage};

/// OpenCode Go provider routed through the OpenAI Responses API.
#[derive(Clone)]
pub struct OpenCodeGoResponsesProvider {
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
            provider_max_tokens: spec.max_tokens,
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
    /// OpenAI-style JSON output mode. Mirrors the OpenAI-compat
    /// gating: omitted for free-text roles (`Sketch`, `FinalReport`,
    /// etc.) and for models on the `response_format_opt_out` list.
    /// `Capabilities::for_opencode_go_responses` advertises
    /// `supports_response_format: true` because the Responses API
    /// still honours the field, so a JSON role on a non-opted-out
    /// model must send it.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponsesResponseFormat>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Compute the gate the OpenAI-compat path uses to decide whether
/// to send `response_format: { type: "json_object" }`. Kept local
/// so the wire builder can stay a free function and so the
/// `#[cfg(test)]` block can drive it directly.
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
        .map_err(|e| Error::Provider(format!("build reqwest client: {e}")))
}

/// Custom Debug that masks `max_tokens_table` — `MaxTokensTable`
/// does not implement `Debug` (that lives in `probe_table.rs`,
/// outside this provider's owned files). The table is a shared
/// `Arc`, so emitting `<shared>` keeps the dump informative.
impl std::fmt::Debug for OpenCodeGoResponsesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenCodeGoResponsesProvider")
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
        // Apply three-layer max_tokens cap (mirrors
        // OpenAiCompatProvider and MinimaxProvider). Highest priority
        // (smallest wins) to lowest:
        //   1. `OPENCODE_GO_MAX_TOKENS_CAP = 16_384` — the documented
        //      hard ceiling for the 2026-08-04 roster (kimi-k* /
        //      gpt-5.6-luna accept at most 16_384, below the upstream's
        //      393216 documented max).
        //   2. `ProviderConfig::max_tokens` — operator TOML override.
        //   3. `MaxTokensTable::resolve_cached` — auto-probed
        //      per-(provider, model) value; primary source of truth
        //      when present.
        let mut req = req.clone();
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        let cap = operator_cap.min(table_cap).min(OPENCODE_GO_MAX_TOKENS_CAP);
        req.max_tokens = req.max_tokens.min(cap);
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
                response_format: if wants_response_format(req.role, &self.model) {
                    Some(ResponsesResponseFormat {
                        kind: "json_object",
                    })
                } else {
                    None
                },
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
        response_format: if wants_response_format(req.role, model) {
            Some(ResponsesResponseFormat {
                kind: "json_object",
            })
        } else {
            None
        },
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
        let cap = operator_cap.min(table_cap).min(OPENCODE_GO_MAX_TOKENS_CAP);
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
                omit_max_tokens: false,
                max_token_auto: None,
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
        let p = OpenCodeGoResponsesProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1/responses".into(),
                model: "gpt-5.6-luna".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
                omit_max_tokens: false,
                max_token_auto: None,
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
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
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
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
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
                extra_messages: vec![],
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
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
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
                extra_messages: vec![],
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
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
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
                extra_messages: vec![],
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: Some(8192),
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: Some(8192),
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
                extra_messages: vec![],
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
                    "model": "gpt-5.6-luna",
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: Some(8192),
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: true,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                // 1024 is well below OPENCODE_GO_MAX_TOKENS_CAP
                // (16_384) so the cap does not engage and the
                // assertion about the field surviving the wire
                // builder stays meaningful.
                max_tokens: 1024,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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

    /// OpenCode Go hard cap (`OPENCODE_GO_MAX_TOKENS_CAP = 16_384`)
    /// must clamp the wire body BEFORE the upstream sees it. The
    /// Responses wire exposes a different shape than chat-completions
    /// (no `omit_max_tokens` flag here — the test disables it
    /// explicitly so the field stays present after the clamp) so
    /// this regression guard parallels the chat-completions one.
    #[test]
    fn send_clamps_max_tokens_to_opencode_go_hard_cap() {
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    // None on purpose — exercise the "no TOML
                    // override, only the hard cap applies" branch.
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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
                body.get("max_tokens").and_then(|v| v.as_u64()),
                Some(super::super::capabilities::OPENCODE_GO_MAX_TOKENS_CAP as u64),
                "opencode_go hard cap must clamp 1_000_000 → 16_384, got body: {body}"
            );
        });
    }

    /// Same regression guard for the streaming Responses path
    /// (`send_streaming`). The clamp must apply on both paths so
    /// operators who flip the wire to `stream=true` (e.g. for the
    /// long-context roles) do not reintroduce the HTTP 400.
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
            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                    max_token_auto: None,
                    max_token_auto_save: true,
                    plan: None,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: true,
                extra_messages: vec![],
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
                body.get("max_tokens").and_then(|v| v.as_u64()),
                Some(super::super::capabilities::OPENCODE_GO_MAX_TOKENS_CAP as u64),
                "opencode_go hard cap must clamp 1_000_000 → 16_384 on the SSE path too, got body: {body}"
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
        }
    }

    /// JSON role + non-opted-out model → `response_format` must be
    /// serialised as `{"type":"json_object"}`. Pins the contract
    /// the capability matrix advertises
    /// (`supports_response_format: true`).
    #[test]
    fn responses_wire_sets_response_format_when_role_requires_json_and_model_is_not_opted_out() {
        let req = json_request(crate::llm::Role::Intake, "gpt-5.6-luna");
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value.get("response_format"),
            Some(&serde_json::json!({"type": "json_object"})),
            "Intake role + gpt-5.6-luna must include response_format, got: {value}"
        );
    }

    /// `Sketch` is a free-text role and is NOT in `role_requires_json`,
    /// so the field must stay absent even on a non-opted-out model.
    #[test]
    fn responses_wire_omits_response_format_for_role_sketch() {
        let req = json_request(crate::llm::Role::Sketch, "gpt-5.6-luna");
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert!(
            value.get("response_format").is_none(),
            "Sketch role must drop response_format, got: {value}"
        );
    }

    /// Model on the opt-out list + JSON role → field must stay
    /// absent so the upstream returns raw markdown instead of
    /// prose-prefixed JSON.
    #[test]
    fn responses_wire_omits_response_format_for_opted_out_model() {
        let req = json_request(crate::llm::Role::Intake, "kimi-k3");
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert!(
            value.get("response_format").is_none(),
            "opted-out model kimi-k3 must drop response_format, got: {value}"
        );
    }

    /// The pre-existing fields (`model`, `instructions`, `input`,
    /// `max_tokens`, `temperature`, `top_p`, `stream`) are untouched
    /// regardless of the gate. Guards against a regression that
    /// would re-shape the body when adding the new field.
    #[test]
    fn responses_wire_includes_other_fields_unaffected() {
        let req = Request {
            role: crate::llm::Role::Route,
            model: "gpt-5.6-luna".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: 256,
            temperature: Some(0.4),
            top_p: Some(0.9),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
        };
        let body = build_responses_body(&req, &req.model, false, false);
        let value: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["model"], "gpt-5.6-luna");
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
        // is also present.
        assert_eq!(
            value["response_format"],
            serde_json::json!({"type": "json_object"})
        );
    }

    /// Auto-probe table clamp contract: when
    /// `with_max_tokens_table` attaches a table carrying a
    /// discovered value smaller than the requested `max_tokens`
    /// AND smaller than the documented hard cap, the wire body
    /// must carry the discovered value on the non-streaming
    /// Responses path. Pins the v0.7 precedence order:
    /// `OPENCODE_GO_MAX_TOKENS_CAP` > operator > table > req.
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
            let discovered = table
                .probe_and_store("opencode_go", "gpt-5.6-luna", transport)
                .await
                .expect("probe_and_store");
            // The wire-body assertion below uses `discovered`
            // directly: this test pins the wiring contract
            // (table value honoured on the wire) without depending
            // on the probe algorithm's exact convergence — that
            // algorithm has a known ±N imprecision at non-trivial
            // boundaries (see pre-existing
            // `probe::tests::detect_finds_cap_at_8k`).

            let p = OpenCodeGoResponsesProvider::new(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: format!("{}/v1", server.uri()),
                    model: "gpt-5.6-luna".into(),
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    hard_incompatibilities: vec![],
                    omit_max_tokens: false,
                    plan: None,
                    max_token_auto: None,
                    max_token_auto_save: true,
                },
                SecretString::new("dummy".into()),
            )
            .unwrap()
            .with_max_tokens_table(table);

            let req = Request {
                role: crate::llm::Role::Intake,
                model: "gpt-5.6-luna".into(),
                system: "sys".into(),
                user: "user".into(),
                max_tokens: 1_000_000,
                temperature: None,
                top_p: None,
                response_schema: None,
                stream: false,
                extra_messages: vec![],
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
