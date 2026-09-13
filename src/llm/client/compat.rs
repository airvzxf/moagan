//! Pre-#933 compatibility shims.
//!
//! Several non-LLM modules (`cli/run.rs`, `cli/discover.rs`,
//! `telemetry/mod.rs`, …) and a long tail of integration tests
//! import the legacy `crate::llm::provider::{Provider, Request,
//! Response, BreakeredProvider, SaturationSink, attach_parallelism_rate_limit, …}`
//! surface. After #933 the production wrapper is
//! [`crate::llm::client::BreakeredClient`] and the per-provider
//! rate limiter was retired alongside the legacy `BreakeredProvider`.
//!
//! This module provides `pub` aliases for the legacy trait shape so
//! the call sites keep compiling while the migration moves forward.
//! The aliases are **deprecated** and will be removed once every
//! call site migrates to `LlmClient`.

// The compat shims use the deprecated `Request` / `Response` aliases
// and the `LlmClientProvider` stub from this same module. Suppress
// the self-referential `#[deprecated]` warnings so the
// `clippy -D warnings` gate does not break the build for this file.
#![allow(deprecated)]

use std::sync::Arc;

use async_trait::async_trait;

use super::{LlmClient, LlmRequest, LlmResponse};

/// Pre-#933 name for [`LlmRequest`].
#[deprecated(note = "post-#933 alias for LlmRequest")]
pub type Request = LlmRequest;

/// Pre-#933 name for [`LlmResponse`].
#[deprecated(note = "post-#933 alias for LlmResponse")]
pub type Response = LlmResponse;

/// Pre-#933 provider trait. Equivalent to [`LlmClient`] — every
/// pre-#933 `Provider::send(&req) -> Result<(u16, Response)>` call
/// maps to `LlmClient::send_once(&req) -> Result<LlmResponse>` with
/// `LlmResponse::http_status` carrying the transport status.
///
/// The blanket impl for `Arc<dyn LlmClient>` lets every SDK impl
/// (`AnthropicClient`, `OpenAIClient`, `MockClient`) satisfy the
/// legacy trait automatically — no per-impl adapter code needed.
/// Re-export the pre-#933 `SaturationSink` trait with the
/// post-#933 compat shape so legacy call sites (`impl
/// SaturationSink for …`, `use crate::llm::SaturationSink`, …)
/// keep compiling.
pub use self::SaturationSink as _;

/// Pre-#933 saturation-event re-export.
pub use self::SaturationEvent as _SaturationEvent;

/// Pre-#933 `ProviderRegistry` re-export. The legacy alias
/// points at [`super::super::registry::LlmClientRegistry`].
pub use super::registry::ProviderRegistry;

/// Pre-#933 `LlmClientProvider` shim — the post-#933 world has no
/// need for the bridge because every SDK impl implements
/// `LlmClient` directly. The legacy adapter is kept as a
/// type alias so test code keeps compiling (the tests' `new(sdk)`
/// pattern is now equivalent to `Arc::new(sdk)`).
#[deprecated(note = "post-#933 no-op; every SDK impl is an LlmClient directly")]
pub struct LlmClientProvider;

impl LlmClientProvider {
    /// No-op constructor. Pre-#933 the bridge wrapped an
    /// `Arc<dyn LlmClient>` into an `Arc<dyn Provider>`. Post-#933
    /// the SDK impl is an `LlmClient` directly, so callers
    /// register the SDK impl through
    /// [`LlmClientRegistry::insert`] / `insert_raw` and the
    /// bridge is unnecessary.
    pub fn new<T>(_inner: T) -> Self {
        Self
    }
}

/// Pre-#933 `ProviderLlmClient` shim. The post-#933 world does
/// not need this bridge either — it was the inverse of
/// `LlmClientProvider`, wrapping an `Arc<dyn Provider>` into
/// an `Arc<dyn LlmClient>`. Since `Provider` and `LlmClient`
/// are now blanket-impl-aliased (every `Arc<dyn Provider>`
/// already satisfies `LlmClient` via the compat shim), the
/// adapter is just a type alias.
#[deprecated(note = "post-#933 no-op; Provider blanket-impls LlmClient")]
pub type ProviderLlmClient = Arc<dyn LlmClient>;

/// Convert a `(u16, LlmResponse)` pair into the legacy
/// `(u16, Response)` shape. The orphan rule prevents the impl on
/// `(u16, Response)` directly, so the conversion goes through a
/// method on `LlmResponse` ([`crate::llm::client::LlmResponse::into_legacy`]).
/// Probe call sites use that method instead of `From<LlmResponse>`.
pub fn into_legacy_response(resp: super::LlmResponse) -> super::Response {
    super::Response {
        text: resp.text,
        finish_reason: resp.finish_reason,
        truncated: resp.truncated,
        usage: resp.usage,
        http_status: resp.http_status,
    }
}

/// Mirror of [`into_legacy_response`] for `&LlmResponse`. Returns
/// the `(http_status, Response)` pair in one call so the probe
/// algorithms can keep their single-line `match` arm.
pub fn into_legacy_pair(resp: &super::LlmResponse) -> (u16, super::Response) {
    (
        resp.http_status,
        super::Response {
            text: resp.text.clone(),
            finish_reason: resp.finish_reason.clone(),
            truncated: resp.truncated,
            usage: resp.usage.clone(),
            http_status: resp.http_status,
        },
    )
}

/// Pre-#933 provider trait. Equivalent to [`LlmClient`] — every
/// pre-#933 `Provider::send(&req) -> Result<(u16, Response)>` call
/// maps to `LlmClient::send_once(&req) -> Result<LlmResponse>` with
/// `LlmResponse::http_status` carrying the transport status.
///
/// The blanket impl for `Arc<dyn LlmClient>` lets every SDK impl
/// (`AnthropicClient`, `OpenAIClient`, `MockClient`) satisfy the
/// legacy trait automatically — no per-impl adapter code needed.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable short name (e.g. `"minimax"`, `"mock"`).
    fn name(&self) -> &str;
    /// Model identifier.
    fn model(&self) -> &str;
    /// HTTP endpoint.
    fn endpoint(&self) -> &str;
    /// Static capability matrix.
    fn capabilities(&self) -> super::LlmCapabilities;
    /// Send a request, returning `(http_status, response)`. Maps to
    /// `LlmClient::send_once` + a transport-status fold.
    async fn send(&self, req: &Request) -> crate::error::Result<(u16, Response)>;
    /// Effective `max_tokens` after every per-provider cap.
    fn effective_max_tokens(&self, req: &Request) -> u32;
    /// Probe-bypass variant.
    async fn send_probe(&self, req: &Request) -> crate::error::Result<(u16, Response)>;
    /// Upper bound the auto-probe searches up to.
    fn max_tokens_probe_ceiling(&self) -> u32;
    /// Optional: count tokens for pre-flight estimation.
    async fn count_tokens(&self, text: &str) -> Option<u64>;
}

#[async_trait]
impl<T: LlmClient + ?Sized> Provider for T {
    fn name(&self) -> &str {
        LlmClient::name(self)
    }
    fn model(&self) -> &str {
        LlmClient::model(self)
    }
    fn endpoint(&self) -> &str {
        LlmClient::endpoint(self)
    }
    fn capabilities(&self) -> super::LlmCapabilities {
        LlmClient::capabilities(self)
    }
    async fn send(&self, req: &Request) -> crate::error::Result<(u16, Response)> {
        let resp = LlmClient::send_once(self, req).await?;
        Ok((resp.http_status, resp))
    }
    fn effective_max_tokens(&self, req: &Request) -> u32 {
        LlmClient::effective_max_tokens(self, req)
    }
    async fn send_probe(&self, req: &Request) -> crate::error::Result<(u16, Response)> {
        let resp = LlmClient::send_probe(self, req).await?;
        Ok((resp.http_status, resp))
    }
    fn max_tokens_probe_ceiling(&self) -> u32 {
        LlmClient::max_tokens_probe_ceiling(self)
    }
    async fn count_tokens(&self, text: &str) -> Option<u64> {
        LlmClient::count_tokens(self, text).await
    }
}

/// Pre-#933 wrapper alias. The legacy `BreakeredProvider` and the
/// post-#933 `BreakeredClient` have the same surface (a wrapper
/// around an inner SDK impl) — the new wrapper holds an
/// `Arc<dyn LlmClient>` directly instead of `Arc<dyn Provider>`.
#[deprecated(note = "post-#933 alias for BreakeredClient")]
pub type BreakeredProvider = super::breakered::BreakeredClient;

/// Pre-#933 saturation-sink trait. The per-`(provider, role)`
/// `ThrottleGovernor` (D8) replaced it; tests that wired a custom
/// sink through `BreakeredProvider::with_saturation_sink` can
/// route the same intent through `RunContext::throttle`.
pub trait SaturationSink: Send + Sync {
    /// Called by the wrapper when the upstream saturated. Takes a
    /// `SaturationEvent` (mirrors the legacy
    /// `crate::llm::provider::SaturationEvent`) so the trait
    /// surface stays shape-compatible with the pre-#933 callers.
    fn on_saturation(&self, event: &SaturationEvent);
}

/// Pre-#933 saturation-event shape. The legacy
/// `crate::llm::provider::SaturationEvent` struct carried the
/// provider name + kind string. Kept here so `Telemetry`'s impl
/// (and any test impl) keeps compiling.
#[derive(Debug, Clone)]
pub struct SaturationEvent {
    /// Provider name the wrapper reported.
    pub provider: String,
    /// Rejection kind (`"breaker"`, `"rate_limit"`, …).
    pub kind: String,
    /// Optional run id the wrapper stamped (legacy wrappers set
    /// `None`; the sink re-stamps with the current run id).
    pub run_id: Option<String>,
}

/// Pre-#933 rate-limit configuration. Kept as a re-export so legacy
/// call sites that took `Option<&RateLimitConfig>` keep compiling
/// even though the function is a no-op.
#[allow(non_camel_case_types)]
#[deprecated(note = "post-#933 no-op; per-provider rate limiter retired")]
pub type RateLimitConfig = ();

/// Pre-#933 rate-limiter knob. Post-#933 the per-provider
/// rate-limiter layer was retired — the per-`(provider, role)`
/// `ThrottleGovernor` (D8) absorbs the throttle cases the old
/// wrapper handled, and `BreakeredClient` does not carry a
/// per-call rate limiter. The hook stays as a no-op so legacy
/// call sites keep compiling.
#[deprecated(note = "post-#933 no-op; per-provider rate limiter retired")]
pub fn attach_parallelism_rate_limit<R, T, U>(
    _registry: &R,
    _effective_rate_limit: Option<&T>,
    _rate_limit_per_provider: &U,
) {
    // No-op.
}

/// Pre-#933 `wire_format_id()` helper. Returns the SDK identifier
/// the legacy `Provider::wire_format_id` returned. Routes through
/// `LlmClient::capabilities().capabilities().wire_format_id()`.
pub fn wire_format_id(provider: &dyn Provider) -> &'static str {
    // `Provider::capabilities` returns `LlmCapabilities` which
    // derefs to `ProviderCapabilities` via the inner `pub` field.
    // Call through the deref so the `wire_format_id()` helper on
    // `ProviderCapabilities` is reachable.
    let cap = provider.capabilities();
    let inner: &crate::llm::capabilities::ProviderCapabilities = &cap.0;
    inner.wire_format_id()
}

/// Pre-#933 `registry_from_config_with_home_and_sink` shim. Returns
/// an empty [`super::registry::LlmClientRegistry`]; production
/// callers now go through
/// [`crate::llm::client::dispatcher::build_client`] directly. Kept
/// as a stub so legacy callers keep compiling.
#[deprecated(note = "post-#933 stub; use client::dispatcher::build_client")]
pub fn registry_from_config_with_home_and_sink<R, H, C>(
    _cfg: &R,
    _circuit_breaker: &C,
    _home: H,
    _sink: Option<Arc<dyn SaturationSink>>,
    _active_pairs: Option<Vec<(String, String)>>,
) -> super::registry::LlmClientRegistry {
    let _ = (_circuit_breaker, _home, _sink, _active_pairs);
    super::registry::LlmClientRegistry::new()
}

/// Pre-#933 `registry_from_config_with_sink` shim.
#[deprecated(note = "post-#933 stub; use client::dispatcher::build_client")]
pub fn registry_from_config_with_sink<R, C>(
    cfg: &R,
    circuit_breaker: &C,
    home: Option<Arc<()>>,
    sink: Option<Arc<dyn SaturationSink>>,
) -> super::registry::LlmClientRegistry {
    registry_from_config_with_home_and_sink(
        cfg,
        circuit_breaker,
        home.unwrap_or_else(|| Arc::new(())),
        sink,
        None::<Vec<(String, String)>>,
    )
}

/// Pre-#933 `registry_from_config` shim.
#[deprecated(note = "post-#933 stub; use client::dispatcher::build_client")]
pub fn registry_from_config<R>(cfg: &R) -> super::registry::LlmClientRegistry {
    registry_from_config_with_sink(cfg, &(), None, None)
}

/// Pre-#933 `registry_from_config_with_sink_active` shim.
#[deprecated(note = "post-#933 stub; use client::dispatcher::build_client")]
pub fn registry_from_config_with_sink_active<R, C>(
    cfg: &R,
    circuit_breaker: &C,
) -> super::registry::LlmClientRegistry {
    registry_from_config_with_sink(cfg, circuit_breaker, None, None)
}
