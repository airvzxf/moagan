//! Wire-format trait. Lets us share request/response shapes between
//! providers that share a protocol (e.g. Anthropic-compatible, OpenAI).
//!
//! Compliance: 10-integrada-v0 §D.1.2 (WireFormat).

use async_trait::async_trait;

use crate::error::{Error, Result};

use super::role::Role;
use super::wire::{Request, Response, Usage};

/// HTTP status payload returned alongside the decoded body. Lets
/// the dispatcher log transport-level detail (status code,
/// retry-after) without round-tripping through the raw
/// `reqwest::Response`.
#[derive(Debug, Clone)]
pub struct WireResponse {
    /// HTTP status code returned by the upstream.
    pub status: u16,
    /// Decoded response body.
    pub body: Response,
}

/// A wire format translates a moagan [`Request`] into the JSON shape
/// the provider expects, and parses the raw response back. Three
/// concrete impls live here:
///
/// - [`AnthropicWire`] — `/v1/messages` shape, `system` separate
///   from `messages` (used by `minimax` and `opencode_go_anthropic`).
/// - [`OpenAiWire`] — `/v1/chat/completions` shape, used by all
///   OpenAI-compat providers (DeepSeek, `opencode_go` chat path).
/// - [`CustomWire`] — caller-supplied handler for backends that
///   follow none of the stock shapes.
///
/// Implementations are stateless zSTs; one instance can be shared
/// across providers that share a wire format.
#[async_trait]
pub trait WireFormat: Send + Sync {
    /// Stable name (e.g. `"anthropic"`, `"openai"`, `"custom"`).
    fn name(&self) -> &str;

    /// Serialize the request body for the provider's HTTP endpoint.
    fn encode_body(&self, req: &Request) -> Result<Vec<u8>>;

    /// Parse the raw response body into a moagan [`Response`] and
    /// return it alongside the HTTP status. Returning
    /// [`WireResponse`] keeps the dispatcher transparent: callers
    /// that do not need the status code can drop it with
    /// `.body`.
    fn decode(&self, status: u16, body: &[u8]) -> Result<WireResponse>;

    /// Map an HTTP error response into a moagan [`Error`]. Default
    /// delegates to the canonical
    /// [`classify_status`](super::http::classify_status) helper,
    /// which is shared with the production HTTP path so wire
    /// formats stay aligned.
    fn classify_error(&self, status: u16, body: &str) -> Error {
        super::http::classify_status(reqwest::StatusCode::from_u16(status).unwrap(), body)
    }

    /// Encode the request body, returning a [`serde_json::Value`]
    /// (debug / wire-shape introspection helper). Most callers
    /// want [`Self::encode_body`] for the actual HTTP body. The
    /// value mirrors what `encode_body` produces minus the
    /// `serde_json::to_vec` step.
    fn encode_value(&self, req: &Request) -> Result<serde_json::Value> {
        let bytes = self.encode_body(req)?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Provider(format!("encode_value: {e}")))
    }
}

/// Anthropic-compatible wire format. Compatible with the `minimax`
/// endpoint at `/v1/messages`.
pub struct AnthropicWire;

#[async_trait]
impl WireFormat for AnthropicWire {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn encode_body(&self, req: &Request) -> Result<Vec<u8>> {
        let body = super::http::body_from_request(req);
        serde_json::to_vec(&body).map_err(|e| Error::Provider(format!("encode: {e}")))
    }

    fn decode(&self, status: u16, body: &[u8]) -> Result<WireResponse> {
        let parsed: super::http::MessagesResponseBody =
            serde_json::from_slice(body).map_err(|e| Error::Provider(format!("decode: {e}")))?;
        let response = parsed
            .into_response()
            .map_err(|e| Error::Provider(e.to_string()))?;
        Ok(WireResponse {
            status,
            body: response,
        })
    }
}

/// OpenAI-compatible wire format. `/v1/chat/completions` body
/// shape, with the role-based JSON mode flag and the
/// per-model opt-out list honoured on the way out. Used by all
/// OpenAI-compat providers (DeepSeek, the `opencode_go` chat
/// path, and any future OpenAI-compat subscription).
pub struct OpenAiWire;

#[async_trait]
impl WireFormat for OpenAiWire {
    fn name(&self) -> &str {
        "openai"
    }

    fn encode_body(&self, req: &Request) -> Result<Vec<u8>> {
        let value = build_openai_body(req);
        serde_json::to_vec(&value).map_err(|e| Error::Provider(format!("encode: {e}")))
    }

    fn decode(&self, status: u16, body: &[u8]) -> Result<WireResponse> {
        let parsed: OpenAiChatResponse =
            serde_json::from_slice(body).map_err(|e| Error::Provider(format!("decode: {e}")))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| Error::Provider("openai wire: empty choices array".into()))?;
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
        Ok(WireResponse {
            status,
            body: response,
        })
    }
}

/// OpenAI Responses API wire format. The shape differs from
/// `/v1/chat/completions`: the request uses `input` (single
/// string) instead of `messages`, the system prompt rides on
/// `instructions`, and the response body is
/// `{"output": [{"content": [{"type": "output_text",
/// "text": "..."}]}]}`. Currently used by the `opencode_go_responses`
/// provider for `gpt-5.6-luna`.
pub struct ResponsesWire;

#[async_trait]
impl WireFormat for ResponsesWire {
    fn name(&self) -> &str {
        "responses"
    }

    fn encode_body(&self, req: &Request) -> Result<Vec<u8>> {
        let value = serde_json::json!({
            "model": req.model,
            "instructions": req.system,
            "input": req.user,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "top_p": req.top_p,
            "stream": false,
        });
        serde_json::to_vec(&value).map_err(|e| Error::Provider(format!("encode: {e}")))
    }

    fn decode(&self, status: u16, body: &[u8]) -> Result<WireResponse> {
        let parsed: ResponsesBody =
            serde_json::from_slice(body).map_err(|e| Error::Provider(format!("decode: {e}")))?;
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
        Ok(WireResponse {
            status,
            body: response,
        })
    }
}

/// Custom wire format — caller-supplied. Holders carry an
/// opaque `id` (debug identifier) and an arbitrary JSON
/// `schema` (the canned body the caller wants to send on the
/// wire; encode passes it through unchanged). Decode still
/// returns a structured error explaining the receiver was
/// never wired up — custom shapes are paired with a custom
/// decoder, which `OpenAiWire` covers for the common case.
pub struct CustomWire {
    /// Stable identifier for this custom wire (e.g. `"ngc"`).
    pub id: String,
    /// Caller-provided JSON body that overrides the standard
    /// request shape. `OpenAiWire` ignores this field; the
    /// dispatcher applies `schema` to `encode_body`.
    pub schema: serde_json::Value,
}

#[async_trait]
impl WireFormat for CustomWire {
    fn name(&self) -> &str {
        &self.id
    }

    fn encode_body(&self, _req: &Request) -> Result<Vec<u8>> {
        serde_json::to_vec(&self.schema).map_err(|e| Error::Provider(format!("encode custom: {e}")))
    }

    fn decode(&self, _status: u16, _body: &[u8]) -> Result<WireResponse> {
        Err(Error::Provider(format!(
            "custom wire '{}' has no decoder wired up; configure one via the dispatcher",
            self.id
        )))
    }
}

// -------------------------------------------------------------------------
// OpenAI Chat Completions request shape
// -------------------------------------------------------------------------

/// Roles that produce structured JSON output. The OpenAI-compat
/// providers get `response_format` set to `json_object` for these
/// roles so the JSON parser in `parse_model_json` stops hitting
/// the trailing-token / missing-brace pathologies.
fn role_requires_json(role: Role) -> bool {
    use Role::*;
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

/// Build the OpenAI Chat Completions request body for a given
/// provider request, applying the role-based JSON output mode and
/// the per-model opt-out from `response_format_opt_out`.
pub(crate) fn build_openai_body(req: &Request) -> serde_json::Value {
    let mut value = serde_json::json!({
        "model": req.model,
        "messages": [
            {"role": "system", "content": req.system},
            {"role": "user", "content": req.user},
        ],
        "max_tokens": req.max_tokens,
    });
    if let Some(t) = req.temperature {
        value["temperature"] = serde_json::json!(t);
    }
    if let Some(p) = req.top_p {
        value["top_p"] = serde_json::json!(p);
    }
    if role_requires_json(req.role)
        && !super::response_format_opt_out::model_skips_response_format(&req.model)
    {
        value["response_format"] = serde_json::json!({"type": "json_object"});
    }
    value
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiChatUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiChatChoice {
    message: OpenAiChatMessageOut,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiChatMessageOut {
    content: String,
}

#[derive(Debug, serde::Deserialize, Default)]
struct OpenAiChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

// -------------------------------------------------------------------------
// OpenAI Responses API response shape
// -------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct ResponsesBody {
    #[serde(default)]
    output: Vec<ResponsesOutput>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct ResponsesOutput {
    #[serde(default)]
    content: Vec<ResponsesContent>,
}

#[derive(Debug, serde::Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> Request {
        Request {
            role: Role::Intake,
            model: "deepseek-v4-flash".into(),
            system: "system".into(),
            user: "user".into(),
            max_tokens: 128,
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            reasoning_tokens: None,
            reasoning_effort: None,
        }
    }

    #[tokio::test]
    async fn anthropic_wire_roundtrips() {
        let req = Request {
            role: Role::Intake,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            reasoning_tokens: None,
            reasoning_effort: None,
        };
        let wire = AnthropicWire;
        let body = wire.encode_body(&req).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "m");
        assert_eq!(json["system"], "s");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "u");
    }

    /// OpenAI-compat body contract: the request carries `model`,
    /// `messages` (system + user), `max_tokens`, and any optional
    /// temperature / top_p the call site set. The
    /// `response_format` flag is omitted for non-JSON roles.
    #[test]
    fn openai_wire_encodes_basic_request() {
        let wire = OpenAiWire;
        let body = wire.encode_body(&sample_request()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "deepseek-v4-flash");
        assert_eq!(json["max_tokens"], 128);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "user");
        let temp = json["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.6).abs() < 1e-6,
            "temperature must be 0.6, got {temp}"
        );
        let top_p = json["top_p"].as_f64().unwrap();
        assert!(
            (top_p - 0.95).abs() < 1e-6,
            "top_p must be 0.95, got {top_p}"
        );
        // Intake is a JSON role, but "deepseek-v4-flash" is not on
        // the opt-out list — the field is sent.
        assert_eq!(
            json["response_format"],
            serde_json::json!({"type": "json_object"})
        );
    }

    /// OpenAI-compat body contract for markdown roles (Propose):
    /// even when the role expects JSON, markdown output drops the
    /// `response_format` field so the upstream emits free-form
    /// markdown.
    #[test]
    fn openai_wire_omits_response_format_for_markdown_role() {
        let wire = OpenAiWire;
        let req = Request {
            role: Role::Propose,
            ..sample_request()
        };
        let body = wire.encode_body(&req).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("response_format").is_none(),
            "Propose role must drop response_format, got: {json}"
        );
    }

    /// OpenAI-compat body contract for opted-out models: even on
    /// JSON roles, models on the `response_format_opt_out` list
    /// lose the field so the upstream returns raw markdown.
    #[test]
    fn openai_wire_omits_response_format_for_opted_out_model() {
        let wire = OpenAiWire;
        let req = Request {
            role: Role::Route,
            model: "kimi-k2.7-code".into(),
            ..sample_request()
        };
        let body = wire.encode_body(&req).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("response_format").is_none(),
            "opted-out model must drop response_format, got: {json}"
        );
    }

    /// Decode a canonical OpenAI success payload: text, finish
    /// reason, truncated flag, and the usage totals.
    #[test]
    fn openai_wire_decodes_success_response() {
        let wire = OpenAiWire;
        let raw = serde_json::json!({
            "choices": [
                {
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 42,
                "completion_tokens": 7
            }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let decoded = wire.decode(200, &bytes).unwrap();
        assert_eq!(decoded.status, 200);
        assert_eq!(decoded.body.text, "hello");
        assert_eq!(decoded.body.finish_reason.as_deref(), Some("stop"));
        assert!(!decoded.body.truncated);
        assert_eq!(decoded.body.usage.input_tokens, 42);
        assert_eq!(decoded.body.usage.output_tokens, 7);
    }

    /// `finish_reason: length` flips the `truncated` flag so the
    /// pipeline can branch on the cut-off response.
    #[test]
    fn openai_wire_decodes_truncated_response() {
        let wire = OpenAiWire;
        let raw = serde_json::json!({
            "choices": [
                {
                    "message": {"role": "assistant", "content": "partial"},
                    "finish_reason": "length"
                }
            ]
        });
        let decoded = wire
            .decode(200, &serde_json::to_vec(&raw).unwrap())
            .unwrap();
        assert_eq!(decoded.body.text, "partial");
        assert!(decoded.body.truncated);
    }

    /// Empty choices array is a provider-side fault, not a
    /// transport error. Surface as `Error::Provider` so the
    /// circuit-breaker records the failure.
    #[test]
    fn openai_wire_decodes_empty_choices_as_error() {
        let wire = OpenAiWire;
        let raw = serde_json::json!({"choices": []});
        let err = wire
            .decode(200, &serde_json::to_vec(&raw).unwrap())
            .expect_err("empty choices must error");
        assert!(matches!(err, Error::Provider(_)));
    }

    /// 429 maps to `PlanExhausted` via the shared
    /// `classify_status` helper. Other status codes stay stable
    /// across wire formats so dashboards see one shape.
    #[test]
    fn openai_wire_decodes_429_as_rate_limited() {
        let wire = OpenAiWire;
        let body = r#"{"error": {"message": "rate limit", "type": "rate_limit_error"}}"#;
        let err = wire.classify_error(429, body);
        assert!(
            matches!(err, Error::PlanExhausted(_)),
            "429 must classify as PlanExhausted, got: {err:?}"
        );
    }

    /// 401 surfaces as `Error::InvalidApiKey` so the operator
    /// gets a clear signal that the credential bundle is
    /// misconfigured.
    #[test]
    fn openai_wire_decodes_401_as_invalid_api_key() {
        let wire = OpenAiWire;
        let body = "unauthorized";
        let err = wire.classify_error(401, body);
        assert!(matches!(err, Error::InvalidApiKey(_)));
    }

    /// Responses API round-trip: encode produces an `input`
    /// string + optional `instructions`; decode threads the
    /// `output_text` blocks into a flat string.
    #[test]
    fn responses_wire_round_trip() {
        let wire = ResponsesWire;
        let req = Request {
            role: Role::Intake,
            model: "gpt-5.6-luna".into(),
            system: "sys".into(),
            user: "u".into(),
            max_tokens: 64,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            reasoning_tokens: None,
            reasoning_effort: None,
        };
        let body = wire.encode_body(&req).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "gpt-5.6-luna");
        assert_eq!(json["instructions"], "sys");
        assert_eq!(json["input"], "u");
        assert_eq!(json["max_tokens"], 64);
        assert_eq!(json["stream"], false);
        // Decode a full success body.
        let raw = serde_json::json!({
            "output": [
                {"content": [{"type": "output_text", "text": "hi"}]}
            ],
            "usage": {
                "input_tokens": 11,
                "output_tokens": 2
            }
        });
        let decoded = wire
            .decode(200, &serde_json::to_vec(&raw).unwrap())
            .unwrap();
        assert_eq!(decoded.body.text, "hi");
        assert_eq!(decoded.body.usage.input_tokens, 11);
        assert_eq!(decoded.body.usage.output_tokens, 2);
    }

    /// CustomWire passes the caller-supplied schema through
    /// untouched on encode; decode still returns a structured
    /// error so callers notice a decoder was never wired up.
    #[test]
    fn custom_wire_passes_schema_through() {
        let wire = CustomWire {
            id: "ngc".into(),
            schema: serde_json::json!({"custom": true, "shape": "arbitrary"}),
        };
        let req = sample_request();
        let bytes = wire.encode_body(&req).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"custom": true, "shape": "arbitrary"})
        );
        assert_eq!(wire.name(), "ngc");
        let err = wire
            .decode(200, b"{}")
            .expect_err("custom wire must surface unconfigured decoder");
        assert!(matches!(err, Error::Provider(_)));
    }
}
