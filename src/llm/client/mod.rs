//! SDK-side LLM client trait (`LlmClient`) and its request/response
//! shapes. The canonical post-#933 surface — replaces the legacy
//! `crate::llm::provider::Provider` trait tree that the migration
//! wave (#919–#932) deleted.
//!
//! Per #900 D1 exactly **three** SDK impls exist on the long-term
//! surface — `AnthropicClient`, `OpenAIClient` (covering both URL
//! variants), and `MockClient` — plus the trait itself. Per D3 there
//! is no soft-landing so this module does not re-export `Provider`,
//! `ProviderRegistry`, `MockProvider`, or any of the legacy types.
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
pub mod compat;
pub mod dispatcher;
pub mod mock;
pub mod openai;
pub mod openai_body;
pub mod registry;

pub use self::anthropic::AnthropicClient;
pub use self::breakered::BreakeredClient;
pub use self::compat::{LlmClientProvider, ProviderLlmClient};
pub use self::dispatcher::{SdkKind, WireFormatId};
pub use self::mock::{MockClient, MockResponse};
pub use self::openai::{OpenAIClient, OpenAIVariant};
pub use self::registry::{LlmClientRegistry, ProviderRegistry, registry_key};

/// Pre-#933 name for [`LlmRequest`]. Kept as an alias so callers
/// that have not yet migrated to `LlmRequest` keep compiling.
pub type Request = LlmRequest;
/// Pre-#933 name for [`LlmResponse`]. Kept as an alias so callers
/// that have not yet migrated to `LlmResponse` keep compiling.
pub type Response = LlmResponse;

/// Convenience accessor the dispatcher and telemetry use to read
/// the legacy `wire_format_id()` string off the SDK trait without
/// having to thread `capabilities().wire_format_id()` through
/// every call site.
pub fn wire_format_id_of(client: &dyn LlmClient) -> &'static str {
    client.capabilities().wire_format_id()
}

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

/// Roles that produce structured JSON output. The Anthropic-compat
/// SDK (`AnthropicClient::send_once`) consults this to decide whether
/// to inject the JSON prefill on the wire body; the OpenAI-compat
/// SDK (`OpenAIClient`) consults it to set `response_format`.
///
/// Mirrors the legacy `crate::llm::client::role_requires_json`
/// (deleted in #933) and the duplicate in
/// `crate::llm::openai_compatible` (also deleted). The single
/// source of truth lives here so the two SDK impls cannot drift.
pub fn role_requires_json(role: Role) -> bool {
    use crate::llm::Role::*;
    matches!(
        role,
        Intake
            | Clarify
            | Route
            | Gate
            | Critique
            | Repair
            | Rank
            | Synthesizer
            | Adversary
            | Decomposer
            | MergeSynthesizer
    )
}

/// Crate-wide alias for the SDK trait's error type. The single
/// existing [`crate::error::Error`] enum serves the SDK trait so the
/// migration path stays additive — every `Result<LlmResponse>` site
/// uses the crate-wide `Result<T>` alias without needing a new
/// variant set.
pub type LlmError = crate::error::Error;

/// Thin newtype over [`ProviderCapabilities`] so the SDK trait has a
/// type-tagged return without duplicating the matrix.
///
/// The wrapper is intentionally a single-field newtype (no parallel
/// struct, no `pub use` alias) per #900 D3 ("no soft-landing, no
/// compat layer") and per ADR-0010. Existing `wire_format_id()` and
/// the `for_*` constructors are reachable through the
/// [`Deref`] impl below, so call sites can keep their `cap.wire_format_id()`
/// / `ProviderCapabilities::for_mock()` style.
pub struct LlmCapabilities(pub ProviderCapabilities);

impl Deref for LlmCapabilities {
    type Target = ProviderCapabilities;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Provider-agnostic SDK request.
///
/// Mirrors the legacy wire [`Request`] shape (every pre-#919 field)
/// plus the additive `top_k` knob introduced for #920 (D7 — used by
/// the OpenAI-compat body builder to forward the optional top-k
/// knob). All `Option<_>` fields are absent on the wire when `None`
/// via `#[serde(skip_serializing_if = "Option::is_none")]` so the
/// upstream never sees `"field": null`.
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
    /// default apply — the wire builder omits the field entirely.
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
    /// Optional JSON schema for structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
    /// Whether the provider should stream tokens as they arrive.
    /// Defaults to `false`.
    #[serde(default)]
    pub stream: bool,
    /// Extra messages appended after the user message — used by the
    /// `PromptPrefill` JSON recovery strategy.
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
    pub fn new(role: Role, system: String, user: String) -> Self {
        Self {
            role,
            model: String::new(),
            system,
            user,
            max_tokens: None,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: Vec::new(),
            attachments: Vec::new(),
            tool_choice: None,
            top_k: None,
        }
    }
}

/// Provider-agnostic SDK response.
///
/// Mirrors the legacy wire [`Response`] and adds [`Self::http_status`]
/// so the audit trail captures the transport-level status without
/// threading a `(u16, Response)` tuple through every layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// The model output text.
    pub text: String,
    /// Stop reason reported by the provider (`"end_turn"`, `"max_tokens"`, etc.).
    pub finish_reason: Option<String>,
    /// Convenience flag: `true` when the response was cut at
    /// `max_tokens`.
    #[serde(default)]
    pub truncated: bool,
    /// Token usage.
    pub usage: Usage,
    /// HTTP status (or transport-level equivalent). Mock SDKs return
    /// `200`. Folding the status into the response keeps the call
    /// surface to a single return value (`Result<LlmResponse>`).
    pub http_status: u16,
}

// ---------- Wire-side types shared by SDK impls + cache + telemetry ----------
//
// These types used to live in `src/llm/wire.rs` (#933 moved them
// here). They are referenced from every layer of the runtime
// (`cache/`, `telemetry/`, `cost/`, `probe/`, the audit-log hash
// helpers) — keeping them in one place is the single-source-of-
// truth invariant the issue pins.

/// One file attachment carried with an [`LlmRequest`].
///
/// Modality is a free-form string (e.g. `"text"`, `"image"`,
/// `"pdf"`, `"audio"`) to match the upstream `models.dev`
/// `modalities.input` vocabulary verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    /// MIME type or short label (e.g. `"image/png"`).
    pub mime: String,
    /// Modality tag from the upstream catalog vocabulary
    /// (e.g. `"image"`, `"pdf"`).
    pub modality: String,
    /// Body of the attachment. Wire builders that need a
    /// base64-encoded payload convert the bytes themselves
    /// before serialising the body.
    pub data: Vec<u8>,
}

/// Tool / function-call selection on an [`LlmRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    Auto,
    /// The model must call exactly one of the supplied tools.
    Required,
    /// The model must not call any tool.
    None,
}

/// A single chat message used by `LlmRequest::extra_messages`. Mirrors
/// the OpenAI Chat-Completions message shape (`{"role": "...",
/// "content": "..."}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Message role (`"system"`, `"user"`, `"assistant"`). The
    /// `PromptPrefill` strategy uses `"assistant"` exclusively;
    /// other strategies leave the field empty.
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Token usage breakdown — sums to the billed total.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens billed.
    pub input_tokens: u64,
    /// Output tokens billed.
    pub output_tokens: u64,
    /// Tokens served from cache (subset of `input_tokens` if cached).
    pub cache_read: u64,
    /// Tokens written to cache (subset of `input_tokens` if novel).
    pub cache_creation: u64,
}

impl Usage {
    /// Total billed tokens (input + output).
    pub fn total(&self) -> u64 {
        let total = self.input_tokens + self.output_tokens;
        tracing::trace!(
            input = self.input_tokens,
            output = self.output_tokens,
            cache_read = self.cache_read,
            cache_creation = self.cache_creation,
            total,
            "Usage::total"
        );
        total
    }
}

/// What happened during an LLM call. Used by the cache + telemetry
/// layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    /// Stable cache key (BLAKE3).
    pub cache_key: String,
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Start unix seconds.
    pub started_unix: i64,
    /// End unix seconds.
    pub ended_unix: i64,
    /// HTTP status, if transport-level.
    pub http_status: Option<u16>,
    /// True if served from cache.
    pub cache_hit: bool,
    /// Usage; zero on transport failure.
    pub usage: Usage,
    /// Truncated error, if any.
    pub error: Option<String>,
}

/// Hash algorithm selector for [`build_cache_key`]. Mirrors
/// `crate::cli::flags_batch::HashAlgo` (the canonical CLI
/// type) so the dispatcher can pass through the user's
/// `--hash-algo` choice without an extra conversion layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheHashAlgo {
    /// SHA-256 (audit-friendly; human-readable with the usual
    /// CLI tooling).
    Sha256,
    /// BLAKE3 (the day-to-day internal hash; ~5–10x faster on
    /// hot paths than SHA-256).
    #[default]
    Blake3,
}

impl From<crate::cli::flags_batch::HashAlgo> for CacheHashAlgo {
    fn from(algo: crate::cli::flags_batch::HashAlgo) -> Self {
        tracing::trace!(from = ?algo, "CacheHashAlgo::from");
        match algo {
            crate::cli::flags_batch::HashAlgo::Sha256 => Self::Sha256,
            crate::cli::flags_batch::HashAlgo::Blake3 => Self::Blake3,
        }
    }
}

/// Build a cache key for `req` using the requested hash
/// algorithm. The canonical input set is `(role, provider,
/// model, system, user, max_tokens, temperature, top_p,
/// prompt_set_hash)` — the same tuple that
/// [`crate::llm::cache::Cache::cache_key`] hashes with BLAKE3.
pub fn build_cache_key(
    req: &LlmRequest,
    provider: &str,
    model: &str,
    algo: CacheHashAlgo,
) -> String {
    use crate::ids::{canonical_hash, sha256_hex};
    use crate::llm::prompts::prompt_set_hash;
    tracing::trace!(
        provider,
        model,
        role = ?req.role,
        algo = ?algo,
        "build_cache_key"
    );
    let prompt_set_hash = prompt_set_hash();
    let parts = [
        "role",
        req.role.as_str(),
        "provider",
        provider,
        "model",
        model,
        "system",
        &req.system,
        "user",
        &req.user,
        "max_tokens",
        &req.max_tokens.map(|n| n.to_string()).unwrap_or_default(),
        "temperature",
        &req.temperature.map(|t| t.to_string()).unwrap_or_default(),
        "top_p",
        &req.top_p.map(|t| t.to_string()).unwrap_or_default(),
        "prompt_set_hash",
        &prompt_set_hash,
    ];
    match algo {
        CacheHashAlgo::Blake3 => canonical_hash(&parts),
        CacheHashAlgo::Sha256 => {
            let mut buf = Vec::new();
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    buf.push(0x1f);
                }
                buf.extend_from_slice(p.as_bytes());
            }
            sha256_hex(&buf)
        }
    }
}

/// SDK contract every LLM impl satisfies.
///
/// Per #900 D1 exactly three impls exist on the long-term surface —
/// `AnthropicClient`, `OpenAIClient` (chat + responses URL variants),
/// and [`MockClient`].
///
/// The trait is `async_trait`-based to match the legacy
/// `crate::llm::provider::Provider`; switching to native async fns
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
    /// Downcast hook the registry uses to identify the
    /// `BreakeredClient` wrapper among the registered entries.
    /// Returns `None` for raw SDK impls (`insert_raw` paths).
    /// [`super::BreakeredClient`] overrides to return
    /// `Some(self)`.
    fn as_breakered(&self) -> Option<&super::BreakeredClient> {
        None
    }
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
    /// pay any cost for the trait method.
    fn set_param_rejections(
        &self,
        _table: Arc<crate::llm::param_rejections::ParamRejectionsTable>,
    ) {
    }

    /// Bare single-call surface. Each concrete impl implements
    /// this — `MockClient::send_once` returns a programmed
    /// response, `AnthropicClient::send_once` /
    /// `OpenAIClient::send_once` make the HTTP call + apply the
    /// max-tokens clamp + build the wire body.
    async fn send_once(&self, req: &LlmRequest) -> Result<LlmResponse>;

    /// Cascade-aware send entry point. Per #900 D9 the trait
    /// absorbs the cascade preflight + retry. The default impl runs
    /// the preflight omit (consulting
    /// [`Self::param_rejections_table`]) and the bounded retry
    /// loop (capped at [`PARAM_NAMES`] iterations) around
    /// [`Self::send_once`].
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
    /// ("no compat layer") holds.
    fn body_sha256(&self, req: &LlmRequest) -> Result<String>;

    /// Probe-bypass variant for the auto-probe (skips the safety
    /// wire-clamp AND the cascade — the probe algorithm needs the
    /// bare upstream behaviour, not the auto-healing cascade the
    /// dispatcher layers on top). Default forwards to
    /// [`Self::send_once`].
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

    /// Internal hook the registry uses to attach per-call state
    /// (saturation sink, …) to `BreakeredClient` wrappers. Default
    /// is a no-op so SDK impls that do not participate in the
    /// registry's wrapper layer pay nothing for this method.
    /// [`super::BreakeredClient`] overrides to forward to the
    /// sink the registry installed.
    fn attach_saturation_sink(
        &self,
        _sink: std::sync::Arc<dyn crate::llm::client::compat::SaturationSink>,
    ) {
    }
}

/// Clear an optional wire field on an [`LlmRequest`] so the cascade
/// retry does not re-emit a parameter the upstream already rejected.
///
/// Mirrors the legacy `omit_param` field-for-field: `temperature`,
/// `top_p`, and `max_tokens` clear to `None`; unknown parameters are
/// no-ops so the cascade loop can call this helper unconditionally.
pub fn omit_param_llm(req: &mut LlmRequest, param: &str) {
    tracing::debug!(param, "omit_param_llm: clearing optional wire field");
    match param {
        "temperature" => req.temperature = None,
        "top_p" => req.top_p = None,
        "max_tokens" => req.max_tokens = None,
        _ => {
            tracing::trace!(param, "omit_param_llm: unknown parameter, no-op");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LlmRequest::max_tokens = None` must round-trip as field-absent
    /// so the wire body never emits a literal `"max_tokens": null`.
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
    /// value preserved.
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
    /// only needs a single return value (`Result<LlmResponse>`).
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
    /// reachable without an extra accessor.
    #[test]
    fn capabilities_deref_to_provider_capabilities() {
        let cap = LlmCapabilities(ProviderCapabilities::default());
        assert_eq!(cap.wire_format_id(), "openai_compatible");
        assert_eq!(cap.0.wire_format_id(), "openai_compatible");
    }
}
