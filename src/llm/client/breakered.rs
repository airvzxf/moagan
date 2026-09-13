//! [`BreakeredClient`] — the post-#933 circuit-breaker wrapper.
//!
//! Wraps any [`LlmClient`] SDK impl with the breaker / rate-limiter
//! / saturation-sink layer that used to live on `BreakeredProvider`.
//! The dispatcher (`phases::phase::RunContext::llm_client`) always
//! hands callers a `BreakeredClient`, so every SDK impl routed
//! through `RunContext` goes through the breaker transparently.
//!
//! Before this PR `BreakeredClient` was an adapter that delegated
//! to `BreakeredProvider`; the legacy `Provider` trait tree was
//! deleted as part of issue #933, so the wrapper now holds an
//! `Arc<dyn LlmClient>` directly. Behaviour parity:
//!
//! - `sdk_type` / `name` / `model` / `endpoint` / `capabilities`
//!   delegate to the inner SDK impl (the inner impl IS the SDK).
//! - `send` / `send_probe` delegate to the inner SDK impl. The
//!   cascade lives on the trait default impl of
//!   `LlmClient::send` so every wrapper participates in it
//!   transparently.
//! - `param_rejections_table` / `set_param_rejections` carry the
//!   run-level cascade table behind interior mutability so the
//!   dispatcher's setter can write without `&mut self`.
//! - `body_sha256` / `effective_max_tokens` / `max_tokens_probe_ceiling`
//!   delegate so the audit-log hash is byte-for-byte identical to
//!   the wire body `send` transmits.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::error::Result;
use crate::llm::http::request_body_sha256;
use crate::llm::param_rejections::ParamRejectionsTable;

use super::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};

/// Circuit-breaker wrapper around any [`LlmClient`] SDK impl.
///
/// The wrapper holds an `Arc<dyn LlmClient>` directly (no
/// intermediate `BreakeredProvider`) so the breaker / rate-limiter
/// / cascade layer sits in front of every SDK impl without the
/// dispatcher needing to know.
///
/// The wrapper also carries an optional **cascade context** —
/// `(name, model)` the cascade uses as the key for the
/// `param_rejections` table. The dispatcher's section/model is the
/// operator-facing key the table is persisted under, so we wrap the
/// `BreakeredClient` with it via [`Self::with_cascade_context`] at
/// construction time. When the context is `None`, the cascade
/// falls back to the inner client's `name()`/`model()`.
pub struct BreakeredClient {
    inner: Arc<dyn LlmClient>,
    /// Optional param-rejection table the cascade default impl
    /// consults. `None` keeps the wrapper on the straight-send path.
    /// The `Mutex` is interior-mutable so the dispatcher's setter
    /// can write without `&mut self`.
    param_rejections: Mutex<Option<Arc<ParamRejectionsTable>>>,
    /// Optional cascade context — `(name, model)` the cascade
    /// uses as the key for the param-rejections table. When
    /// `Some`, [`Self::name`] / [`Self::model`] return the
    /// override; when `None` they delegate to the inner client.
    /// Stored as direct `String` fields (not behind a lock) so
    /// `name()`/`model()` can return `&str` borrowing `self`
    /// without the temporary-lifetime gymnastics a `MutexGuard`
    /// would require.
    cascade_name_override: Option<String>,
    cascade_model_override: Option<String>,
    /// Optional saturation sink the registry installs via
    /// [`LlmClient::attach_saturation_sink`]. `None` keeps the
    /// wrapper on the no-sink path.
    saturation_sink: Mutex<Option<Arc<dyn super::compat::SaturationSink>>>,
}

impl BreakeredClient {
    /// Build a wrapper around the supplied SDK impl. The inner
    /// client is held behind `Arc<dyn LlmClient>` so the wrapper
    /// itself can be cloned cheaply and shared across the run
    /// process.
    pub fn new(inner: Arc<dyn LlmClient>) -> Self {
        Self {
            inner,
            param_rejections: Mutex::new(None),
            cascade_name_override: None,
            cascade_model_override: None,
            saturation_sink: Mutex::new(None),
        }
    }

    /// Install a saturation sink (legacy `BreakeredProvider::with_saturation_sink`
    /// shim). Mirrors the post-#933 surface — the SDK impl
    /// itself does not own the sink; the wrapper around it does.
    pub fn with_saturation_sink(&self, sink: Arc<dyn super::compat::SaturationSink>) {
        *self.saturation_sink.lock() = Some(sink);
    }

    /// Override the cascade context — the `(name, model)` pair the
    /// SDK impl's cascade keys the param-rejections table on. The
    /// dispatcher installs this when it knows the operator-facing
    /// section/model differs from the inner client's identifier.
    /// When unset (the production default), the cascade uses the
    /// inner client's `name()`/`model()`.
    pub fn with_cascade_context(self, name: String, model: String) -> Self {
        Self {
            cascade_name_override: Some(name),
            cascade_model_override: Some(model),
            ..self
        }
    }

    /// Read-only access to the inner SDK impl. Used by
    /// integration tests that need to inspect the inner state
    /// (e.g. `call_count()` on a `ScriptedLlmClient`).
    #[cfg(test)]
    pub fn inner(&self) -> &Arc<dyn LlmClient> {
        &self.inner
    }
}

#[async_trait]
impl LlmClient for BreakeredClient {
    fn as_breakered(&self) -> Option<&super::BreakeredClient> {
        Some(self)
    }

    fn attach_saturation_sink(&self, sink: Arc<dyn super::compat::SaturationSink>) {
        self.with_saturation_sink(sink);
    }

    fn sdk_type(&self) -> &'static str {
        self.inner.sdk_type()
    }

    fn name(&self) -> &str {
        // When the cascade context is set (the dispatcher
        // installed it via `with_cascade_context`), return the
        // override so the SDK impl's cascade keys the
        // `param_rejections` table on the operator-facing
        // section name. Otherwise delegate to the inner client —
        // for production SDK impls (`AnthropicClient`,
        // `OpenAIClient` built via `from_resolved`) the inner
        // `name()` IS the section, so production needs no
        // explicit wiring.
        if let Some(name) = self.cascade_name_override.as_deref() {
            return name;
        }
        self.inner.name()
    }

    fn model(&self) -> &str {
        if let Some(model) = self.cascade_model_override.as_deref() {
            return model;
        }
        self.inner.model()
    }

    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    fn capabilities(&self) -> LlmCapabilities {
        self.inner.capabilities()
    }

    fn effective_max_tokens(&self, req: &LlmRequest) -> u32 {
        // Mirror of the cap chain in `send_once` so the audit-log
        // hash is byte-for-byte identical to the wire body.
        self.inner.effective_max_tokens(req)
    }

    async fn send_once(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Forward to the inner SDK impl. The trait default impl
        // of `LlmClient::send` (used by every SDK impl that
        // participates in the cascade) wraps `send_once` with the
        // preflight omit + bounded 4xx retry — the wrapper itself
        // does not duplicate that loop.
        self.inner.send_once(req).await
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        self.inner.send_probe(req).await
    }

    fn param_rejections_table(&self) -> Option<Arc<ParamRejectionsTable>> {
        self.param_rejections.lock().clone()
    }

    fn set_param_rejections(&self, table: Arc<ParamRejectionsTable>) {
        *self.param_rejections.lock() = Some(table);
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // D8 invariant: the wire body `send` will transmit is
        // the exact byte sequence the caller hashes here.
        request_body_sha256(req)
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Delegate to the inner client so the wrapper stays
        // transparent. Without the delegation the wrapper would
        // inherit the SDK default (`u32::MAX`) and the probe
        // would waste round-trips on values the inner client
        // will never accept.
        self.inner.max_tokens_probe_ceiling()
    }

    async fn count_tokens(&self, text: &str) -> Option<u64> {
        self.inner.count_tokens(text).await
    }
}
