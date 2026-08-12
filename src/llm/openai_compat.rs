//! Generic OpenAI Chat Completions provider.
//!
//! Used by any backend whose API is OpenAI-compatible: DeepSeek
//! (`https://api.deepseek.com/v1`), OpenCode Go
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
use super::opencode_go::OpenCodeGoDispatch;
use super::probe::MIN_AUTOPROBE_FLOOR;
use super::probe_table::MaxTokensTable;
use super::provider::Provider;
use super::response_format_opt_out;
use super::size_limits::{MAX_RESPONSE_BYTES, check_size};
use super::wire::{Request, Response, Usage};

/// Generic OpenAI-compat provider.
#[derive(Clone)]
pub struct OpenAiCompatProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: Client,
    max_retries: u32,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,000,000), so the per-role runtime
    /// value normally fits under the cap. The clamp below exists
    /// for the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request with 400.
    provider_max_tokens: Option<u32>,
    /// Kind-level hard cap on `max_tokens`, applied as a second
    /// layer on top of `provider_max_tokens`. `None` means no
    /// kind-level cap (DeepSeek-direct uses this; DeepSeek accepts
    /// up to 8192 per its docs and the per-provider TOML knob is
    /// enough). `Some(16_384)` is wired by the OpenCode Go
    /// dispatcher so the upstream never returns HTTP 400 for
    /// kimi-k*, qwen3.x, gpt-5.6-luna, etc. — see
    /// `capabilities::OPENCODE_GO_MAX_TOKENS_CAP`.
    kind_hard_cap: Option<u32>,
    /// Auto-probed `max_tokens` table. When `Some` the
    /// `resolve_cached(self.name(), self.model())` value joins the
    /// clamp chain as the third-highest layer (kind-level cap >
    /// operator override > table). `None` when the provider was
    /// built without going through `registry_from_config` — unit
    /// tests and legacy call paths.
    max_tokens_table: Option<Arc<MaxTokensTable>>,
}

impl OpenAiCompatProvider {
    /// Build from a `ProviderConfig` and a resolved API key.
    /// Equivalent to [`OpenAiCompatProvider::new_with_kind_cap`]
    /// with `cap = None`. Use that constructor instead when the
    /// provider is routed through OpenCode Go.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        Self::new_with_kind_cap(spec, api_key, None)
    }

    /// Build from a `ProviderConfig`, a resolved API key, and an
    /// optional kind-level hard cap on `max_tokens`. The
    /// dispatcher (`super::opencode_go`) passes
    /// `Some(capabilities::OPENCODE_GO_MAX_TOKENS_CAP)` so
    /// every OpenCode Go model respects the upstream's
    /// heterogeneous `max_tokens` ceiling; DeepSeek-direct passes
    /// `None` because the direct upstream's 8192 limit is already
    /// covered by `ProviderConfig::max_tokens`.
    pub fn new_with_kind_cap(
        spec: &ProviderConfig,
        api_key: SecretString,
        cap: Option<u32>,
    ) -> Result<Self> {
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
            kind_hard_cap: cap,
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
        ChatRequest {
            model: &self.model,
            messages,
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

/// Custom Debug that masks `max_tokens_table` — `MaxTokensTable`
/// does not implement `Debug` (that lives in `probe_table.rs`,
/// outside this provider's owned files) and the table is a shared
/// `Arc`, so emitting `<shared>` keeps the dump informative without
/// forcing a cross-file change.
impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
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
        self.send_with_safety_clamp(req, true).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Mirror of the clamp chain in
        // `send_with_safety_clamp(_, true)` so the audit-log hash is
        // byte-for-byte identical to the wire body. Same ordering as
        // `send`:
        //   1. `kind_hard_cap` (dispatcher-set, e.g.
        //      `OPENCODE_GO_MAX_TOKENS_CAP = 16_384` for OpenCode
        //      Go routes; `None` for DeepSeek-direct).
        //   2. `provider_max_tokens` (operator TOML override).
        //   3. `MaxTokensTable::resolve_cached` (auto-probed value).
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let kind_cap = self.kind_hard_cap.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        req.max_tokens
            .min(operator_cap)
            .min(kind_cap)
            .min(table_cap)
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
}

impl OpenCodeGoDispatch for OpenAiCompatProvider {
    fn url(&self) -> String {
        self.chat_url()
    }
}

impl OpenAiCompatProvider {
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
                // Apply three-layer max_tokens cap. Highest priority
                // (smallest wins) to lowest:
                //   1. `kind_hard_cap` — set by the OpenCode Go
                //      dispatcher to `OPENCODE_GO_MAX_TOKENS_CAP =
                //      16_384`. DeepSeek-direct leaves `None` (the
                //      direct upstream's 8192 limit is covered by
                //      the operator override).
                //   2. `provider_max_tokens` — operator TOML override.
                //   3. `MaxTokensTable::resolve_cached` — auto-probed
                //      per-(provider, model) value; primary source of
                //      truth when present.
                let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
                let kind_cap = self.kind_hard_cap.unwrap_or(u32::MAX);
                let table_cap = self
                    .max_tokens_table
                    .as_ref()
                    .and_then(|t| t.resolve_cached(self.name(), self.model()))
                    .unwrap_or(u32::MAX);
                let cap = operator_cap.min(kind_cap).min(table_cap);
                if body.max_tokens > cap {
                    ChatRequest {
                        max_tokens: cap,
                        ..body
                    }
                } else {
                    body
                }
            } else {
                // Probe path: bypass every cap. Floor ensures we
                // never ask for `max_tokens < 1024` (some upstreams
                // reject the request outright below that minimum).
                if body.max_tokens < MIN_AUTOPROBE_FLOOR {
                    ChatRequest {
                        max_tokens: MIN_AUTOPROBE_FLOOR,
                        ..body
                    }
                } else {
                    body
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
                        return Err(Error::Provider(format!(
                            "openai-compat: HTTP {code} after {attempt} attempts: {body}"
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
                Err(e) => {
                    if attempt >= max_retries {
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
    use crate::llm::capabilities;

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
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_save: true,
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
        let p = OpenAiCompatProvider::new(
            &ProviderConfig {
                kind: "deepseek".into(),
                endpoint: "https://api.deepseek.com/v1".into(),
                model: "deepseek-v4-flash".into(),
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
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_save: true,
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
        let p = provider_with_model("opencode_go", "https://opencode.ai/zen/go/v1", "kimi-k3");
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

    /// PR-fix (opencode_go hard cap): when the dispatcher wires an
    /// `OpenAiCompatProvider` for an OpenCode Go model
    /// (`new_with_kind_cap(_, _, Some(OPENCODE_GO_MAX_TOKENS_CAP))`)
    /// the wire body must clamp `request.max_tokens` to 16_384 even
    /// if `ProviderConfig::max_tokens` is unset. Without the clamp
    /// the upstream returns HTTP 400 because the per-role default
    /// `DEFAULT_MAX_TOKENS = 1_000_000` flows through. Pins the
    /// defence at the integration boundary (`send` + recorded
    /// request body).
    #[test]
    fn opencode_go_backed_provider_clamps_max_tokens_to_hard_cap() {
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
            let p = OpenAiCompatProvider::new_with_kind_cap(
                &ProviderConfig {
                    kind: "opencode_go".into(),
                    endpoint: server.uri(),
                    model: "kimi-k2.7-code".into(),
                    // None on purpose: only the kind-level cap
                    // applies.
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
                Some(capabilities::OPENCODE_GO_MAX_TOKENS_CAP),
            )
            .unwrap();
            let req = Request {
                role: crate::llm::Role::Route,
                model: "kimi-k2.7-code".into(),
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
            assert_eq!(
                body["max_tokens"],
                serde_json::json!(capabilities::OPENCODE_GO_MAX_TOKENS_CAP),
                "opencode_go chat-completions hard cap must clamp 1_000_000 → 16_384, got body: {body}"
            );
        });
    }

    /// `new` (no kind cap) preserves the existing DeepSeek-direct
    /// behaviour: when the operator sets `max_tokens = None` in
    /// TOML the wire body carries whatever `request.max_tokens`
    /// says, because DeepSeek-direct has no kind-level cap. Pins
    /// the asymmetry between `new` (no cap) and `new_with_kind_cap`
    /// (cap wired by the dispatcher).
    #[test]
    fn deepseek_direct_provider_uses_no_kind_cap() {
        let p = OpenAiCompatProvider::new(
            &ProviderConfig {
                kind: "deepseek".into(),
                endpoint: "https://api.deepseek.com/v1".into(),
                model: "deepseek-v4-flash".into(),
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
        assert_eq!(p.kind_hard_cap, None);
    }

    /// `new_with_kind_cap` propagates the cap from the dispatcher
    /// (an operator who constructs it manually — e.g. a test rig —
    /// observes the same value the dispatcher would). Pins the
    /// surface so a future refactor cannot accidentally drop the
    /// wiring.
    #[test]
    fn new_with_kind_cap_stores_cap_in_field() {
        let p = OpenAiCompatProvider::new_with_kind_cap(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "kimi-k3".into(),
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
            Some(capabilities::OPENCODE_GO_MAX_TOKENS_CAP),
        )
        .unwrap();
        assert_eq!(
            p.kind_hard_cap,
            Some(capabilities::OPENCODE_GO_MAX_TOKENS_CAP)
        );
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
        let discovered = table
            .probe_and_store("deepseek", "deepseek-v4-flash", transport)
            .await
            .expect("probe_and_store");
        // The wire-body assertion below uses `discovered`
        // directly: this test pins the wiring contract (table
        // value honoured on the wire) without depending on the
        // probe algorithm's exact convergence — that algorithm
        // has a known ±N imprecision at non-trivial boundaries
        // (see pre-existing `probe::tests::detect_finds_cap_at_8k`).

        let p = OpenAiCompatProvider::new(
            &ProviderConfig {
                kind: "deepseek".into(),
                endpoint: server.uri(),
                model: "deepseek-v4-flash".into(),
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
            role: crate::llm::Role::Route,
            model: "deepseek-v4-flash".into(),
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
        let body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("mock server received a JSON body");
        assert_eq!(
            body["max_tokens"],
            serde_json::json!(discovered),
            "wire body must carry the table-resolved value ({discovered}), got body: {body}"
        );
    }
}
