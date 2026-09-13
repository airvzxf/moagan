//! Wire-body builders for the OpenAI-compat (`/v1/chat/completions`)
//! and OpenAI Responses (`/v1/responses`) variants.
//!
//! Lifted from `src/llm/openai_compatible.rs::build_chat_request_body`
//! and `src/llm/openai_compat.rs::build_responses_body` (both deleted
//! in #933). The SDK impl (`OpenAIClient`) drives these helpers
//! directly so the wire shape stays byte-identical to the pre-#933
//! `OpenAICompatibleProvider` / `OpenAICompatProvider` impls.

use serde::{Deserialize, Serialize};

use crate::llm::client::LlmRequest;

use super::role_requires_json;

/// Build the `/v1/chat/completions` wire body for `(model, req)`.
pub(crate) fn build_chat_request_body<'a>(model: &'a str, req: &LlmRequest) -> ChatRequest<'a> {
    let strategy = crate::llm::json_strategy::strategy_for(model, None);
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
    for m in &req.extra_messages {
        messages.push(ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        });
    }
    if req.extra_messages.is_empty() && crate::llm::json_strategy::needs_assistant_prefill(strategy)
    {
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: "{".into(),
        });
    }
    let response_format = if role_requires_json(req.role)
        && !crate::llm::response_format_opt_out::model_skips_response_format(model)
    {
        Some(ResponseFormat {
            kind: "json_object",
        })
    } else {
        None
    };
    tracing::trace!(
        model = %model,
        role = ?req.role,
        strategy = ?strategy,
        message_count = messages.len(),
        wants_format = response_format.is_some(),
        "build_chat_request_body"
    );
    ChatRequest {
        model,
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stream: false,
        response_format,
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest<'a> {
    model: &'a str,
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f32>,
    /// Top-k sampling cutoff (`#920` D7). `None` keeps the wire body
    /// byte-identical to pre-#920 chat-completions requests so
    /// existing upstreams (which do not advertise `top_k`) keep
    /// accepting the body without the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_k: Option<u32>,
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_format: Option<ResponseFormat>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatChoice {
    message: ChatMessageOut,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatMessageOut {
    content: String,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ChatUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// Build the `/v1/responses` wire body for `(req, model)`.
pub(crate) fn build_responses_body<'a>(
    req: &'a LlmRequest,
    model: &'a str,
    stream: bool,
    omit_max_tokens: bool,
) -> ResponsesRequest<'a> {
    let body = ResponsesRequest {
        model,
        instructions: Some(&req.system),
        input: &req.user,
        max_tokens: if omit_max_tokens {
            None
        } else {
            req.max_tokens
        },
        temperature: req.temperature,
        top_p: req.top_p,
        text: responses_text_json_object(wants_response_format(req.role, model)),
        stream,
    };
    tracing::trace!(
        model,
        role = ?req.role,
        stream,
        omit_max_tokens,
        max_tokens = ?body.max_tokens,
        wants_format = body.text.is_some(),
        "build_responses_body"
    );
    body
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesRequest<'a> {
    pub(crate) model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<&'a str>,
    pub(crate) input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<ResponsesText>,
    pub(crate) stream: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesText {
    format: ResponsesTextFormat,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesTextFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// Compute the gate the OpenAI-compat path uses to decide whether
/// to send `text: { format: { type: "json_object" } }`.
pub(crate) fn wants_response_format(role: crate::llm::Role, model: &str) -> bool {
    role_requires_json(role)
        && !crate::llm::response_format_opt_out::model_skips_response_format(model)
}

/// Build the `text: { format: { type: "json_object" } }` payload
/// when the JSON gate fires, or `None` so the field is dropped from
/// the wire body entirely.
pub(crate) fn responses_text_json_object(wants: bool) -> Option<ResponsesText> {
    if wants {
        Some(ResponsesText {
            format: ResponsesTextFormat {
                kind: "json_object",
            },
        })
    } else {
        None
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesBody {
    #[serde(default)]
    pub(crate) output: Vec<ResponsesOutput>,
    #[serde(default)]
    pub(crate) usage: Option<ResponsesUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesOutput {
    #[serde(default)]
    pub(crate) content: Vec<ResponsesContent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponsesContent {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) text: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ResponsesUsage {
    #[serde(default)]
    pub(crate) input_tokens: u64,
    #[serde(default)]
    pub(crate) output_tokens: u64,
}

/// Accumulate an OpenAI Responses SSE wire body and return the
/// joined text plus the most-recent usage block. Lifted from the
/// legacy `OpenAICompatProvider::accumulate_sse_responses` so the
/// `OpenAIClient::send_streaming` path keeps working without a
/// dependency on the deleted `src/llm/openai_compat.rs` module.
pub(crate) fn accumulate_sse_responses(
    body: &[u8],
) -> crate::error::Result<(String, ResponsesUsage)> {
    use crate::llm::sse_parser::{SseError, SseParser};
    let mut parser = SseParser::new(body);
    let mut text = String::new();
    let mut usage = ResponsesUsage::default();
    let mut deltas = 0usize;
    loop {
        match parser.next_data::<ResponsesBody>() {
            Ok(Some(delta)) => {
                deltas += 1;
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
                tracing::warn!(error = %e, deltas, "accumulate_sse_responses: SSE parse failed");
                return Err(match e {
                    SseError::Io(err) => crate::error::Error::Provider {
                        message: format!("sse io: {err}"),
                        http_status: None,
                    },
                    SseError::Parse(err) => crate::error::Error::Provider {
                        message: format!("sse parse: {err}"),
                        http_status: None,
                    },
                });
            }
        }
    }
    tracing::trace!(
        deltas,
        text_len = text.len(),
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        "accumulate_sse_responses: complete"
    );
    Ok((text, usage))
}
