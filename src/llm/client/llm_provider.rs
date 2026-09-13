//! [`LlmClientProvider`] — the **reverse** bridge adapter that lets
//! CLI call sites (issue #926) hand an `Arc<dyn LlmClient>` back to
//! the legacy [`Provider`](crate::llm::provider::Provider) tree.
//!
//! The companion to [`super::provider_adapter::ProviderLlmClient`]:
//! where that adapter goes `Provider → LlmClient`, this one goes
//! `LlmClient → Provider`. The CLI uses both adapters at different
//! boundaries:
//!
//! - `ProviderLlmClient` wraps a freshly-built `Provider` so the
//!   probe subsystem (issue #925) can consume it via the SDK trait.
//! - `LlmClientProvider` wraps a freshly-built `LlmClient` so the
//!   legacy `ProviderRegistry` (returned by
//!   [`crate::cli::run::build_registry_for_with_active`]) can
//!   insert it under the registry's joined key. The rest of the
//!   run pipeline still operates on `Arc<dyn Provider>`; this
//!   adapter is the bridge until the registry flips to the SDK
//!   trait in a later migration wave.
//!
//! Like the forward adapter, the wrapper is **temporary**: issue
//! #933 deletes the legacy `Provider` trait, and with it the
//! registry insertion site that needs this adapter. Until then
//! the adapter is a pure type-shape bridge — every state lives on
//! the inner SDK impl (auth header, retry policy, wire body) and
//! the adapter's methods just translate the wire types.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::llm::capabilities::ProviderCapabilities;
use crate::llm::provider::Provider;
use crate::llm::wire::{Request, Response};

/// Adapter that mirrors an [`LlmClient`](super::LlmClient) but
/// speaks the legacy [`Provider`] trait. Used by the CLI's
/// `--api-key` short-circuit (issue #926) so a dispatcher-built
/// SDK can be inserted into the legacy registry until the
/// registry flips to the SDK trait in a later migration wave.
pub struct LlmClientProvider {
    inner: Arc<dyn super::LlmClient>,
}

impl LlmClientProvider {
    /// Build an adapter around the supplied SDK client. The inner
    /// `Arc<dyn LlmClient>` is the same object the dispatcher
    /// returns from
    /// [`super::dispatcher::build_client`](super::dispatcher::build_client)
    /// so the adapter is a pure type-shape bridge — every SDK
    /// state (auth header injection, retry policy, wire body)
    /// carries through untouched.
    pub fn new(inner: Arc<dyn super::LlmClient>) -> Self {
        Self { inner }
    }

    /// Borrow the underlying SDK client. Useful for tests that want
    /// to inspect call counts or for callers that need an escape
    /// hatch back to the SDK trait after wrapping.
    pub fn client(&self) -> &Arc<dyn super::LlmClient> {
        &self.inner
    }
}

#[async_trait]
impl Provider for LlmClientProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Delegate so telemetry sees the inner SDK's exact
        // preference rather than the OpenAI-compat default. The
        // adapter is transparent — the wire-format choice was made
        // when the SDK was built by the dispatcher, and that
        // decision propagates through the adapter untouched.
        // `ProviderCapabilities` is `Copy` so the wrapper copies the
        // inner field directly without a clone allocation.
        self.inner.capabilities().0
    }

    fn wire_format_id(&self) -> &'static str {
        // Delegate so the wire-format identifier stays in sync with
        // the SDK's `sdk_type()`. Mirrors the SDK contract for
        // telemetry consumers that key off `wire_format_id`.
        self.inner.sdk_type()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        // Convert the legacy wire shape into the SDK request, run
        // through the inner SDK, then split the SDK response back
        // into the `(status, body)` tuple the legacy registry
        // expects. The conversions live in
        // [`super::conversions`] so both directions share a single
        // source of truth.
        let sdk_req: super::LlmRequest = req.into();
        let sdk_resp = self.inner.send(&sdk_req).await?;
        Ok((&sdk_resp).into())
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Mirror the cap chain the SDK runs against
        // `req.max_tokens` so the audit-log hash stays byte-for-byte
        // identical to the wire body `send` will transmit.
        let sdk_req: super::LlmRequest = req.into();
        self.inner.effective_max_tokens(&sdk_req)
    }

    async fn send_probe(&self, req: &Request) -> Result<(u16, Response)> {
        // Probe path: the SDK's `send_probe` skips the per-call
        // safety clamp so the auto-probe sees the upstream's real
        // boundary. Mirror the same behaviour here so the bridge
        // carries the probe intent into the SDK call without
        // rewriting it.
        let sdk_req: super::LlmRequest = req.into();
        let sdk_resp = self.inner.send_probe(&sdk_req).await?;
        Ok((&sdk_resp).into())
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Delegate so the adapter is transparent and the probe
        // does not waste round-trips on values the inner SDK will
        // reject.
        self.inner.max_tokens_probe_ceiling()
    }

    async fn count_tokens(&self, text: &str) -> Option<u64> {
        // Delegate so the adapter is transparent.
        self.inner.count_tokens(text).await
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests pinning the adapter's identity, accessor, and
    //! type-shape bridge. End-to-end coverage of the conversion
    //! logic lives in [`super::conversions`] tests; this module
    //! only pins the wrapper surface.

    use super::super::LlmClient;
    use super::*;
    use crate::llm::client::mock::MockClient;

    /// Smoke: `LlmClientProvider::new` accepts any
    /// `Arc<dyn LlmClient>` and exposes a `client()` accessor that
    /// hands the same Arc back. Pins the bridge's identity
    /// guarantee — symmetric with
    /// [`super::provider_adapter::ProviderLlmClient::new`].
    #[test]
    fn new_returns_adapter_with_client_accessor() {
        let client: Arc<dyn LlmClient> = Arc::new(MockClient::empty());
        let adapter = LlmClientProvider::new(Arc::clone(&client));
        assert!(
            Arc::ptr_eq(adapter.client(), &client),
            "client() must hand back the same Arc the wrapper was built from"
        );
    }

    /// `name()`, `model()`, `endpoint()` all delegate to the inner
    /// SDK without modification.
    #[test]
    fn name_model_endpoint_delegate_to_inner() {
        let client: Arc<dyn LlmClient> = Arc::new(MockClient::empty());
        let adapter = LlmClientProvider::new(client);
        assert_eq!(adapter.name(), "mock");
        assert_eq!(adapter.model(), "mock-model");
        assert_eq!(adapter.endpoint(), "mock://local");
    }

    /// `wire_format_id()` delegates to the inner SDK's
    /// `sdk_type()`, keeping the legacy telemetry accessor in sync
    /// with the new SDK identifier.
    #[test]
    fn wire_format_id_delegates_to_sdk_type() {
        let client: Arc<dyn LlmClient> = Arc::new(MockClient::empty());
        let adapter = LlmClientProvider::new(client);
        assert_eq!(adapter.wire_format_id(), "mock");
    }

    /// `max_tokens_probe_ceiling()` delegates to the inner SDK so
    /// the probe algorithm stays transparent across the bridge.
    #[test]
    fn max_tokens_probe_ceiling_delegates_to_inner() {
        let client: Arc<dyn LlmClient> = Arc::new(MockClient::empty());
        let adapter = LlmClientProvider::new(client);
        // Mock SDK's default is `u32::MAX` — pin the bridge
        // delegates that value rather than re-deriving its own.
        assert_eq!(adapter.max_tokens_probe_ceiling(), u32::MAX);
    }
}
