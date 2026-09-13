//! SDK-side LLM client trait (`LlmClient`) and its request/response
//! shapes. This is the *future* surface that will replace
//! `crate::llm::provider::Provider` once issue #933 lands; the
//! existing `Provider` trait stays untouched in this PR so the
//! ~280 `Provider::` references across the runtime keep compiling
//! until the migration PRs flip them one at a time.
//!
//! EPIC #847 — issue #919 (`feat(llm): introduce LlmClient trait +
//! LlmRequest/LlmResponse + MockClient`). Per #900 D1 the long
//! term surface has exactly **three** SDK impls. They are the
//! AnthropicClient, the OpenAIClient (covering both URL variants),
//! and the MockClient, plus the trait itself. Per D3 there is no
//! soft-landing so this module never re-exports Provider,
//! ProviderRegistry, MockProvider, or any of the legacy types.
//! Those remain where they live today and are deleted as a unit in
//! issue #933.
//!
//! ADR-0010 captures the deferred plan this module materialises; the
//! authoritative `src/` source wins on conflict.
//!
//! The `LlmClient` trait is the contract every SDK impl implements.
//! The companion `LlmRequest` / `LlmResponse` types are the
//! provider-agnostic shapes the rest of the runtime hands to the
//! dispatcher; `body_sha256` is the single source of truth for the
//! audit-hash so D8 (no compat layer, no `if minimax` branch) stays
//! satisfied.

pub mod anthropic;
pub mod breakered;
pub mod conversions;
pub mod dispatcher;
pub mod llm_provider;
pub mod mock;
pub mod openai;
pub mod provider_adapter;

pub use self::anthropic::AnthropicClient;
pub use self::breakered::BreakeredClient;
pub use self::dispatcher::SdkKind;
pub use self::llm_provider::LlmClientProvider;
pub use self::mock::MockClient;
pub use self::openai::{OpenAIClient, OpenAIVariant};
pub use self::provider_adapter::ProviderLlmClient;

// SDK-side test stub used by the probe subsystem (issue #925).
// Gated on `#[cfg(test)]` so the release binary does not see the
// scripted-queue plumbing. Production code paths use `MockClient`
// (which is `pub`).
#[cfg(test)]
mod test_stubs;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use test_stubs::{ScriptedLlmClient, ScriptedLlmResponse};

use std::ops::Deref;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::llm::capabilities::ProviderCapabilities;
use crate::llm::param_rejections::{PARAM_NAMES, detect_all_rejections, parse_provider_error_body};
use crate::llm::role::Role;
use crate::llm::wire::{Attachment, Message, ToolChoice, Usage};

/// Crate-wide alias for the SDK trait's error type. Issue #919 keeps
/// the single, existing [`crate::error::Error`] enum so the migration
/// path stays additive — every `Result<LlmResponse>` site uses the
/// crate-wide `Result<T>` alias without needing a new variant set.
pub type LlmError = crate::error::Error;

/// Thin newtype over [`ProviderCapabilities`] so the SDK trait has a
/// type-tagged return without duplicating the matrix.
///
/// The wrapper is intentionally a single-field newtype (no parallel
/// struct, no `pub use` alias) per #900 D3 ("no soft-landing, no
/// compat layer") and per ADR-0010. Existing `wire_format_id()` and
/// the `for_*` constructors are reachable through the
/// [`Deref`] impl below, so call sites can keep their `cap.wire_format_id()`
/// / `ProviderCapabilities::for_mock()` style once the migration
/// completes.
pub struct LlmCapabilities(pub ProviderCapabilities);

impl Deref for LlmCapabilities {
    type Target = ProviderCapabilities;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Provider-agnostic SDK request.
///
/// Mirrors [`crate::llm::wire::Request`] field-for-field so the
/// migration PRs (issues #921-#930) can mechanically swap the type at
/// each call site. The single additive change is `top_k`, introduced
/// for #920 (D7 — used by the OpenAI-compat body builder to forward
/// the optional top-k knob). All other fields stay byte-identical so
/// the pre-existing wire body builders keep compiling until #933
/// deletes the legacy `Request` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Which role this call plays in the pipeline.
    pub role: Role,
    /// Model identifier (e.g. `"MiniMax-M3"`).
    pub model: String,
    /// System prompt. Stable across calls of the same role.
    pub system: String,
    /// User prompt. The actual content the model reacts to.
    pub user: String,
    /// Maximum tokens to generate. `None` lets the provider / upstream
    /// default apply — the wire builder omits the field entirely. See
    /// [`crate::llm::wire::Request::max_tokens`] for the full contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature (e.g. 0.6). `None` lets the provider choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling top-p (e.g. 0.95). `None` lets the provider choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Top-k sampling cutoff. `None` lets the provider choose.
    /// `skip_serializing_if = "Option::is_none"` keeps the wire body
    /// byte-identical to pre-#919 requests when the field is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Optional JSON schema for structured output. See
    /// [`crate::llm::wire::Request::response_schema`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
    /// Whether the provider should stream tokens as they arrive.
    /// Defaults to `false`.
    #[serde(default)]
    pub stream: bool,
    /// Extra messages appended after the user message — used by the
    /// `PromptPrefill` JSON recovery strategy. See
    /// [`crate::llm::wire::Request::extra_messages`].
    #[serde(default)]
    pub extra_messages: Vec<Message>,
    /// File attachments carried with the request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Tool / function-call selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

impl LlmRequest {
    /// Build a request with the bare minimum the SDK impls need.
    /// Mirrors the field-for-field shape of
    /// [`crate::llm::wire::Request`] so the migration PRs can
    /// mechanically swap `Request` for `LlmRequest` at every call
    /// site without dropping per-call options. Every other field
    /// (`max_tokens`, `temperature`, `top_p`, `response_schema`,
    /// `stream`, `extra_messages`, `attachments`, `tool_choice`) is
    /// filled with the SDK-side default (`None` / `false` /
    /// empty), which matches the legacy `Request::default()` shape
    /// after `request_default!` expanded. Issue #923 wires
    /// `call_with_retry` through this constructor so the default-
    /// pair dispatch keeps the same wire body it had before the
    /// `LlmClient` migration.
    pub fn new(role: Role, system: String, user: String) -> Self {
        Self {
            role,
            model: String::new(),
            system,
            user,
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            response_schema: None,
            stream: false,
            extra_messages: Vec::new(),
            attachments: Vec::new(),
            tool_choice: None,
        }
    }
}

/// Provider-agnostic SDK response.
///
/// Mirrors [`crate::llm::wire::Response`] and adds [`Self::http_status`]
/// so the audit trail captures the transport-level status without
/// threading a `(u16, Response)` tuple through every layer (the legacy
/// `Provider::send` returns `Result<(u16, Response)>` per
/// [`crate::llm::provider::Provider::send`], doc-comment at
/// `provider.rs:97-103`; the SDK trait folds the status into the
/// response so callers only need a single value back).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// The model output text.
    pub text: String,
    /// Stop reason reported by the provider (`"end_turn"`, `"max_tokens"`, etc.).
    pub finish_reason: Option<String>,
    /// Convenience flag: `true` when the response was cut at
    /// `max_tokens`. See [`crate::llm::wire::Response::truncated`].
    #[serde(default)]
    pub truncated: bool,
    /// Token usage.
    pub usage: Usage,
    /// HTTP status (or transport-level equivalent). Mock SDKs return
    /// `200`. Folding the status into the response keeps the call
    /// surface to a single return value (`Result<LlmResponse>`) and
    /// preserves the audit trail the dispatcher writes into
    /// [`crate::llm::wire::CallRecord::http_status`].
    pub http_status: u16,
}

/// SDK contract every LLM impl satisfies.
///
/// Per #900 D1 exactly three impls exist on the long-term surface —
/// `AnthropicClient`, `OpenAIClient` (chat + responses URL variants),
/// and [`MockClient`]. Issue #919 ships only the trait and the mock;
/// the live-SDK impls land in #920 and #921.
///
/// The trait is `async_trait`-based to match
/// [`crate::llm::provider::Provider`]; switching to native async fns
/// in trait is deferred to a later Rust toolchain bump. `Send + Sync`
/// is required so the impl can live inside an `Arc` and be shared
/// across the run process.
///
/// #932 (D9) absorbed the cascade preflight + retry loop into the
/// trait. Each concrete impl provides [`Self::send_once`] — the bare
/// single-call surface (HTTP transport for live SDKs, queue pop for
/// the mock). The trait default [`Self::send`] wraps `send_once`
/// with the preflight omit + cascade-retry logic so every SDK impl
/// handles its own cascade without the dispatcher knowing about it.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Stable SDK identifier (`"anthropic"`, `"openai_chat"`,
    /// `"openai_responses"`, `"mock"`). Distinct from
    /// [`Self::name`] (the operator-facing registry key) so the
    /// URL-path dispatcher in #922 can route on the SDK identity
    /// without consulting the registry.
    fn sdk_type(&self) -> &'static str;

    /// Operator-facing name (the `[providers.<name>]` section header).
    fn name(&self) -> &str;

    /// Model identifier (e.g. `"MiniMax-M3"`).
    fn model(&self) -> &str;

    /// Full URL (carries the wire-format path suffix — used by the
    /// dispatcher in #922 to pick the right wire-format builder).
    /// SDKs that do not talk HTTP (the mock) return any stable
    /// sentinel (e.g. `"mock://local"`).
    fn endpoint(&self) -> &str;

    /// Static capability matrix. See [`LlmCapabilities`] — the
    /// wrapper derefs to [`ProviderCapabilities`] so call sites can
    /// reach `wire_format_id()` and the `for_*` constructors without
    /// an extra hop.
    fn capabilities(&self) -> LlmCapabilities;

    /// Optional param-rejection table the SDK cascade consults
    /// (via the default [`Self::send`] impl). Each concrete SDK
    /// impl stores the table behind interior mutability and returns
    /// a clone via this accessor; the default returns `None` so
    /// SDK impls that do not participate in the cascade are
    /// zero-cost. The dispatcher (`phases::phase`) injects the
    /// table via [`Self::set_param_rejections`] before the first
    /// `send`; the SDK impl's internal storage is an
    /// `Arc<ParamRejectionsTable>` so a single write propagates
    /// across every later `send`.
    fn param_rejections_table(
        &self,
    ) -> Option<Arc<crate::llm::param_rejections::ParamRejectionsTable>> {
        None
    }

    /// Interior-mutability setter the dispatcher uses to inject the
    /// run-level [`crate::llm::param_rejections::ParamRejectionsTable`]
    /// into the SDK impl. Default is a no-op so SDK impls that do
    /// not participate in the cascade (the probe stub, …) do not
    /// pay any cost for the trait method. Implementations that
    /// override [`Self::param_rejections_table`] MUST override
    /// this setter too so the table the cascade consults is the
    /// one the dispatcher injected.
    fn set_param_rejections(
        &self,
        _table: Arc<crate::llm::param_rejections::ParamRejectionsTable>,
    ) {
        // No-op default. SDK impls with interior-mutable storage
        // override this to push the table into their slot.
    }

    /// Bare single-call surface. Each concrete impl implements
    /// this — `MockClient::send_once` returns a programmed
    /// response, `AnthropicClient::send_once` /
    /// `OpenAIClient::send_once` make the HTTP call + apply the
    /// max-tokens clamp + build the wire body, and the adapter
    /// impls (`BreakeredClient`, `ProviderLlmClient`) delegate to
    /// the inner provider. The default [`Self::send`] impl wraps
    /// this with the cascade preflight + retry loop, so the SDK
    /// impl never needs to know the cascade exists.
    async fn send_once(&self, req: &LlmRequest) -> Result<LlmResponse>;

    /// Cascade-aware send entry point. Per #900 D9 the trait
    /// absorbs the cascade preflight + retry currently duplicated
    /// in `phases::phase` so every SDK impl handles its own
    /// cascade without the dispatcher knowing about it. The
    /// default impl runs the preflight omit (consulting
    /// [`Self::param_rejections_table`]) and the bounded retry
    /// loop (capped at [`PARAM_NAMES`] iterations) around
    /// [`Self::send_once`]. Issue #923 lands the SDK-trait
    /// migration; #932 (D9) folds the cascade into this default
    /// impl so every SDK impl handles its own recovery without
    /// the dispatcher knowing about it.
    ///
    /// SDK impls that participate in the cascade must NOT override
    /// this method — overriding breaks the D9 invariant that the
    /// cascade lives in exactly one place. Overriding
    /// [`Self::send_once`] is the right hook for SDK-specific
    /// transport behaviour (HTTP call, queue pop, …).
    async fn send(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let table = self.param_rejections_table();
        let mut working = req.clone();
        if let Some(table) = table.as_ref() {
            let mut omitted: Vec<&str> = Vec::new();
            for param in PARAM_NAMES {
                if table.should_omit(self.name(), self.model(), param) {
                    omit_param_llm(&mut working, param);
                    omitted.push(*param);
                }
            }
            if !omitted.is_empty() {
                tracing::debug!(
                    provider = self.name(),
                    model = self.model(),
                    omitted = ?omitted,
                    "omitted known-rejected params before dispatch"
                );
            }
        }
        let mut result = self.send_once(&working).await;
        let max_rejection_retries = PARAM_NAMES.len();
        let mut rejection_attempts: usize = 0;
        while rejection_attempts < max_rejection_retries {
            let status = match result.as_ref().err().and_then(|e| e.http_status()) {
                Some(s) if (400..500).contains(&s) => s,
                _ => break,
            };
            let err = result.as_ref().expect_err("status set implies Err");
            let body = parse_provider_error_body(err, status);
            let detected = detect_all_rejections(status, body.as_ref());
            if detected.is_empty() {
                break;
            }
            for detected_param in &detected {
                tracing::info!(
                    provider = self.name(),
                    model = self.model(),
                    detected_param = %detected_param,
                    "auto-detected param rejection; retrying without it"
                );
                if let Some(table) = table.as_ref()
                    && let Err(rec_err) = table.record(self.name(), self.model(), detected_param)
                {
                    tracing::warn!(
                        error = %rec_err,
                        "failed to persist param rejection; in-memory entry still kept"
                    );
                }
                omit_param_llm(&mut working, detected_param);
            }
            rejection_attempts += 1;
            result = self.send_once(&working).await;
            if result.is_ok() {
                break;
            }
        }
        result
    }

    /// SHA-256 of the wire body that `send` will transmit. This is
    /// the single source of truth for the audit-hash so D8
    /// ("no compat layer") holds: there is **no** `if minimax` branch
    /// anywhere; the same byte sequence `send` emits goes through
    /// this function. Implementations must mirror the byte-level
    /// changes `send` applies (max_tokens clamp, param omission,
    /// tool_choice mapping).
    ///
    /// SDKs that do not talk HTTP (the mock) return a deterministic
    /// hash of the canonical request shape — same bytes across
    /// runs, distinct across requests that differ on any wire
    /// field. The mock uses `sha256(serde_json::to_vec(req))` so a
    /// call with a different `user` prompt produces a different
    /// digest, satisfying the "audit trail tracks per-call payload"
    /// invariant without lying about a wire body that does not
    /// exist.
    fn body_sha256(&self, req: &LlmRequest) -> Result<String>;

    /// Probe-bypass variant for the auto-probe (skips the safety
    /// wire-clamp AND the cascade — the probe algorithm needs the
    /// bare upstream behaviour, not the auto-healing cascade the
    /// dispatcher layers on top). Default forwards to
    /// [`Self::send_once`] — correct for SDKs that do not clamp
    /// (mock) and a safe baseline for SDKs that do (the override
    /// lives in the live impl). SDK impls that DO have a
    /// probe-specific transport (skip the safety clamp) must
    /// override this method.
    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let _ = req;
        self.send_once(req).await
    }

    /// Upper bound the auto-probe should search up to. Default
    /// `u32::MAX` — correct for SDKs without a documented ceiling
    /// (the mock). Live SDKs that carry a wire-side ceiling override
    /// this so the probe does not waste 30 sequential round-trips
    /// probing values the upstream will never accept.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        u32::MAX
    }

    /// Return the `max_tokens` value that [`Self::send`] will
    /// actually transmit on the wire for `req`, after every
    /// per-provider cap (operator override, kind-level ceiling,
    /// auto-probe table, …) has been applied.
    ///
    /// This is the single source of truth for the audit-log hash:
    /// the caller (`phases::phase`) clones `req`, sets
    /// `cloned.max_tokens = self.effective_max_tokens(req)`, and
    /// feeds the clone to `request_body_sha256`. Because the clamp
    /// chain here is the same one `send` runs against
    /// `req.max_tokens`, the recorded sha256 matches the proxy's
    /// wire capture byte-for-byte.
    ///
    /// Default returns `req.max_tokens` unchanged — correct for
    /// SDKs that do not clamp (the mock, …). Implementations that
    /// clamp inside `send` must override this so the audit hash
    /// stays in sync with the wire body.
    ///
    /// `None` on `req.max_tokens` is treated as `u32::MAX` so the
    /// audit hash stays deterministic when the auto-healing path
    /// drops the field from the wire body.
    fn effective_max_tokens(&self, req: &LlmRequest) -> u32 {
        req.max_tokens.unwrap_or(u32::MAX)
    }

    /// Optional: count tokens for pre-flight estimation. Default
    /// `None` — the caller falls back to a heuristic.
    async fn count_tokens(&self, text: &str) -> Option<u64> {
        let _ = text;
        None
    }
}

impl LlmResponse {
    /// Lift a `(http_status, legacy::Response)` pair into the
    /// SDK-side `LlmResponse`. The transport status folds into
    /// `LlmResponse::http_status` so callers only deal with a
    /// single return value (`Result<LlmResponse>`). Shared by every
    /// SDK impl that wraps a `Provider`-shaped transport
    /// (`AnthropicClient`, `OpenAIClient`, …) and the
    /// `BreakeredClient` adapter (#923).
    ///
    /// Delegates to the canonical `From<(u16, Response)>` impl in
    /// [`crate::llm::client::conversions`] so the conversion
    /// behaviour has a single source of truth. The wrapper is
    /// deprecated in favour of calling `.into()` directly; it
    /// stays because the SDK impls (`AnthropicClient::send_once`,
    /// `OpenAIClient::send_once`, `BreakeredClient::send_once`)
    /// already use the named method. #933 deletes the legacy
    /// `Response` type and with it this wrapper.
    pub(crate) fn from_parts(http_status: u16, resp: crate::llm::wire::Response) -> Self {
        (http_status, resp).into()
    }
}

/// Clear an optional wire field on an [`LlmRequest`] so the cascade
/// retry does not re-emit a parameter the upstream already rejected.
///
/// Mirrors [`crate::llm::wire::omit_param`] (the legacy
/// `Request`-shaped helper) field-for-field: `temperature`, `top_p`,
/// and `max_tokens` clear to `None` (each is `Option<_>`, so the
/// wire body drops the field via
/// `#[serde(skip_serializing_if = "Option::is_none")]`); unknown
/// parameters are no-ops so the cascade loop can call this helper
/// unconditionally for every name the detector surfaces without
/// filtering against [`PARAM_NAMES`] first.
///
/// Lives in `client` (not `wire`) so the SDK cascade reuses the
/// same symbol the legacy `Request` cascade uses — the two helpers
/// have parallel surface area and parallel semantics so the
/// cascade can be ported across request shapes without re-reading
/// the field-by-field contract.
pub fn omit_param_llm(req: &mut LlmRequest, param: &str) {
    tracing::debug!(param, "omit_param_llm: clearing optional wire field");
    match param {
        "temperature" => req.temperature = None,
        "top_p" => req.top_p = None,
        // `max_tokens` → `None` drops the field from the wire body
        // (via `skip_serializing_if`). The auto-healing
        // `param_rejections` table records the rejection so the
        // next run omits the field from the first call, closing
        // the loop for upstreams that reject the *presence* of
        // `max_tokens` (e.g. `gpt-5.6-luna`).
        "max_tokens" => req.max_tokens = None,
        // Unknown parameters are a no-op so the runtime can record
        // the rejection (so the next run learns) without breaking
        // the current call.
        _ => {
            tracing::trace!(param, "omit_param_llm: unknown parameter, no-op");
        }
    }
}

/// Run the SDK cascade: preflight omit + bounded 4xx retry around
/// the impl's bare [`LlmClient::send_once`].
///
/// This is the default body of [`LlmClient::send`] — the trait
/// method delegates here so every SDK impl inherits the cascade
/// for free. SDK impls MUST NOT override [`LlmClient::send`];
/// overriding [`LlmClient::send_once`] is the right hook for
/// SDK-specific transport behaviour. The cascade contracts:
///
/// - **Preflight** — for every name in [`PARAM_NAMES`], if the
///   [`LlmClient::param_rejections_table`] says
///   `(name, model, param)` is known-rejected, clear the field on
///   a clone of `req`. The clone is what `send_once` sees; the
///   caller's `req` is untouched.
/// - **Cascade retry** — on `4xx`, run [`detect_all_rejections`]
///   against the body ([`parse_provider_error_body`] strips the
///   `provider error:` envelope). For every detected name:
///   record into the table (no-op when the SDK impl has no
///   table), clear the field on the working clone, retry via
///   `send_once`. Bounded at [`PARAM_NAMES`] iterations so an
///   upstream that loops the same body can never starve the
///   dispatcher. A 5xx / transport error / non-4xx aborts the
///   cascade with the upstream error intact.
/// - **D8 invariant** — the cascade never branches on the SDK
///   identifier. Every provider sees the same loop; the SDK impl
///   returns the same wire body the audit hash captures.
///
/// #932 moved this loop out of `phases::phase::dispatch_to_provider`
/// and `dispatch_to_provider_for` so every SDK impl handles its
/// own cascade. The dispatch site collapses from ~70 lines of
/// cascade bookkeeping to a single `client.send(&req).await` call.
#[cfg(test)]
mod tests {
    use super::*;

    /// `LlmRequest::max_tokens = None` must round-trip as field-absent
    /// so the wire body never emits a literal `"max_tokens": null` —
    /// mirrors the contract pinned for [`crate::llm::wire::Request`].
    #[test]
    fn request_omits_max_tokens_when_none() {
        let r = LlmRequest {
            role: Role::Intake,
            model: "m".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let j: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(
            j.get("max_tokens").is_none(),
            "max_tokens must be absent from the wire when None, got: {j}"
        );
        assert!(
            j.get("top_k").is_none(),
            "top_k must be absent from the wire when None, got: {j}"
        );
        assert!(
            j.get("temperature").is_none(),
            "temperature must be absent when None, got: {j}"
        );
        let back: LlmRequest = serde_json::from_value(j).unwrap();
        assert_eq!(back.max_tokens, None);
        assert_eq!(back.top_k, None);
    }

    /// `LlmRequest::top_k = Some(n)` round-trips with the numeric
    /// value preserved. Pins the byte-identity contract the new
    /// wire body builders will rely on (#920).
    #[test]
    fn request_includes_top_k_when_some() {
        let r = LlmRequest {
            role: Role::Intake,
            model: "m".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: Some(40),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let j: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(
            j.get("top_k"),
            Some(&serde_json::json!(40)),
            "top_k must serialise as a numeric JSON value, got: {j}"
        );
        let back: LlmRequest = serde_json::from_value(j).unwrap();
        assert_eq!(back.top_k, Some(40));
    }

    /// `LlmResponse` carries the audit `http_status` so the call site
    /// only needs a single return value (`Result<LlmResponse>`). The
    /// status must serialise alongside the rest of the response so
    /// downstream tooling (telemetry, dashboards) can read it
    /// through the same envelope.
    #[test]
    fn response_carries_http_status() {
        let r = LlmResponse {
            text: "ok".into(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: Usage::default(),
            http_status: 200,
        };
        let j: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(j.get("http_status"), Some(&serde_json::json!(200)));
        let back: LlmResponse = serde_json::from_value(j).unwrap();
        assert_eq!(back.http_status, 200);
    }

    /// `LlmCapabilities` derefs to `ProviderCapabilities` so the
    /// `wire_format_id()` helper and the `for_*` constructors stay
    /// reachable without an extra accessor. Pins the newtype-as-thin-
    /// wrapper design from #900 D3.
    #[test]
    fn capabilities_deref_to_provider_capabilities() {
        let cap = LlmCapabilities(ProviderCapabilities::default());
        assert_eq!(cap.wire_format_id(), "openai_compatible");
        assert_eq!(cap.0.wire_format_id(), "openai_compatible");
    }
}
