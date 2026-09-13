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
use parking_lot::Mutex;

use crate::error::Result;
use crate::llm::client::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};
use crate::llm::http::request_body_sha256;
use crate::llm::param_rejections::ParamRejectionsTable;
use crate::llm::provider::{BreakeredProvider, Provider};

/// Adapter that mirrors [`BreakeredProvider`] but speaks
/// [`LlmClient`]. The struct holds the wrapper `Arc` directly so
/// the wrapper layer (breaker, rate-limiter, semaphore,
/// saturation-sink) stays in front of every `send`.
///
/// #932 (D9) — `BreakeredClient` carries the run-level
/// param-rejections table behind interior mutability so the cascade
/// default impl of [`LlmClient::send`] can consult it via
/// [`LlmClient::param_rejections_table`]. The dispatcher injects the
/// table via [`LlmClient::set_param_rejections`] before the first
/// `send`. This is the adapter that owns the cascade in production:
/// every SDK impl that goes through `RunContext::llm_client()` lands
/// on a `BreakeredClient`, so wiring the cascade here closes the
/// loop for the full `Provider`-migration surface.
///
/// The adapter also carries an optional **cascade context** —
/// `(name, model)` the cascade uses as the key for the
/// `param_rejections` table. The dispatcher's section/model is the
/// operator-facing key the table is persisted under, so we wrap
/// the `BreakeredClient` with it via [`Self::with_cascade_context`]
/// at construction time. When the context is `None`, the cascade
/// falls back to the inner provider's `name()`/`model()` (which
/// matches the section/model for production SDK impls built via
/// `AnthropicClient::from_resolved` etc., so the production path
/// needs no explicit wiring — the context is only an override for
/// test stubs that hardcode a different inner name).
pub struct BreakeredClient {
    inner: Arc<BreakeredProvider>,
    /// Optional param-rejection table the cascade default impl
    /// consults. `None` keeps the adapter on the pre-#932
    /// straight-send path. The `Mutex` is interior-mutable so the
    /// dispatcher's setter can write without `&mut self`.
    param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,
    /// Optional cascade context — `(name, model)` the cascade
    /// uses as the key for the param-rejections table. When
    /// `Some`, [`Self::name`] / [`Self::model`] return the
    /// override; when `None` they delegate to the inner provider.
    /// Stored as direct `String` fields (not behind a lock) so
    /// `name()`/`model()` can return `&str` borrowing `self`
    /// without the temporary-lifetime gymnastics a `MutexGuard`
    /// would require.
    cascade_name_override: Option<String>,
    cascade_model_override: Option<String>,
}

impl BreakeredClient {
    /// Build an adapter around the supplied wrapper. The wrapper
    /// is the same `Arc<BreakeredProvider>` the registry stores,
    /// so the adapter is a pure type-shape bridge — every state
    /// (breaker counter, rate-limiter bucket, …) lives on the
    /// inner wrapper and the adapter's methods just delegate.
    pub fn new(inner: Arc<BreakeredProvider>) -> Self {
        Self {
            inner,
            param_rejections: Mutex::new(None),
            cascade_name_override: None,
            cascade_model_override: None,
        }
    }

    /// Override the cascade context — the `(name, model)` pair the
    /// SDK impl's cascade keys the param-rejections table on. The
    /// dispatcher installs this when it knows the operator-facing
    /// section/model differs from the inner provider's identifier
    /// (e.g. test stubs that hardcode `name() = "retry-script"`
    /// while the dispatch's section is `"retry"`). When unset
    /// (the production default), the cascade uses the inner
    /// provider's `name()`/`model()` — which already equals the
    /// section/model for SDK impls built via
    /// `AnthropicClient::from_resolved` etc.
    pub fn with_cascade_context(self, name: String, model: String) -> Self {
        Self {
            cascade_name_override: Some(name),
            cascade_model_override: Some(model),
            ..self
        }
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
        // When the cascade context is set (the dispatcher
        // installed it via `with_cascade_context`), return the
        // override so the SDK impl's cascade keys the
        // `param_rejections` table on the operator-facing
        // section name. Otherwise delegate to the inner provider
        // — for production SDK impls (`AnthropicClient`,
        // `OpenAIClient` built via `from_resolved`) the inner
        // `name()` IS the section, so production needs no
        // explicit wiring.
        if let Some(name) = self.cascade_name_override.as_deref() {
            return name;
        }
        self.inner.name()
    }

    fn model(&self) -> &str {
        // Mirror of [`Self::name`]`: the cascade context's
        // `model` wins over the inner provider's `model()` when
        // the dispatch installed an override.
        if let Some(model) = self.cascade_model_override.as_deref() {
            return model;
        }
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

    async fn send_once(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Convert the SDK request to the legacy wire shape, run
        // it through the wrapper (breaker / rate-limiter /
        // semaphore / saturation-sink stay in front), then fold
        // the transport status into the SDK response.
        let legacy_req: crate::llm::wire::Request = req.into();
        let (status, legacy_resp) = self.inner.send(&legacy_req).await?;
        Ok(LlmResponse::from((status, legacy_resp)))
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Probe path: bypass the cascade (we use `send_probe` on
        // the inner wrapper, which itself is the legacy
        // Provider::send_probe — typically a `safety_clamp=false`
        // HTTP call that returns the upstream's real boundary
        // instead of the dispatcher-clobbered value). Delegates
        // to `inner.send_probe` so the breaker / rate-limiter /
        // semaphore layer stays in front and the wrapper's
        // saturation-sink contract holds.
        let legacy_req: crate::llm::wire::Request = req.into();
        let (status, legacy_resp) = self.inner.send_probe(&legacy_req).await?;
        Ok(LlmResponse::from((status, legacy_resp)))
    }

    fn param_rejections_table(&self) -> Option<Arc<ParamRejectionsTable>> {
        self.param_rejections.lock().clone()
    }

    fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>) {
        *self.param_rejections.lock() = Some(table);
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
