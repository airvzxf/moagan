//! D.12.11: `LlmError` companion with `RetryAdvice`. Companion type
//! to `Error::Provider(_)` that callers can attach via
//! `Error::context(llm_error)` when the provider error needs an
//! explicit retry policy instead of the default `is_retriable()`
//! lookup.
//!
//! The `RetryAdvice` enum captures the four policies the spec
//! recognises:
//! - `Retry`: re-issue the request immediately (transport
//!   blip, transient hiccup).
//! - `DoNotRetry`: the provider has rejected the input;
//!   re-issuing will not change the outcome. Examples:
//!   schema violation, bad API key, cancellation.
//! - `RetryAfterBackoff`: the provider asked us to slow
//!   down (HTTP 429, rate-limit response). The caller
//!   should consult the provider's `Retry-After` header
//!   when present.
//! - `SwitchProvider`: the breaker / orchestrator should
//!   route the next attempt to a different provider
//!   (persistent 5xx, repeated auth failure, repeated
//!   schema violation).
//!
//! The companion never replaces the public `Error` enum; it
//! only enriches the caller's decision-making when they
//! already have a `Provider(_)` variant in hand.

use serde::Serialize;

/// Recommended retry policy for an LLM-related failure.
///
/// `Default` is `Retry` so that a freshly constructed
/// `RetryAdvice` is the safe optimistic default — callers
/// that need a stricter policy must opt-in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum RetryAdvice {
    /// Re-issue the request immediately.
    #[default]
    Retry,
    /// Do not retry; the outcome will not change.
    DoNotRetry,
    /// Retry after the recommended backoff window.
    RetryAfterBackoff,
    /// Switch to a different provider before retrying.
    SwitchProvider,
}

/// Companion error type that augments `Error::Provider(_)` with
/// retry advice. Lives as a separate type so the public `Error`
/// enum remains stable while callers can attach richer context
/// when they need it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LlmError {
    /// Wire-form error code (e.g. `INVALID_RESPONSE`,
    /// `QUOTA_EXCEEDED`). Mirrors `ErrorCode::stable()`.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Retry policy the caller should follow.
    pub retry: RetryAdvice,
}

impl LlmError {
    /// Build a new `LlmError` with the default retry policy.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retry: RetryAdvice::default(),
        }
    }

    /// Build a new `LlmError` with an explicit retry policy.
    pub fn with_retry(
        code: impl Into<String>,
        message: impl Into<String>,
        retry: RetryAdvice,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retry,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} (retry: {:?})",
            self.code, self.message, self.retry
        )
    }
}

impl std::error::Error for LlmError {}
