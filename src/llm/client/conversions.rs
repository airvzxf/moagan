//! Bridge converters between the legacy `Request`/`Response` wire
//! types and the new SDK-side `LlmRequest`/`LlmResponse`.
//!
//! These converters are a temporary layer: issue #933 deletes the
//! legacy `Provider`/`Request`/`Response` types wholesale, and with
//! them every `From` impl in this file. Until then the converters
//! keep the migration non-breaking — every SDK impl can keep
//! speaking `Provider::send` internally while the public dispatch
//! surface speaks `LlmClient::send`.
//!
//! The single additive change between `LlmRequest` and the legacy
//! `Request` is `LlmRequest::top_k` (introduced in #919 for #920).
//! The legacy type has no `top_k` field, so the `LlmRequest →
//! Request` direction drops it. Every other field maps 1:1 so a
//! `Request → LlmRequest` round-trip preserves byte-for-byte
//! fidelity (other than `top_k`).
//!
//! The `Response → LlmResponse` direction folds the transport
//! `http_status` (carried as a tuple element on
//! `Provider::send`) into `LlmResponse::http_status` so callers
//! only deal with a single return value.

use crate::llm::wire::{Request, Response};

use super::{LlmRequest, LlmResponse};

impl From<&LlmRequest> for Request {
    fn from(req: &LlmRequest) -> Self {
        // Drop `top_k` (the single additive change in #919); every
        // other field maps 1:1. The legacy `Request` serialises to
        // the wire body the dispatcher hashes, and `top_k` is
        // already absent from the legacy type so the round-trip
        // preserves byte-for-byte fidelity.
        Self {
            role: req.role,
            model: req.model.clone(),
            system: req.system.clone(),
            user: req.user.clone(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            response_schema: req.response_schema.clone(),
            stream: req.stream,
            extra_messages: req.extra_messages.clone(),
            attachments: req.attachments.clone(),
            tool_choice: req.tool_choice.clone(),
        }
    }
}

impl From<&Request> for LlmRequest {
    fn from(req: &Request) -> Self {
        // Mirror of the `From<&LlmRequest> for Request` impl above.
        // The legacy `Request` has no `top_k` field, so the
        // `Request → LlmRequest` direction sets it to `None` —
        // preserving byte-for-byte fidelity with the pre-#919
        // wire body (any future `top_k` injection happens at the
        // `LlmRequest` construction site, never at the legacy
        // `Request` boundary). Used by [`crate::phases::phase::
        // RunContext::dispatch_to_provider`] to convert the
        // gate-mutated `Request` into the SDK shape the
        // [`LlmClient`] impls consume.
        Self {
            role: req.role,
            model: req.model.clone(),
            system: req.system.clone(),
            user: req.user.clone(),
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: req.top_p,
            top_k: None,
            response_schema: req.response_schema.clone(),
            stream: req.stream,
            extra_messages: req.extra_messages.clone(),
            attachments: req.attachments.clone(),
            tool_choice: req.tool_choice.clone(),
        }
    }
}

impl From<&LlmResponse> for Response {
    fn from(resp: &LlmResponse) -> Self {
        // Inverse of the `From<(u16, Response)> for LlmResponse`
        // impl: drops `http_status` (the SDK layer folds it into
        // the response for the call surface, but the legacy
        // persistence layer — `Cache::store`,
        // `RecordCacheHit`-style helpers — expects the
        // transport-agnostic `Response` shape). Used at the
        // boundary between the new SDK-shape dispatch and the
        // legacy persistence path so the cache entry stays
        // byte-identical to the pre-#923 schema.
        Self {
            text: resp.text.clone(),
            finish_reason: resp.finish_reason.clone(),
            truncated: resp.truncated,
            usage: resp.usage.clone(),
        }
    }
}

impl From<(u16, Response)> for LlmResponse {
    fn from((http_status, response): (u16, Response)) -> Self {
        // Fold the transport status into the SDK response so callers
        // only deal with a single return value. The named method
        // `LlmResponse::from_parts` (kept on the `LlmResponse`
        // impl block in `client/mod.rs` for source compatibility
        // with the SDK impls) delegates here, so the conversion
        // behaviour has a single source of truth.
        Self {
            text: response.text,
            finish_reason: response.finish_reason,
            truncated: response.truncated,
            usage: response.usage,
            http_status,
        }
    }
}

/// Inverse of `From<(u16, Response)> for LlmResponse`: split an
/// `LlmResponse` back into the `(status, body)` shape the probe
/// algorithm's classifier consumes (`src/llm/temperature_probe.rs::
/// classify_probe_response` and the 4xx-vs-2xx branches of
/// `src/lm/probe.rs::probe_send_with_body`). Routes through the
/// two existing converters so a future field added to either side
/// surfaces here for free.
impl From<&LlmResponse> for (u16, Response) {
    fn from(resp: &LlmResponse) -> Self {
        let r: Response = resp.into();
        (resp.http_status, r)
    }
}

/// Move-based companion of [`From<&LlmResponse> for (u16, Response)`].
/// Lets the probe transport's
/// `let (status, body): (u16, Response) = resp.into();` site use
/// the owned `LlmResponse` directly without borrowing.
impl From<LlmResponse> for (u16, Response) {
    fn from(resp: LlmResponse) -> Self {
        (&resp).into()
    }
}
