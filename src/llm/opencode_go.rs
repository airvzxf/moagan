//! `opencode_go` provider — dispatcher for OpenCode Go's three
//! wire-format endpoints (per the 2026-08-04 operator model roster).
//!
//! OpenCode Go exposes models under three different SDK flavors:
//!
//! - `/v1/chat/completions` (OpenAI-compatible, `@ai-sdk/openai-compatible`):
//!   glm-5.1, glm-5.2, kimi-k3, kimi-k2.7-code, kimi-k2.6, deepseek-v4-pro,
//!   deepseek-v4-flash, mimo-v2.5, mimo-v2.5-pro, hy3
//! - `/v1/messages` (Anthropic-compatible, `@ai-sdk/anthropic`):
//!   minimax-m3, minimax-m2.7, minimax-m2.5, qwen3.8-max, qwen3.7-max,
//!   qwen3.7-plus, qwen3.6-plus
//! - `/v1/responses` (OpenAI Responses API, `@ai-sdk/openai`):
//!   gpt-5.6-luna
//!
//! Operator policy (documented, not enforced by code): the
//! `minimax-*` family is BLOCKED — the operator prefers direct
//! MiniMax access. The `deepseek-*` family is a fallback when the
//! direct DeepSeek credits run out.
//!
//! ## Per-model temperature overrides (Fix #5, validation-2026-08-04)
//!
//! Some OpenCode Go models (notably `kimi-k3`) reject the per-role
//! temperature with HTTP 400 — they only accept `temperature=1`. The
//! [`MODEL_TEMPERATURE_OVERRIDES`] map below lists the known
//! workarounds. The retry path in
//! `phase.rs::call_with_retry_parse` (`call_with_retry_parse`)
//! surface these errors so the operator can extend the map when a
//! new model is added.

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::{OPENCODE_GO_MAX_TOKENS_CAP, ProviderCapabilities};
use super::openai_compat::OpenAiCompatProvider;
use super::opencode_go_anthropic::OpenCodeGoAnthropicProvider;
use super::opencode_go_responses::OpenCodeGoResponsesProvider;
use super::provider::Provider;
use super::wire::{Request, Response};

/// Per-model temperature overrides for OpenCode Go. Lookup by
/// `cfg.model` (the exact model string the provider sends to the
/// upstream). When an entry is present the per-role temperature
/// from `phase.rs::temperature_for_role` is ignored; the value
/// here is what the upstream API expects.
pub static MODEL_TEMPERATURE_OVERRIDES: &[(&str, f32)] = &[
    // kimi-k3 on OpenCode Go rejects any temperature != 1.0 with
    // HTTP 400 "only 1 is allowed for this model". Discovered via
    // Bloque E connectivity test on 2026-08-04.
    ("kimi-k3", 1.0),
];

/// Look up the configured temperature override for a model. Returns
/// `None` when the model sends its per-role temperature unchanged.
pub fn temperature_override_for(model: &str) -> Option<f32> {
    MODEL_TEMPERATURE_OVERRIDES
        .iter()
        .find(|(name, _)| *name == model)
        .map(|(_, t)| *t)
}

/// Sub-trait used by the dispatcher to retrieve the routed URL.
/// All three concrete OpenCode Go providers (Anthropic-compatible,
/// OpenAI Responses, OpenAI Chat Completions) implement this; the
/// dispatcher keeps an `OpenCodeGoDispatch` instead of a plain
/// `Provider` so its `url()` method can be called without
/// downcasting.
///
/// `url()` returns the request URL (base + model-routed path). The
/// `Provider::endpoint()` contract is the stable identifier and only
/// carries the base; the path component is dispatcher-specific and
/// lives on this sub-trait.
pub trait OpenCodeGoDispatch: Provider {
    /// Fully-routed HTTP URL the provider will POST to on a `send()`
    /// call. Built by the inner provider's URL helper from the base
    /// endpoint plus the model-specific path (`/v1/messages`,
    /// `/v1/responses`, or `/v1/chat/completions`).
    fn url(&self) -> String;
}

/// Endpoint path for a model on OpenCode Go, derived from the
/// operator's 2026-08-04 model roster. Returns `None` for models
/// that are not on the OpenCode Go subscription.
pub fn endpoint_path_for(model: &str) -> Option<&'static str> {
    match model {
        // /v1/responses (OpenAI Responses API)
        "gpt-5.6-luna" => Some("responses"),
        // /v1/messages (Anthropic-compatible)
        "minimax-m3" | "minimax-m2.7" | "minimax-m2.5" | "qwen3.8-max" | "qwen3.7-max"
        | "qwen3.7-plus" | "qwen3.6-plus" => Some("messages"),
        // /v1/chat/completions (OpenAI-compatible)
        "glm-5.1" | "glm-5.2" | "kimi-k3" | "kimi-k2.7-code" | "kimi-k2.6" | "deepseek-v4-pro"
        | "deepseek-v4-flash" | "mimo-v2.5" | "mimo-v2.5-pro" | "hy3" => Some("chat/completions"),
        _ => None,
    }
}

/// Dispatcher. Picks the right concrete provider based on the model
/// in the spec. The dispatcher is the only entry point the registry
/// uses; the concrete providers above are constructor-only from
/// outside tests.
pub struct OpenCodeGoProvider {
    inner: Box<dyn OpenCodeGoDispatch>,
}

impl OpenCodeGoProvider {
    /// Build from an OpenCode Go provider config and a resolved API
    /// key. Routes to the appropriate wire-format provider based on
    /// the model name: each model on the operator's 2026-08-04 roster
    /// has a known endpoint path. The `spec.endpoint` is used as the
    /// base (default `https://opencode.ai/zen/go/v1`) and the
    /// model-specific path is appended.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "opencode_go" {
            return Err(Error::InvalidArgs(format!(
                "opencode_go provider got kind '{}'",
                spec.kind
            )));
        }
        if Self::is_blocked(&spec.model) {
            return Err(Error::InvalidArgs(format!(
                "model '{}' is blocked for opencode_go; use direct minimax provider instead",
                spec.model
            )));
        }
        let path = endpoint_path_for(&spec.model).ok_or_else(|| {
            Error::InvalidArgs(format!(
                "model '{}' is not on the OpenCode Go model roster",
                spec.model
            ))
        })?;
        // Build a synthetic spec with the base endpoint so the concrete
        // provider's URL builder appends the path exactly once. The
        // concrete providers (OpenCodeGoAnthropic,
        // OpenCodeGoResponses, OpenAiCompat) all have url helpers that
        // handle the `base + path` join themselves.
        let routed_spec = ProviderConfig {
            endpoint: spec.endpoint.trim_end_matches('/').to_owned(),
            ..spec.clone()
        };
        let provider: Box<dyn OpenCodeGoDispatch> = if path == "messages" {
            Box::new(OpenCodeGoAnthropicProvider::new(&routed_spec, api_key)?)
        } else if path == "responses" {
            Box::new(OpenCodeGoResponsesProvider::new(&routed_spec, api_key)?)
        } else {
            // Chat-completions wire. Pin the kind-level hard cap so
            // every opencode_go model respects the
            // `OPENCODE_GO_MAX_TOKENS_CAP` ceiling regardless of
            // what `ProviderConfig::max_tokens` carries.
            Box::new(OpenAiCompatProvider::new_with_kind_cap(
                &routed_spec,
                api_key,
                Some(OPENCODE_GO_MAX_TOKENS_CAP),
            )?)
        };
        Ok(Self { inner: provider })
    }

    /// Build from config, resolving the API key via the unified
    /// helper (PR-B2). The helper honours `<MOAGAN_HOME>/api_keys.toml`
    /// first, then falls back to the direct `OPENCODE_GO_API_KEY`
    /// env var so existing CI / shell setups keep working untouched.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = super::api_keys::lookup_key("opencode_go", None)
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "OPENCODE_GO_API_KEY not set; provide via env, --api-key, or api_keys.toml"
                        .into(),
                )
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey(msg) => Error::InvalidApiKey(format!(
                    "opencode_go: {msg}; check api_keys.toml and the env var fallback"
                )),
                other => other,
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// Hard-blocked model names. OpenCode Go offers minimax-m3 /
    /// minimax-m2.7 / minimax-m2.5 on its `/messages` endpoint, but
    /// the operator has a policy: never use the minimax family via
    /// this subscription (prefer direct MiniMax).
    pub const BLOCKED_MODELS: &'static [&'static str] =
        &["minimax-m3", "minimax-m2.7", "minimax-m2.5"];

    /// True when the given model name is in the blocked list.
    pub fn is_blocked(model: &str) -> bool {
        Self::BLOCKED_MODELS.contains(&model)
    }

    /// Fully-routed HTTP URL the dispatcher will POST to on a `send()`
    /// call. Built by the inner provider's URL helper (see
    /// [`OpenCodeGoDispatch`]) from the base endpoint plus the
    /// model-specific path.
    pub fn url(&self) -> String {
        self.inner.url()
    }
}

#[async_trait]
impl Provider for OpenCodeGoProvider {
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
        // Delegate to the routed provider so the dispatcher's
        // capability view matches the wire-format path actually
        // exercised on the wire (Anthropic for messages models,
        // Responses for gpt-5.6-luna, OpenAI-compat for the rest).
        self.inner.capabilities()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        // Fix #5, B (per-model map): apply MODEL_TEMPERATURE_OVERRIDES
        // before forwarding the request. Unknown models keep the
        // per-role temperature.
        let mut effective_req = match temperature_override_for(req.model.as_str()) {
            Some(t) if req.temperature.is_none() || req.temperature != Some(t) => Request {
                temperature: Some(t),
                ..req.clone()
            },
            _ => req.clone(),
        };
        let first = self.inner.send(&effective_req).await;
        // Fix #5, A (retry safety net): if the upstream rejects with
        // a 400 whose body mentions a temperature restriction
        // ("only 1 is allowed for this model", etc.) retry with
        // `temperature=1.0`. This catches models that are added to
        // OpenCode Go after the map is updated — the operator can
        // extend MODEL_TEMPERATURE_OVERRIDES later without losing
        // runtime coverage.
        if let Err(Error::Provider(body)) = &first {
            let lower = body.to_ascii_lowercase();
            let temp_blocked = (lower.contains("invalid temperature")
                || lower.contains("only 1 is allowed")
                || lower.contains("temperature must be"))
                && lower.contains("temperature");
            if temp_blocked && effective_req.temperature != Some(1.0) {
                effective_req = Request {
                    temperature: Some(1.0),
                    ..effective_req
                };
                return self.inner.send(&effective_req).await;
            }
        }
        first
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // The router does NOT clamp on its own — it forwards to the
        // routed provider (Anthropic, Responses, or chat-completions).
        // Delegating preserves the routed provider's clamp chain
        // (e.g. OPENCODE_GO_MAX_TOKENS_CAP for messages and responses,
        // `kind_hard_cap` for chat-completions).
        self.inner.effective_max_tokens(req)
    }

    /// Delegate to the routed provider so each OpenCode Go wire
    /// path observes its own per-kind ceiling. The Anthropic and
    /// Responses providers carry `OPENCODE_GO_MAX_TOKENS_CAP =
    /// 16_384` directly; the chat-completions path goes through
    /// `OpenAiCompatProvider::kind_hard_cap`. Without this
    /// delegation the dispatcher would inherit the trait default
    /// (≈ 1.07G) and the probe would probe values the upstream
    /// rejects on every model routed through OpenCode Go.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        self.inner.max_tokens_probe_ceiling()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            kind: "opencode_go".into(),
            endpoint: "https://opencode.ai/zen/go/v1".into(),
            model: "kimi-k2.7-code".into(),
            max_tokens: Some(8192),
            temperature: Some(0.6),
            top_p: Some(0.95),
            hard_incompatibilities: vec![],
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
        }
    }

    #[test]
    fn is_blocked_recognises_minimax_family() {
        assert!(OpenCodeGoProvider::is_blocked("minimax-m3"));
        assert!(OpenCodeGoProvider::is_blocked("minimax-m2.7"));
        assert!(OpenCodeGoProvider::is_blocked("minimax-m2.5"));
    }

    #[test]
    fn is_blocked_allows_other_models() {
        assert!(!OpenCodeGoProvider::is_blocked("kimi-k2.7-code"));
        assert!(!OpenCodeGoProvider::is_blocked("deepseek-v4-flash"));
        assert!(!OpenCodeGoProvider::is_blocked("gpt-5.6-luna"));
        assert!(!OpenCodeGoProvider::is_blocked("kimi-k3"));
        assert!(!OpenCodeGoProvider::is_blocked("glm-5.2"));
        assert!(!OpenCodeGoProvider::is_blocked("qwen3.7-max"));
        assert!(!OpenCodeGoProvider::is_blocked("mimo-v2.5"));
        assert!(!OpenCodeGoProvider::is_blocked("hy3"));
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
        }
        let result = OpenCodeGoProvider::from_config(&config());
        assert!(matches!(result, Err(Error::InvalidApiKey(_))));
    }

    #[test]
    fn new_errors_when_model_is_blocked() {
        let cfg = ProviderConfig {
            model: "minimax-m3".into(),
            ..config()
        };
        let result = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into()));
        assert!(matches!(result, Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn new_errors_when_kind_mismatch() {
        let cfg = ProviderConfig {
            kind: "minimax".into(),
            ..config()
        };
        let result = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into()));
        assert!(matches!(result, Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn temperature_override_for_known_incompatible_model() {
        assert_eq!(temperature_override_for("kimi-k3"), Some(1.0));
    }

    #[test]
    fn temperature_override_for_unlisted_model_returns_none() {
        assert_eq!(temperature_override_for("kimi-k2.7-code"), None);
        assert_eq!(temperature_override_for("gpt-5.6-luna"), None);
        assert_eq!(temperature_override_for("qwen3.7-max"), None);
    }

    #[test]
    fn detect_temperature_rejection_patterns() {
        // Fix #5, A: the retry safety net triggers on these phrasings.
        let body = r#"{"error":"invalid temperature: only 1 is allowed for this model"}"#;
        let lower = body.to_ascii_lowercase();
        let hits = (lower.contains("invalid temperature") || lower.contains("only 1 is allowed"))
            && lower.contains("temperature");
        assert!(hits);
        // Negative case: regular 400 without temperature mention
        let body2 = r#"{"error":"model not found"}"#;
        let lower2 = body2.to_ascii_lowercase();
        let hits2 = (lower2.contains("invalid temperature")
            || lower2.contains("only 1 is allowed"))
            && lower2.contains("temperature");
        assert!(!hits2);
    }

    #[test]
    fn provider_name_is_opencode_go_for_chat_completions() {
        let provider =
            OpenCodeGoProvider::new(&config(), SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.name(), "opencode_go");
        assert_eq!(provider.model(), "kimi-k2.7-code");
        // `Provider::endpoint()` returns the stable base; the
        // model-routed path is appended by the inner provider's URL
        // helper and exposed via `OpenCodeGoProvider::url()`.
        let base = provider.endpoint();
        assert_eq!(base, "https://opencode.ai/zen/go/v1");
        assert_eq!(
            provider.url(),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
    }

    #[test]
    fn provider_routes_to_anthropic_for_messages_model() {
        let cfg = ProviderConfig {
            model: "qwen3.7-max".into(),
            ..config()
        };
        let provider = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.endpoint(), "https://opencode.ai/zen/go/v1");
        assert_eq!(provider.url(), "https://opencode.ai/zen/go/v1/messages");
    }

    #[test]
    fn provider_routes_to_responses_for_responses_model() {
        let cfg = ProviderConfig {
            model: "gpt-5.6-luna".into(),
            ..config()
        };
        let provider = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.endpoint(), "https://opencode.ai/zen/go/v1");
        assert_eq!(provider.url(), "https://opencode.ai/zen/go/v1/responses");
    }

    #[test]
    fn provider_routes_to_chat_completions_for_chat_model() {
        let cfg = ProviderConfig {
            model: "mimo-v2.5".into(),
            ..config()
        };
        let provider = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.endpoint(), "https://opencode.ai/zen/go/v1");
        assert_eq!(
            provider.url(),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
    }

    #[test]
    fn dispatcher_does_not_double_append_path() {
        // Fix #4 regression: the dispatcher used to construct
        // `endpoint = base + path`, then the concrete provider's url
        // builder appended the path again, producing
        // `/v1/messages/v1/messages`. The base must stay as the base;
        // the routed path must appear exactly once in `url()`.
        let cfg = ProviderConfig {
            model: "qwen3.7-max".into(),
            endpoint: "https://opencode.ai/zen/go/v1".into(),
            ..config()
        };
        let provider = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.endpoint(), "https://opencode.ai/zen/go/v1");
        assert_eq!(provider.url(), "https://opencode.ai/zen/go/v1/messages");
        // No double append: count the routed-path occurrences.
        assert_eq!(
            provider.url().matches("/v1/messages").count(),
            1,
            "routed path must appear exactly once, got: {}",
            provider.url()
        );
    }

    #[test]
    fn provider_rejects_model_not_on_roster() {
        let cfg = ProviderConfig {
            model: "unsupported-model".into(),
            ..config()
        };
        let result = OpenCodeGoProvider::new(&cfg, SecretString::new("dummy".into()));
        assert!(matches!(result, Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn endpoint_path_for_known_models() {
        assert_eq!(endpoint_path_for("gpt-5.6-luna"), Some("responses"));
        assert_eq!(endpoint_path_for("minimax-m3"), Some("messages"));
        assert_eq!(endpoint_path_for("qwen3.7-max"), Some("messages"));
        assert_eq!(
            endpoint_path_for("kimi-k2.7-code"),
            Some("chat/completions")
        );
        assert_eq!(endpoint_path_for("mimo-v2.5-pro"), Some("chat/completions"));
        assert_eq!(endpoint_path_for("hy3"), Some("chat/completions"));
        assert_eq!(
            endpoint_path_for("deepseek-v4-flash"),
            Some("chat/completions")
        );
        assert_eq!(endpoint_path_for("unknown"), None);
    }

    /// Track A.4 contract pin: every default `ProviderConfig` for an
    /// OpenCode Go model registered in `src/config.rs::default_providers`
    /// carries the stable base URL — never the routed path. The path
    /// (`/v1/messages`, `/v1/responses`, `/v1/chat/completions`) is
    /// selected by the dispatcher at construction time, not stored on
    /// the config. The URL builders in `opencode_go_anthropic`,
    /// `opencode_go_responses`, and `openai_compat` already accept both
    /// forms, so this test pins the contract on the config side.
    #[test]
    fn registry_default_endpoints_are_bases() {
        use crate::config::Config;
        let cfg = Config::default();
        let oc_base = "https://opencode.ai/zen/go/v1";
        // One representative model from each wire-format group.
        let representatives = [
            ("opencode_go", "deepseek-v4-flash"), // /v1/chat/completions
            ("minimax-m3", "minimax-m3"),         // /v1/messages
            ("qwen3.7-max", "qwen3.7-max"),       // /v1/messages
            ("qwen3.7-plus", "qwen3.7-plus"),     // /v1/messages
            ("gpt-5.6-luna", "gpt-5.6-luna"),     // /v1/responses
        ];
        for (alias, expected_model) in representatives {
            let spec = cfg
                .providers
                .get(alias)
                .unwrap_or_else(|| panic!("alias {alias} missing from default providers"));
            assert_eq!(
                spec.endpoint, oc_base,
                "OpenCode Go default for {alias} must be the base URL, got: {}",
                spec.endpoint
            );
            assert_eq!(
                spec.model, expected_model,
                "OpenCode Go default for {alias} must carry model {expected_model}"
            );
        }
    }
}
