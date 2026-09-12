//! URL-path dispatcher (D2).
//!
//! Picks the right [`LlmClient`] SDK impl by matching the endpoint
//! URL's path suffix. Operates **in parallel with** the legacy
//! section-name dispatcher at `src/llm/provider.rs:1423-1438` until
//! issue #933 deletes the legacy tree; `registry_from_config` keeps
//! routing through the legacy dispatcher until the migration wave
//! (issues #5-#11 in EPIC #847) lands.
//!
//! Per #900 D2 the URL is the single source of truth for SDK
//! selection — no `sdk = "..."` knob in the config, no section-name
//! fast-paths. The dispatcher extends the existing
//! [`wire_format_from_url`](crate::llm::wire_format::wire_format_from_url)
//! helper with the URL-without-`/v1/` variants (`/messages`,
//! `/chat/completions`, `/responses`) so hosts that don't prefix
//! with `/v1/` still resolve, plus explicit `mock://...` handling so
//! the test-time mock fits the same shape as the live SDKs.
//!
//! EPIC #847 — issue #922 (`feat(llm): URL-path dispatcher (D2) —
//! src/llm/client/dispatcher.rs`). Wave 1.4 of the EPIC, immediately
//! after #919 (`LlmClient` trait + `MockClient`), #920
//! (`AnthropicClient`), and #921 (`OpenAIClient`). The PR that
//! rewires `registry_from_config` to use this dispatcher lives in
//! the migration wave (issues #5-#11).

use std::sync::Arc;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::LlmClient;
use super::anthropic::AnthropicClient;
use super::mock::MockClient;
use super::openai::OpenAIClient;

/// SDK kind picked by URL path. The runtime holds one SDK impl per
/// `(section, model)` pair; the dispatcher pins which impl at
/// construction time. Per #900 D1 exactly three live impls exist on
/// the long-term surface (`AnthropicClient`, `OpenAIClient` covering
/// both URL variants, `MockClient`) plus the trait itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkKind {
    /// `/v1/messages` (or `/messages`) → [`AnthropicClient`].
    Anthropic,
    /// `/v1/chat/completions` (or `/chat/completions`) →
    /// [`OpenAIClient`] with the chat-completions variant.
    OpenAIChat,
    /// `/v1/responses` (or `/responses`) → [`OpenAIClient`] with
    /// the Responses API variant.
    OpenAIResponses,
    /// `mock://...` → [`MockClient`].
    Mock,
}

/// Map an endpoint URL to its [`SdkKind`]. Pure function — no I/O,
/// no allocation beyond the local `&str` slice. Strips the query
/// string and a trailing `/` so URLs like
/// `https://example.com/v1/messages?token=abc` resolve the same way
/// as the canonical path.
///
/// The `mock://` prefix is matched against the **raw** endpoint (not
/// the normalized path) so callers can pass any mock URI without
/// tripping the suffix matcher.
pub fn pick_sdk(endpoint: &str) -> Result<SdkKind> {
    let normalized = endpoint
        .split('?')
        .next()
        .unwrap_or(endpoint)
        .trim_end_matches('/');
    if normalized.ends_with("/v1/messages") || normalized.ends_with("/messages") {
        Ok(SdkKind::Anthropic)
    } else if normalized.ends_with("/v1/chat/completions")
        || normalized.ends_with("/chat/completions")
    {
        Ok(SdkKind::OpenAIChat)
    } else if normalized.ends_with("/v1/responses") || normalized.ends_with("/responses") {
        Ok(SdkKind::OpenAIResponses)
    } else if endpoint.starts_with("mock://") {
        Ok(SdkKind::Mock)
    } else {
        Err(Error::InvalidArgs(format!(
            "endpoint URL '{endpoint}' does not end in /v1/messages, /v1/chat/completions, \
             /v1/responses, or mock:// — dispatcher cannot pick an SDK"
        )))
    }
}

/// True when the SDK should wire the DeepSeek chat-variant hard
/// cap. The legacy
/// [`crate::llm::openai_compatible::OpenAICompatibleProvider::new_with_kind_cap`]
/// path activates `Some(DEEPSEEK_MAX_TOKENS_CAP)` only when the
/// section name is `"deepseek"`; the new dispatcher keeps that
/// wiring by also detecting the DeepSeek host so any operator that
/// declares a DeepSeek URL under a custom section name (or runs the
/// dispatcher through the resolved-config path) still gets the
/// cap.
fn is_deepseek_endpoint(endpoint: &str, section_name: &str) -> bool {
    section_name == "deepseek" || endpoint.contains("deepseek.com")
}

/// Build the SDK client the runtime will route through.
///
/// * `cfg` carries the operator-declared endpoint URL (used by
///   [`pick_sdk`] for SDK selection and, when present, the
///   per-model `endpoint` override that becomes the SDK's URL).
/// * `api_key` is the resolved API key the SDK uses for
///   `Authorization`.
/// * `section_name` is the `[providers.<name>]` section header; the
///   dispatcher consults it only to wire the DeepSeek chat-variant
///   hard cap (the URL alone decides the SDK).
///
/// Per #900 D2 there is no section-name fast-path for SDK
/// selection — `minimax` and `deepseek` no longer branch the
/// dispatch because the URL suffix carries the routing decision.
/// The DeepSeek cap is the only section-name-driven knob that
/// remains (and only on the chat-completions variant).
pub fn build_client(
    cfg: &ProviderConfig,
    api_key: SecretString,
    section_name: &str,
) -> Result<Arc<dyn LlmClient>> {
    let endpoint = cfg.endpoint.as_deref().unwrap_or("");
    match pick_sdk(endpoint)? {
        SdkKind::Anthropic => Ok(Arc::new(AnthropicClient::new(cfg, api_key)?)),
        SdkKind::OpenAIChat => {
            let mut client = OpenAIClient::new(cfg, api_key)?;
            if is_deepseek_endpoint(endpoint, section_name) {
                client = client
                    .with_kind_hard_cap(Some(crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP));
            }
            Ok(Arc::new(client))
        }
        SdkKind::OpenAIResponses => Ok(Arc::new(OpenAIClient::new(cfg, api_key)?)),
        SdkKind::Mock => Ok(Arc::new(MockClient::empty())),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests pinning the URL → SDK mapping the dispatcher
    //! commits to, plus a parity test against the legacy
    //! [`wire_format_from_url`](crate::llm::wire_format::wire_format_from_url)
    //! so a future contributor cannot silently let the two
    //! classifiers drift.
    //!
    //! Every URL in the corpus covers a public provider the
    //! operator can declare in `config.toml`. The parity test
    //! confirms the dispatcher's `SdkKind` and the legacy
    //! `WireFormatId` agree on the wire format for every URL
    //! both functions accept (Mock URLs are excluded — mock has
    //! no `WireFormatId` equivalent).

    use super::*;
    use crate::config::ModelConfig;
    use crate::llm::wire_format::{WireFormatId, wire_format_from_url};

    fn cfg(endpoint: &str) -> ProviderConfig {
        ProviderConfig {
            models: vec![ModelConfig {
                id: "m".into(),
                endpoint: Some(endpoint.into()),
                max_tokens: None,
                omit_max_tokens: false,
            }],
            endpoint: Some(endpoint.into()),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            plan: None,
        }
    }

    // ---- pick_sdk: the 10 URL cases pinned by the issue ----

    /// `https://api.minimax.io/anthropic/v1/messages` is the
    /// canonical minimax route through the Anthropic-compat wire.
    #[test]
    fn pick_sdk_minimax_url_is_anthropic() {
        let got = pick_sdk("https://api.minimax.io/anthropic/v1/messages")
            .expect("minimax /v1/messages must resolve");
        assert_eq!(got, SdkKind::Anthropic);
    }

    /// `https://api.deepseek.com/v1/chat/completions` is the
    /// DeepSeek Chat Completions route.
    #[test]
    fn pick_sdk_deepseek_chat() {
        let got = pick_sdk("https://api.deepseek.com/v1/chat/completions")
            .expect("deepseek /v1/chat/completions must resolve");
        assert_eq!(got, SdkKind::OpenAIChat);
    }

    /// `https://opencode.ai/zen/go/v1/responses` is the OpenCode
    /// Responses API route.
    #[test]
    fn pick_sdk_opencode_responses() {
        let got = pick_sdk("https://opencode.ai/zen/go/v1/responses")
            .expect("opencode /v1/responses must resolve");
        assert_eq!(got, SdkKind::OpenAIResponses);
    }

    /// `https://opencode.ai/zen/go/v1/chat/completions` is the
    /// OpenCode Chat Completions route.
    #[test]
    fn pick_sdk_opencode_chat() {
        let got = pick_sdk("https://opencode.ai/zen/go/v1/chat/completions")
            .expect("opencode /v1/chat/completions must resolve");
        assert_eq!(got, SdkKind::OpenAIChat);
    }

    /// `https://opencode.ai/zen/go/v1/messages` is the OpenCode
    /// Anthropic-compat route — `/v1/messages` resolves to the
    /// Anthropic SDK regardless of the host.
    #[test]
    fn pick_sdk_opencode_messages() {
        let got = pick_sdk("https://opencode.ai/zen/go/v1/messages")
            .expect("opencode /v1/messages must resolve");
        assert_eq!(got, SdkKind::Anthropic);
    }

    /// `mock://...` always picks the mock SDK regardless of the
    /// rest of the URL.
    #[test]
    fn pick_sdk_mock_prefix() {
        let got = pick_sdk("mock://local").expect("mock:// must resolve");
        assert_eq!(got, SdkKind::Mock);
    }

    /// A trailing query string must not trip the suffix matcher.
    #[test]
    fn pick_sdk_query_stripped() {
        let got = pick_sdk("https://example.com/v1/messages?token=abc")
            .expect("query string must be stripped");
        assert_eq!(got, SdkKind::Anthropic);
    }

    /// A trailing `/` must not trip the suffix matcher.
    #[test]
    fn pick_sdk_trailing_slash_stripped() {
        let got =
            pick_sdk("https://example.com/v1/messages/").expect("trailing slash must be stripped");
        assert_eq!(got, SdkKind::Anthropic);
    }

    /// An unrecognised path suffix returns `Error::InvalidArgs`
    /// so the operator gets a clear startup error rather than a
    /// silent wrong-SDK fallback.
    #[test]
    fn pick_sdk_unknown_returns_error() {
        let err = pick_sdk("https://example.com/v1/unknown").expect_err("unknown path must error");
        match err {
            Error::InvalidArgs(msg) => {
                assert!(
                    msg.contains("/v1/messages"),
                    "error must list recognised suffixes, got {msg:?}"
                );
                assert!(
                    msg.contains("/v1/chat/completions"),
                    "error must list recognised suffixes, got {msg:?}"
                );
                assert!(
                    msg.contains("/v1/responses"),
                    "error must list recognised suffixes, got {msg:?}"
                );
                assert!(
                    msg.contains("mock://"),
                    "error must list recognised suffixes, got {msg:?}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    /// An empty endpoint is treated as "no URL" and rejected with
    /// the same `Error::InvalidArgs` so the operator gets the same
    /// clear error class as an unrecognised suffix.
    #[test]
    fn pick_sdk_empty_returns_error() {
        let err = pick_sdk("").expect_err("empty endpoint must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "empty endpoint must be InvalidArgs, got {err:?}"
        );
    }

    // ---- build_client: the 4 SDK selection cases ----

    /// `build_client` for an Anthropic URL produces an
    /// `AnthropicClient` (`sdk_type() == "anthropic"`).
    #[test]
    fn build_client_anthropic_sdk_type_is_anthropic() {
        let client = build_client(
            &cfg("https://api.minimax.io/anthropic/v1/messages"),
            SecretString::new("dummy".into()),
            "minimax",
        )
        .expect("build_client anthropic");
        assert_eq!(client.sdk_type(), "anthropic");
        assert_eq!(client.name(), "m", "model id flows from cfg.models[0]");
    }

    /// `build_client` for a chat-completions URL produces an
    /// `OpenAIClient` (`sdk_type() == "openai_compatible"`).
    #[test]
    fn build_client_openai_chat_sdk_type_is_openai_compatible() {
        let client = build_client(
            &cfg("https://api.deepseek.com/v1/chat/completions"),
            SecretString::new("dummy".into()),
            "deepseek",
        )
        .expect("build_client openai chat");
        assert_eq!(client.sdk_type(), "openai_compatible");
        assert_eq!(client.name(), "m");
    }

    /// `build_client` for a Responses URL produces an
    /// `OpenAIClient` (`sdk_type() == "openai"`).
    #[test]
    fn build_client_openai_responses_sdk_type_is_openai() {
        let client = build_client(
            &cfg("https://opencode.ai/zen/go/v1/responses"),
            SecretString::new("dummy".into()),
            "opencode",
        )
        .expect("build_client openai responses");
        assert_eq!(client.sdk_type(), "openai");
        assert_eq!(client.name(), "m");
    }

    /// `build_client` for a `mock://` URL produces a
    /// `MockClient` (`sdk_type() == "mock"`).
    #[test]
    fn build_client_mock_sdk_type_is_mock() {
        // The mock SDK ignores `cfg.endpoint` for SDK selection —
        // the dispatcher routes on the URL only, so any `mock://`
        // URL works regardless of how `ProviderConfig` is
        // populated.
        let client = build_client(
            &cfg("mock://local"),
            SecretString::new("dummy".into()),
            "mock",
        )
        .expect("build_client mock");
        assert_eq!(client.sdk_type(), "mock");
    }

    // ---- parity with wire_format_from_url ----

    /// Map a [`SdkKind`] to its [`WireFormatId`] equivalent. Mock
    /// has no `WireFormatId`; the parity test skips those URLs
    /// because the legacy classifier cannot speak mock.
    fn sdk_kind_to_wire_format(kind: SdkKind) -> Option<WireFormatId> {
        match kind {
            SdkKind::Anthropic => Some(WireFormatId::Anthropic),
            SdkKind::OpenAIChat => Some(WireFormatId::OpenAICompatible),
            SdkKind::OpenAIResponses => Some(WireFormatId::OpenAI),
            SdkKind::Mock => None,
        }
    }

    /// Every URL the dispatcher accepts (excluding the `mock://`
    /// prefix, which has no `WireFormatId` equivalent) must
    /// resolve to the same `WireFormatId` as
    /// [`wire_format_from_url`]. Pins the dispatcher's parity
    /// contract with the legacy classifier so the migration PRs
    /// can flip `registry_from_config` to the new dispatcher
    /// without any operator-visible wire-format drift.
    #[test]
    fn parity_with_wire_format_from_url() {
        let corpus = [
            "https://api.minimax.io/anthropic/v1/messages",
            "https://api.deepseek.com/v1/chat/completions",
            "https://opencode.ai/zen/go/v1/responses",
            "https://opencode.ai/zen/go/v1/chat/completions",
            "https://opencode.ai/zen/go/v1/messages",
            "https://api.minimax.io/anthropic/v1/messages?api-version=2023-06-01",
            "https://opencode.ai/zen/go/v1/chat/completions/",
            "https://api.deepseek.com/chat/completions",
        ];
        for url in corpus {
            let sdk = pick_sdk(url).unwrap_or_else(|e| {
                panic!("pick_sdk must accept {url}: {e:?}");
            });
            let wf = wire_format_from_url(url).unwrap_or_else(|e| {
                panic!("wire_format_from_url must accept {url}: {e:?}");
            });
            let mapped = sdk_kind_to_wire_format(sdk).unwrap_or_else(|| {
                panic!("{url} resolved to SdkKind::Mock which has no WireFormatId parity")
            });
            assert_eq!(
                mapped, wf,
                "dispatcher parity broken for {url}: pick_sdk -> {sdk:?}, \
                 wire_format_from_url -> {wf:?}"
            );
        }
    }
}
