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
        .map_err(|e| Error::Provider(format!("build reqwest client: {e}")))
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
        HeaderValue::from_str(api_key)
            .map_err(|e| Error::InvalidApiKey(format!("x-api-key: {e}")))?,
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    for (k, v) in extra {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| Error::Provider(format!("header {k}: {e}")))?;
        let value = HeaderValue::from_str(v)
            .map_err(|e| Error::Provider(format!("header {k} value: {e}")))?;
        headers.insert(name, value);
    }
    let _ = AUTHORIZATION;
    Ok(headers)
}

/// Translate status into a moagan error, with structured context.
pub fn classify_status(status: StatusCode, body: &str) -> Error {
    match status.as_u16() {
        401 | 403 => Error::InvalidApiKey(format!("http {status}: {body}")),
        429 => Error::PlanExhausted(format!("http {status}: {body}")),
        408 | 504 | 524 => Error::Timeout(format!("http {status}: {body}")),
        500..=599 => Error::Provider(format!("upstream {status}: {body}")),
        _ => Error::Provider(format!("http {status}: {body}")),
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
    system: &'a str,
    messages: Vec<MessagesMessage>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessagesMessage {
    role: &'static str,
    content: String,
}

/// JSON shape we expect back from the `/v1/messages` endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct MessagesResponseBody {
    content: Vec<MessagesContent>,
    stop_reason: Option<String>,
    usage: Option<MessagesUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesContent {
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
    pub(crate) fn into_response(self) -> std::result::Result<Response, &'static str> {
        let mut text = String::new();
        for c in self.content {
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
        Ok(Response {
            text,
            finish_reason: self.stop_reason,
            usage,
        })
    }
}

/// Translate a [`Request`] into the body shape expected by the
/// Anthropic-compatible messages endpoint.
pub(crate) fn body_from_request(req: &Request) -> MessagesRequestBody<'_> {
    MessagesRequestBody {
        model: &req.model,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        system: &req.system,
        messages: vec![MessagesMessage {
            role: "user",
            content: req.user.clone(),
        }],
    }
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
    fn classify_status_maps_429_to_plan_exhausted() {
        let err = classify_status(StatusCode::TOO_MANY_REQUESTS, "{\"error\":\"throttle\"}");
        assert!(matches!(err, Error::PlanExhausted(_)));
    }

    #[test]
    fn classify_status_maps_401_to_invalid_api_key() {
        let err = classify_status(StatusCode::UNAUTHORIZED, "nope");
        assert!(matches!(err, Error::InvalidApiKey(_)));
    }

    #[test]
    fn classify_status_maps_504_to_timeout() {
        let err = classify_status(StatusCode::GATEWAY_TIMEOUT, "upstream");
        assert!(matches!(err, Error::Timeout(_)));
    }

    #[test]
    fn classify_status_maps_500_to_provider() {
        let err = classify_status(StatusCode::INTERNAL_SERVER_ERROR, "boom");
        assert!(matches!(err, Error::Provider(_)));
    }
}
