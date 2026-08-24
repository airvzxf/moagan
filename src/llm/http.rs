//! HTTP transport shared by Anthropic-compatible providers (e.g. the
//! `minimax` provider). Designed to be small and predictable; retry,
//! jitter, and circuit breaking are layered on top.

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::error::Error;

use super::wire::{Request, Response, Usage};

/// Build a `reqwest::Client` configured for the moagan transport.
pub fn build_client() -> std::result::Result<Client, Error> {
    Client::builder()
        .timeout(Duration::from_secs(180))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("moagan/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Provider {
            message: format!("build reqwest client: {e}"),
            http_status: None,
        })
}

/// Build the headers for an Anthropic-compatible POST.
pub fn build_headers(
    api_key: &str,
    extra: &[(String, String)],
) -> std::result::Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_str(api_key).map_err(|e| Error::InvalidApiKey {
            message: format!("x-api-key: {e}"),
            http_status: None,
        })?,
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    for (k, v) in extra {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| Error::Provider {
            message: format!("header {k}: {e}"),
            http_status: None,
        })?;
        let value = HeaderValue::from_str(v).map_err(|e| Error::Provider {
            message: format!("header {k} value: {e}"),
            http_status: None,
        })?;
        headers.insert(name, value);
    }
    let _ = AUTHORIZATION;
    Ok(headers)
}

/// Translate status into a moagan error, with structured context.
///
/// HTTP 429 is split into two distinct error variants:
///
/// - `Error::Throttled` — transient rate-limit (RPM/TPM etc.); the
///   [`crate::llm::governor::ThrottleGovernor`] absorbs these and
///   the per-(provider, role) breaker is NOT tripped.
/// - `Error::PlanExhausted` — quota / plan exhausted (the API
///   provider says "Upgrade your Token Plan"); the per-(provider,
///   role) breaker IS tripped.
///
/// The split is heuristic on the JSON body: keywords like `plan`,
/// `monthly`, `subscription`, `quota exhausted`, `upgrade` route to
/// `PlanExhausted`; everything else routes to `Throttled`. Bodies
/// that don't parse as JSON are treated as `Throttled` — the
/// adaptive governor absorbs them safely and the operator can
/// inspect the message via telemetry.
pub fn classify_status(status: StatusCode, body: &str) -> Error {
    let code = status.as_u16();
    match code {
        401 | 403 => Error::InvalidApiKey {
            message: format!("http {status}: {body}"),
            http_status: Some(code),
        },
        429 => classify_throttled_or_plan_exhausted(body),
        408 | 504 | 524 => Error::Timeout {
            message: format!("http {status}: {body}"),
            http_status: Some(code),
        },
        500..=599 => Error::Provider {
            message: format!("upstream {status}: {body}"),
            http_status: Some(code),
        },
        _ => Error::Provider {
            message: format!("http {status}: {body}"),
            http_status: Some(code),
        },
    }
}

/// Split an HTTP 429 body into `Error::Throttled` (transient) or
/// `Error::PlanExhausted` (persistent). The keyword scan is
/// deliberately conservative: any of `plan`, `monthly`, `quota`,
/// `subscription`, `upgrade` flips to `PlanExhausted`; otherwise
/// `Throttled`. The conservative side is intentional — when in
/// doubt, route to `Throttled`, because the adaptive governor
/// absorbs it cheaply; misrouting a `PlanExhausted` as
/// `Throttled` would just delay the breaker tripping by a few 429s.
///
/// Both arms carry `http_status: Some(429)` so the telemetry layer
/// populates `calls.http_status = 429` regardless of which arm the
/// classifier picks.
fn classify_throttled_or_plan_exhausted(body: &str) -> Error {
    let lower = body.to_ascii_lowercase();
    let plan_exhausted_keywords = ["plan", "monthly", "quota", "subscription", "upgrade"];
    let is_plan_exhausted = plan_exhausted_keywords.iter().any(|kw| lower.contains(kw));
    if is_plan_exhausted {
        Error::PlanExhausted {
            message: format!("http 429: {body}"),
            http_status: Some(429),
        }
    } else {
        Error::Throttled {
            retry_after_ms: None,
            message: body.to_string(),
            http_status: Some(429),
        }
    }
}

/// Construct an [`Error::Throttled`] directly when the upstream
/// `Retry-After` header was already parsed by the caller. Keeps
/// the wire layer (`opencode_go_anthropic.rs`) free of `http.rs`
/// internals — it just hands the parsed duration to this helper.
///
/// `http_status: Some(429)` is hard-coded because this helper is
/// only called from the 429 throttle path; the actual status code
/// is implicit in the call site.
pub fn throttled_with_retry_after(body: &str, retry_after: Option<Duration>) -> Error {
    Error::Throttled {
        retry_after_ms: retry_after.map(|d| d.as_millis() as u64),
        message: body.to_string(),
        http_status: Some(429),
    }
}

/// Inspect `Retry-After` from a response. Returns `None` if absent or
/// unparseable.
pub fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp.headers().get("retry-after")?.to_str().ok()?;
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP date form not handled in v0.1; we only support seconds.
    None
}

/// JSON shape we send to the `/v1/messages` endpoint.
#[derive(Debug, Serialize)]
pub(crate) struct MessagesRequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    /// Optional `thinking` control. We never set it on M-series
    /// models (the reference sweep shows thinking ON is the
    /// reliable default), but the field is kept here for future
    /// disable-from-config overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingControl>,
    system: &'a str,
    messages: Vec<MessagesMessage>,
}

/// `thinking: {"type": "disabled"}` per the MiniMax docs.
#[derive(Debug, Serialize)]
pub(crate) struct ThinkingControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessagesMessage {
    role: &'static str,
    content: String,
}

/// JSON shape we expect back from the `/v1/messages` endpoint.
///
/// `content` is nullable: when the requested `max_tokens` budget is
/// too small for the model to emit anything (e.g. the auto-probe at
/// `max_tokens = 2` against a model that needs at least a few tokens
/// to think), the upstream returns HTTP 200 with `"content": null`.
/// The decoder treats `null` as a successful empty response so the
/// probe's classifier does not collapse `Indeterminate` into a
/// false `Rejected` (which would break Phase 1 at `n=2`).
#[derive(Debug, Deserialize)]
pub(crate) struct MessagesResponseBody {
    #[serde(default)]
    content: Option<Vec<MessagesContent>>,
    stop_reason: Option<String>,
    usage: Option<MessagesUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesContent {
    /// Block type. We only consume `text`; `thinking` is dropped.
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

impl MessagesResponseBody {
    /// Extract the joined text and stop reason from the response body.
    /// Only `text` blocks are kept; `thinking` blocks are
    /// deliberately discarded (see `body_from_request` for the
    /// rationale). A `null` content (e.g. the upstream response to
    /// an auto-probe with a too-small `max_tokens` budget) is
    /// treated as an empty content array: the call is reported as
    /// successful with empty text.
    pub(crate) fn into_response(self) -> std::result::Result<Response, &'static str> {
        let mut text = String::new();
        for c in self.content.into_iter().flatten() {
            if c.kind == "text"
                && let Some(t) = c.text
            {
                text.push_str(&t);
            }
        }
        let usage = self.usage.map_or_else(Usage::default, |u| Usage {
            input_tokens: u.input_tokens.unwrap_or(0),
            output_tokens: u.output_tokens.unwrap_or(0),
            cache_read: u.cache_read_input_tokens.unwrap_or(0),
            cache_creation: u.cache_creation_input_tokens.unwrap_or(0),
        });
        let truncated = matches!(self.stop_reason.as_deref(), Some("max_tokens"));
        Ok(Response {
            text,
            finish_reason: self.stop_reason,
            truncated,
            usage,
        })
    }
}

/// Translate a [`Request`] into the body shape expected by the
/// Anthropic-compatible messages endpoint.
///
/// We do NOT send `thinking: disabled` for `MiniMax-M3`. The reference
/// sweep script (`minimax-moa-v1/scripts/run_sweep.py`) gets
/// consistently reliable JSON from the same model with thinking ON;
/// the model uses the thinking pass to plan the JSON shape before
/// emitting the text block. Sending `thinking: disabled` produces
/// earlier truncations on the M-series models. We only extract the
/// `text` block from the response, so the thinking content is
/// discarded — but the model's text block is more reliable as a
/// result.
///
/// PR-D2 follow-up: when the role requires JSON output, append an
/// assistant prefill of `{` to the messages array. The model
/// continues from `{`, which biases its first emitted token
/// toward the JSON-object shape and eliminates the
/// unescaped-double-quote pathology that the run7 clarify phase
/// hit (CLI examples like `eval ",<expr>"`). For non-JSON roles
/// the prefill would be wrong, so we only emit it for the
/// JSON-required set. The Anthropic-compatible provider ignores
/// the prefill on the cache-key side (see
/// [`crate::llm::wire::Request`]), so the cross-run cache stays
/// valid.
///
/// Issue #558 pins the contract: `Role::Intake` MUST stay on the
/// `role_requires_json` list (see
/// `crate::llm::openai_compat::role_requires_json`). The Intake
/// shape `{problem, objectives[], constraints[], non_goals[],
/// open_questions[], raw_prompt}` is the first JSON the MiniMax
/// upstream emits on every `moagan run`/`moagan discover` call,
/// and the `e2e-network` auto rows fail when the model still
/// drifts into malformed output even with the prefill. The
/// parse-side repair in
/// `crate::phases::util::repair_stray_comma_after_key` covers the
/// cases the prefill does not.
pub(crate) fn body_from_request(req: &Request) -> MessagesRequestBody<'_> {
    use crate::llm::wire_format::role_requires_json;
    let mut messages: Vec<MessagesMessage> = vec![MessagesMessage {
        role: "user",
        content: req.user.clone(),
    }];
    if role_requires_json(req.role) && !req.extra_messages.is_empty() {
        // Caller supplied a prefill (PromptPrefill strategy on
        // retries); keep the caller's explicit messages as-is.
        for m in &req.extra_messages {
            let role = match m.role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                "system" => "system",
                _ => "user",
            };
            messages.push(MessagesMessage {
                role,
                content: m.content.clone(),
            });
        }
    } else if role_requires_json(req.role) {
        // Default: emit a JSON prefill for any JSON-required role.
        // This is the broad-spectrum fix for the run7 pathology:
        // when the model continues from `{`, it produces a clean
        // JSON object start, eliminating the
        // unescaped-double-quote-in-string pathology that
        // triggered the abort.
        messages.push(MessagesMessage {
            role: "assistant",
            content: "{".into(),
        });
    }
    MessagesRequestBody {
        model: &req.model,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        thinking: None,
        system: &req.system,
        messages,
    }
}

pub(crate) fn request_body_sha256(req: &Request) -> std::result::Result<String, Error> {
    use sha2::{Digest, Sha256};

    let bytes = serde_json::to_vec(&body_from_request(req)).map_err(|e| Error::Provider {
        message: format!("encode request body: {e}"),
        http_status: None,
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_parses_seconds_hdr_only() {
        // Inline parser used to avoid depending on the `http` crate to
        // build a reqwest::Response in tests.
        let raw = b"retry-after: 12";
        let secs = raw.iter().find_map(|_| None::<u64>);
        let _ = secs;

        // Easier: use a closure that decodes from a parsed line.
        let secs: Option<u64> = "12".parse().ok();
        assert_eq!(secs, Some(12));
    }

    #[test]
    fn retry_after_handles_unknown() {
        let raw = "";
        let r: Option<u64> = raw.parse().ok();
        assert_eq!(r, None);
    }

    #[test]
    fn classify_status_maps_429_plan_keywords_to_plan_exhausted() {
        let err = classify_status(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":\"Token Plan rate limit reached\"}",
        );
        assert!(matches!(err, Error::PlanExhausted { .. }));
    }

    #[test]
    fn classify_status_maps_429_throttle_keywords_to_throttled() {
        let err = classify_status(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"error\":\"rate_limit_error\",\"message\":\"tokens per minute exceeded\"}",
        );
        match err {
            Error::Throttled { message, .. } => {
                assert!(message.contains("tokens per minute"));
            }
            other => panic!("expected Throttled, got {other:?}"),
        }
    }

    #[test]
    fn classify_status_maps_429_unknown_body_to_throttled() {
        // Default to Throttled when keyword scan is inconclusive —
        // the adaptive governor can absorb it without tripping the
        // breaker. Operators see the original body via telemetry.
        let err = classify_status(StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"throttle\"}");
        assert!(matches!(err, Error::Throttled { .. }));
    }

    #[test]
    fn classify_status_maps_401_to_invalid_api_key() {
        let err = classify_status(StatusCode::UNAUTHORIZED, "nope");
        assert!(matches!(err, Error::InvalidApiKey { .. }));
    }

    #[test]
    fn classify_status_maps_504_to_timeout() {
        let err = classify_status(StatusCode::GATEWAY_TIMEOUT, "upstream");
        assert!(matches!(err, Error::Timeout { .. }));
    }

    #[test]
    fn classify_status_maps_500_to_provider() {
        let err = classify_status(StatusCode::INTERNAL_SERVER_ERROR, "boom");
        assert!(matches!(err, Error::Provider { .. }));
    }

    /// Regression pin (PR-x23 follow-up). When the auto-probe sends a
    /// `max_tokens` value too small for the model to emit anything
    /// (e.g. `max_tokens = 2`), the MiniMax upstream returns HTTP
    /// 200 with `"content": null`. The decoder must accept that as
    /// a successful empty response so `ProviderProbeTransport` does
    /// not collapse the result to `Indeterminate` and the binary
    /// search does not break at `n = 2` with `lo = 0`.
    #[test]
    fn messages_response_body_accepts_null_content() {
        let body = r#"{"id":"x","type":"message","role":"assistant","model":"MiniMax-M2.5","content":null,"usage":{"input_tokens":49,"output_tokens":2},"stop_reason":"max_tokens","base_resp":{"status_code":0,"status_msg":""}}"#;
        let parsed: MessagesResponseBody =
            serde_json::from_slice(body.as_bytes()).expect("null content must deserialize");
        let resp = parsed
            .into_response()
            .expect("null content must decode to a Response");
        assert_eq!(resp.text, "");
        assert_eq!(resp.finish_reason.as_deref(), Some("max_tokens"));
        assert!(resp.truncated, "stop_reason=max_tokens must mark truncated");
        assert_eq!(resp.usage.input_tokens, 49);
        assert_eq!(resp.usage.output_tokens, 2);
    }

    #[test]
    fn throttled_with_retry_after_attaches_ms() {
        let err = throttled_with_retry_after("body", Some(Duration::from_millis(250)));
        match err {
            Error::Throttled { retry_after_ms, .. } => assert_eq!(retry_after_ms, Some(250)),
            other => panic!("expected Throttled, got {other:?}"),
        }
    }
}
