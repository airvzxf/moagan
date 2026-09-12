//! [`BreakeredClient`] — the temporary adapter (#923) that lets
//! `RunContext::llm_client()` speak [`LlmClient`] while every
//! concrete impl still implements the legacy [`Provider`] trait.
//!
//! The adapter wraps an `Arc<BreakeredProvider>` (looked up from
//! the [`ProviderRegistry::get_wrapped`] map) and forwards every
//! call to the inner wrapper. Because the inner wrapper already
//! owns the breaker / rate-limiter / semaphore / saturation-sink
//! layer, the adapter is a thin type-shape bridge — no extra
//! state, no duplicated wiring.
//!
//! The adapter is **temporary**:
//!
//! - Issue #933 deletes the legacy `Provider` trait and the
//!   `BreakeredProvider` wrapper.
//! - At that point `RunContext::llm_client()` returns the SDK
//!   directly (`AnthropicClient` / `OpenAIClient` / `MockClient`)
//!   and this module becomes dead code, deleted in the same PR.
//!
//! Until then, every LLM call routed through
//! `RunContext::llm_client()` passes through this adapter — the
//! wrapper layer (breaker, rate limiter, etc.) is preserved
//! because the inner `Arc<BreakeredProvider>` is the same object
//! the legacy `provider()` accessor returns, just upcast to
//! `Arc<dyn Provider>`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::llm::client::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};
use crate::llm::http::request_body_sha256;
use crate::llm::provider::{BreakeredProvider, Provider};

/// Adapter that mirrors [`BreakeredProvider`] but speaks
/// [`LlmClient`]. The struct holds the wrapper `Arc` directly so
/// the wrapper layer (breaker, rate-limiter, semaphore,
/// saturation-sink) stays in front of every `send`.
pub struct BreakeredClient {
    inner: Arc<BreakeredProvider>,
}

impl BreakeredClient {
    /// Build an adapter around the supplied wrapper. The wrapper
    /// is the same `Arc<BreakeredProvider>` the registry stores,
    /// so the adapter is a pure type-shape bridge — every state
    /// (breaker counter, rate-limiter bucket, …) lives on the
    /// inner wrapper and the adapter's methods just delegate.
    pub fn new(inner: Arc<BreakeredProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl LlmClient for BreakeredClient {
    fn sdk_type(&self) -> &'static str {
        // SDK identifier routed on by the URL-path dispatcher
        // (#922). Mirrors `BreakeredProvider::wire_format_id` (a
        // default that delegates to `inner.capabilities().wire_
        // format_id()`) so the SDK trait and the registry agree
        // on the literal.
        self.inner.wire_format_id()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    fn capabilities(&self) -> LlmCapabilities {
        // Delegate so telemetry sees the inner provider's exact
        // preference rather than the OpenAI-compat default. The
        // wrapper is transparent: the wire-format choice was made
        // when the inner provider was built, and that decision
        // propagates through the wrapper untouched.
        LlmCapabilities(self.inner.capabilities())
    }

    fn effective_max_tokens(&self, req: &LlmRequest) -> u32 {
        // Mirror of the cap chain in
        // `BreakeredProvider::send` so the audit-log hash is
        // byte-for-byte identical to the wire body. The wrapper
        // already threads the wrapper-level `max_tokens_table`
        // (set by the registry) and the inner provider's
        // `effective_max_tokens`, so a single delegation here
        // preserves both.
        let legacy_req: crate::llm::wire::Request = req.into();
        self.inner.effective_max_tokens(&legacy_req)
    }

    async fn send(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Convert the SDK request to the legacy wire shape, run
        // it through the wrapper (breaker / rate-limiter /
        // semaphore / saturation-sink stay in front), then fold
        // the transport status into the SDK response.
        let legacy_req: crate::llm::wire::Request = req.into();
        let (status, legacy_resp) = self.inner.send(&legacy_req).await?;
        Ok(LlmResponse::from((status, legacy_resp)))
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // D8 invariant: the wire body `send` will transmit is
        // the exact byte sequence the caller hashes here. The
        // wrapper's `send` does NOT mutate the legacy `Request`
        // (it only consults `effective_max_tokens` and reads the
        // body), so the SHA of the converted request matches
        // the wire body up to the per-provider cap (which is
        // already mirrored in `effective_max_tokens`).
        let legacy_req: crate::llm::wire::Request = req.into();
        request_body_sha256(&legacy_req)
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Delegate to the inner provider so the wrapper stays
        // transparent. Without the delegation the wrapper would
        // inherit the SDK default (`u32::MAX`) and the probe
        // would waste round-trips on values the inner provider
        // will never accept.
        self.inner.max_tokens_probe_ceiling()
    }

    async fn count_tokens(&self, text: &str) -> Option<u64> {
        // Delegate so the wrapper is transparent.
        self.inner.count_tokens(text).await
    }
}
