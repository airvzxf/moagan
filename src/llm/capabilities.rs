//! Static capability matrix for each provider.
//!
//! Every [`Provider`](super::provider::Provider) carries a
//! [`ProviderCapabilities`] record that captures how the provider
//! integrates with the rest of the runtime. The values are
//! `Copy + Eq` so the dispatcher can branch on them without
//! allocating and the capability is purely declarative — no I/O,
//! no caching, no fallibility.
//!
//! Three groups of fields:
//!
//! - **Wire-format preference** (`prefers_anthropic_wire`,
//!   `prefers_openai_wire`, `prefers_responses_wire`). One wins per
//!   provider; the others stay false so the dispatcher can fall
//!   back to the OpenAI-compat default when a provider pins a
//!   different protocol.
//! - **Surface capabilities** (`supports_system_field`,
//!   `supports_response_format`, `supports_streaming`,
//!   `supports_tools`). These let upper layers (sketch generation,
//!   role gating) decide which knobs to expose without touching the
//!   concrete provider.
//! - **`max_input_tokens`**. Capped hint used by the budget
//!   observer to refuse an over-budget call early.

use serde::{Deserialize, Serialize};

/// Hard cap on `max_tokens` for minimax (Anthropic-compatible wire).
///
/// Probe at upstream: MiniMax-M3 rejects anything `> 524288` with
/// Capability matrix for a single provider. Construct via the
/// per-provider `for_*` constructors (`for_minimax`,
/// `for_openai_compat`, etc.) so the call sites do not diverge
/// later. Use [`ProviderCapabilities::default`] only when no
/// per-provider helper fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// Provider understands a top-level `system` field in the
    /// request body. Wire formats that fold the system prompt into
    /// the user role (or pass it as instructions) set this to false.
    pub supports_system_field: bool,
    /// Provider honours `response_format` to constrain output shape.
    pub supports_response_format: bool,
    /// Per-call input cap. `None` means the provider did not
    /// publish a cap and the caller is on its own.
    pub max_input_tokens: Option<u32>,
    /// Provider can stream tokens as they arrive. Skipped in MVP
    /// v0.1.
    pub supports_streaming: bool,
    /// Provider can accept tool / function-call definitions. Skipped
    /// in MVP v0.1.
    pub supports_tools: bool,
    /// Provider expects the Anthropic `/v1/messages` wire shape
    /// (`system` separated from `messages`, `max_tokens` at the
    /// top level, `thinking` block support).
    pub prefers_anthropic_wire: bool,
    /// Provider expects the OpenAI `/v1/chat/completions` wire
    /// shape (`messages` array, optional `response_format`). Most
    /// generic providers live here.
    pub prefers_openai_wire: bool,
    /// Provider expects the OpenAI Responses API shape (`input`
    /// string, `instructions` for the system prompt).
    pub prefers_responses_wire: bool,
}

impl Default for ProviderCapabilities {
    /// Defaults match the OpenAI-compat baseline. Providers that
    /// follow a different protocol override the wire flags through
    /// their per-provider constructors.
    fn default() -> Self {
        Self {
            supports_system_field: true,
            supports_response_format: true,
            max_input_tokens: Some(8192),
            supports_streaming: false,
            supports_tools: false,
            prefers_anthropic_wire: false,
            prefers_openai_wire: true,
            prefers_responses_wire: false,
        }
    }
}

impl ProviderCapabilities {
    /// Direct MiniMax provider (`minimax`). Speaks the
    /// Anthropic-compatible `/v1/messages` wire.
    pub fn for_minimax() -> Self {
        tracing::trace!("ProviderCapabilities::for_minimax");
        Self {
            supports_system_field: true,
            supports_response_format: false,
            max_input_tokens: Some(8192),
            supports_streaming: false,
            supports_tools: false,
            prefers_anthropic_wire: true,
            prefers_openai_wire: false,
            prefers_responses_wire: false,
        }
    }

    /// Generic OpenAI-compat provider (DeepSeek, OpenCode's
    /// `/v1/chat/completions`).
    pub fn for_openai_compat() -> Self {
        Self::default()
    }

    /// DeepSeek provider. Same shape as the generic OpenAI-compat
    /// baseline; the constructor exists for symmetry and to give
    /// the operator a hook to tune without touching other
    /// providers.
    pub fn for_deepseek() -> Self {
        Self::default()
    }

    /// OpenCode dispatcher. The dispatch happens at the URL
    /// layer; the capability vector here is the chat-completions
    /// default — the inner provider already does the routing.
    pub fn for_opencode() -> Self {
        Self::default()
    }

    /// OpenCode routed through the Anthropic-compatible
    /// `/v1/messages` endpoint.
    pub fn for_anthropic_compat() -> Self {
        Self {
            supports_system_field: true,
            supports_response_format: false,
            max_input_tokens: Some(8192),
            supports_streaming: false,
            supports_tools: false,
            prefers_anthropic_wire: true,
            prefers_openai_wire: false,
            prefers_responses_wire: false,
        }
    }

    /// OpenCode routed through the OpenAI Responses API
    /// (`/v1/responses`).
    pub fn for_opencode_responses() -> Self {
        Self {
            supports_system_field: false,
            supports_response_format: true,
            max_input_tokens: Some(8192),
            supports_streaming: false,
            supports_tools: false,
            prefers_anthropic_wire: false,
            prefers_openai_wire: false,
            prefers_responses_wire: true,
        }
    }

    /// Local mock provider used by smoke tests. Declares full
    /// streaming and tools support so the dispatcher can exercise
    /// every branch deterministically.
    pub fn for_mock() -> Self {
        Self {
            supports_system_field: true,
            supports_response_format: true,
            max_input_tokens: Some(8192),
            supports_streaming: true,
            supports_tools: true,
            prefers_anthropic_wire: false,
            prefers_openai_wire: true,
            prefers_responses_wire: false,
        }
    }

    /// Resolve a static wire-format identifier from the
    /// preference flags. Mirrors the serde rename on
    /// [`crate::llm::client::WireFormatId`] so log lines and
    /// the JSON serialisation always agree:
    ///
    /// * `prefers_anthropic_wire` → `"anthropic"` (`/v1/messages`)
    /// * `prefers_responses_wire` → `"openai"` (`/v1/responses`,
    ///   the OpenAI Responses API)
    /// * fallback                 → `"openai_compatible"`
    ///   (`/v1/chat/completions`, the OpenAI-compatible Chat API)
    ///
    /// The dispatcher reads the same id from
    /// `WireFormatId::as_str()` so a future refactor cannot
    /// silently diverge the two surfaces.
    pub fn wire_format_id(&self) -> &'static str {
        if self.prefers_anthropic_wire {
            "anthropic"
        } else if self.prefers_responses_wire {
            "openai"
        } else {
            "openai_compatible"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defaults are the OpenAI-compat baseline: no Anthropic or
    /// Responses preference, 8k input cap, system + response_format
    /// supported. The dispatcher's back-compat fallback chains on
    /// this so callers that omit a `for_*` override still land on
    /// `OpenAiWire`.
    #[test]
    fn capabilities_default_is_openai_compatible() {
        let cap = ProviderCapabilities::default();
        assert!(cap.prefers_openai_wire);
        assert!(!cap.prefers_anthropic_wire);
        assert!(!cap.prefers_responses_wire);
        assert!(cap.supports_system_field);
        assert!(cap.supports_response_format);
        assert_eq!(cap.max_input_tokens, Some(8192));
        assert_eq!(cap.wire_format_id(), "openai_compatible");
    }

    /// Direct MiniMax flips the wire preference to Anthropic and
    /// downgrades `supports_response_format` (MiniMax rejects the
    /// OpenAI `response_format: json_object` flag — JSON output
    /// has to ride on the prompt).
    #[test]
    fn capabilities_for_minimax_prefers_anthropic_wire() {
        let cap = ProviderCapabilities::for_minimax();
        assert!(cap.prefers_anthropic_wire);
        assert!(!cap.prefers_openai_wire);
        assert!(!cap.prefers_responses_wire);
        assert!(cap.supports_system_field);
        assert!(!cap.supports_response_format);
        assert_eq!(cap.wire_format_id(), "anthropic");
    }

    /// `opencode_responses` is the OpenAI Responses API slot.
    /// The flag flips to `responses` and the OpenAI baseline
    /// flags drop; `supports_response_format` survives because
    /// the Responses API still honours it.
    #[test]
    fn capabilities_for_opencode_responses_prefers_responses_wire() {
        let cap = ProviderCapabilities::for_opencode_responses();
        assert!(cap.prefers_responses_wire);
        assert!(!cap.prefers_anthropic_wire);
        assert!(!cap.prefers_openai_wire);
        // v0.10 (Phase 2 wire-format rename): the OpenAI Responses
        // wire reports its serde id as `"openai"`. The legacy
        // `"responses"` spelling is gone; tests pin the canonical
        // value the dispatcher and telemetry agree on.
        assert_eq!(cap.wire_format_id(), "openai");
    }

    /// Mock provider is the test-time escape hatch — streaming
    /// and tools support are advertised so the dispatcher can
    /// exercise every branch without touching a live provider.
    #[test]
    fn capabilities_for_mock_advertises_streaming_and_tools() {
        let cap = ProviderCapabilities::for_mock();
        assert!(cap.supports_streaming);
        assert!(cap.supports_tools);
    }

    /// Round-trip the capability record through serde so external
    /// tooling (telemetry, dashboards) can read the matrix.
    #[test]
    fn capabilities_serde_round_trip() {
        let cap = ProviderCapabilities::for_minimax();
        let json = serde_json::to_string(&cap).unwrap();
        let back: ProviderCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(cap, back);
    }

    // Phase 2 (CAP removal): the kind-level `*_MAX_TOKENS_CAP`
    // constants are gone — the runtime respects whatever `max_tokens`
    // the caller passes and the cascade auto-heal handles upstream
    // rejection. The tests that pinned those values to a specific API
    // boundary (`524_288` for MiniMax, `393_216` for DeepSeek) were
    // deleted alongside them; the real boundaries now live in the
    // param-rejections cascade table (see `param_rejections.rs`
    // pattern #4C and the DeepSeek wire-text extractor) and in the
    // persisted `<MOAGAN_HOME>/*_auto.toml` sidecars.
}
