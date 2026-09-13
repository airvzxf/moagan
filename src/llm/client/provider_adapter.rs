//! [`ProviderLlmClient`] — the bridging adapter that lets call sites
//! that still operate on the legacy [`Provider`](crate::llm::provider::Provider)
//! trait hand an `Arc<dyn LlmClient>` to consumers that have already
//! migrated (issue #925 — the probe subsystem).
//!
//! `ProviderLlmClient` is the **non-breakered** sibling of
//! [`super::breakered::BreakeredClient`]. The wrapping is intentionally
//! minimal: every method delegates to the inner `Provider` after
//! translating the SDK-side [`LlmRequest`] into the legacy
//! [`crate::llm::wire::Request`], and folds the transport status into
//! the SDK-side [`LlmResponse`] via the converters in
//! [`super::conversions`].
//!
//! ## Why a separate adapter
//!
//! The probe subsystem (issue #925) needs `Arc<dyn LlmClient>` to keep
//! the algorithm shape symmetric with future migrations, but the rest
//! of the runtime (`cli/probe.rs`, registry probe spawners, test
//! fixtures) still operates on `Arc<dyn Provider>` and is migrated by
//! later issues (#926 and beyond). `ProviderLlmClient` is the bridge:
//! the CLI builds a `Provider`, wraps it here, and hands the wrapper
//! to the new `LlmClientProbeTransport`. No state, no breaker, no rate
//! limiter — those are added later when the SDK impls land and
//! `BreakeredClient` becomes a thin shim around an SDK impl instead
//! of a `BreakeredProvider`.
//!
//! ## Out of scope
//!
//! - Does not add a breaker, rate limiter, or saturation sink. The
//!   probe deliberately bypasses those layers (a failing probe must
//!   not poison the steady-state circuit); a future migration that
//!   routes production traffic through `LlmClient` will wrap the SDK
//!   impl in `BreakeredClient` directly and skip this adapter.
//! - Does **not** delete `Provider` or its converters. Issue #933
//!   performs the unified deletion.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::llm::capabilities::ProviderCapabilities;
use crate::llm::client::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};
use crate::llm::http::request_body_sha256;
use crate::llm::provider::Provider;

/// Adapter that mirrors a raw [`Provider`] but speaks
/// [`LlmClient`]. The struct holds the provider `Arc` directly so
/// every transport-side wiring the inner provider already has
/// (auth header injection, retry policy, ...) carries through
/// untouched.
///
/// `Send + Sync` is satisfied by the `Arc<dyn Provider>` field, so
/// the wrapper can live inside another `Arc` for the trait-object
/// dispatch.
pub struct ProviderLlmClient {
    inner: Arc<dyn Provider>,
}

impl ProviderLlmClient {
    /// Build an adapter around the supplied provider. The provider
    /// is the same `Arc<dyn Provider>` the registry / CLI builds
    /// today, so the adapter is a pure type-shape bridge — every
    /// state lives on the inner provider and the adapter's methods
    /// just delegate.
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        Self { inner }
    }

    /// Borrow the underlying provider. Useful for tests that want
    /// to inspect call counts or for callers that need an escape
    /// hatch back to the legacy trait after wrapping.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.inner
    }
}

#[async_trait]
impl LlmClient for ProviderLlmClient {
    fn sdk_type(&self) -> &'static str {
        // SDK identifier routed on by the URL-path dispatcher
        // (#922). Mirrors `BreakeredClient::sdk_type` so both
        // adapters surface the same identifier to telemetry.
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
        // preference rather than the OpenAI-compat default.
        LlmCapabilities(ProviderCapabilities::clone(&self.inner.capabilities()))
    }

    fn effective_max_tokens(&self, req: &LlmRequest) -> u32 {
        // Mirror of the cap chain in `Provider::send` so the
        // audit-log hash stays byte-for-byte identical to the wire
        // body the legacy `send` would have emitted.
        let legacy_req: crate::llm::wire::Request = req.into();
        self.inner.effective_max_tokens(&legacy_req)
    }

    async fn send(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Convert the SDK request to the legacy wire shape, run
        // through the inner provider, then fold the transport
        // status into the SDK response via the canonical converter.
        let legacy_req: crate::llm::wire::Request = req.into();
        let (status, legacy_resp) = self.inner.send(&legacy_req).await?;
        Ok(LlmResponse::from((status, legacy_resp)))
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Probe path: the inner provider's `send_probe` skips the
        // per-call safety clamp so the auto-probe sees the
        // upstream's real boundary. Mirror the same behaviour here
        // so the bridge carries the probe intent into the legacy
        // call without rewriting it.
        let legacy_req: crate::llm::wire::Request = req.into();
        let (status, legacy_resp) = self.inner.send_probe(&legacy_req).await?;
        Ok(LlmResponse::from((status, legacy_resp)))
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // D8 invariant: the wire body `send` will transmit is the
        // exact byte sequence the caller hashes here. The legacy
        // `send` mutates `req.max_tokens` only via the safety
        // clamp, and that clamp is mirrored in
        // `effective_max_tokens`; the SHA of the converted
        // request matches the wire body up to the per-provider
        // cap.
        let legacy_req: crate::llm::wire::Request = req.into();
        request_body_sha256(&legacy_req)
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Delegate so the adapter is transparent and the probe
        // does not waste round-trips on values the inner provider
        // will reject.
        self.inner.max_tokens_probe_ceiling()
    }

    async fn count_tokens(&self, text: &str) -> Option<u64> {
        // Delegate so the adapter is transparent.
        self.inner.count_tokens(text).await
    }
}

#[cfg(test)]
mod tests {
    //! `ProviderLlmClient` is exercised end-to-end through the probe
    //! transport tests in `src/llm/probe.rs::tests` and
    //! `src/llm/temperature_probe.rs::tests` — those tests build the
    //! adapter around a `MockProvider` and run the real probe
    //! algorithm against it, which is the highest-fidelity check
    //! that the bridge preserves semantics.

    use super::*;

    /// Smoke: `ProviderLlmClient::new` accepts any
    /// `Arc<dyn Provider>` and exposes a `provider()` accessor that
    /// hands the same Arc back. Pins the bridge's identity guarantee.
    #[test]
    fn new_returns_adapter_with_provider_accessor() {
        use crate::llm::mock::MockProvider;
        let mock: Arc<dyn Provider> = Arc::new(MockProvider::empty());
        let adapter = ProviderLlmClient::new(Arc::clone(&mock));
        assert!(
            Arc::ptr_eq(adapter.provider(), &mock),
            "provider() must hand back the same Arc the wrapper was built from"
        );
    }
}
