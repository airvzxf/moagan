//! `deepseek` provider — DeepSeek's OpenAI-compat API at
//! `https://api.deepseek.com/v1/chat/completions`.
//!
//! This is a thin wrapper around `OpenAiCompatProvider` that pre-fills
//! the DeepSeek-specific defaults (endpoint, model, API key env).

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::{DEEPSEEK_MAX_TOKENS_CAP, ProviderCapabilities};
use super::openai_compat::OpenAiCompatProvider;
use super::provider::Provider;
use super::wire::{Request, Response};

/// DeepSeek provider backed by the generic OpenAI-compat implementation.
#[derive(Debug, Clone)]
pub struct DeepSeekProvider(OpenAiCompatProvider);

impl DeepSeekProvider {
    /// Build from a DeepSeek provider config and a resolved API key.
    ///
    /// Wires `DEEPSEEK_MAX_TOKENS_CAP = 393_216` as the
    /// `kind_hard_cap` via [`OpenAiCompatProvider::new_with_kind_cap`]
    /// so every wire body carries the per-provider ceiling even
    /// when the operator's TOML leaves `max_tokens` unset
    /// (`DEFAULT_MAX_TOKENS = 1_000_000`). Without this cap the
    /// upstream returns HTTP 400 `invalid_request_error` with the
    /// body `{"message":"Invalid max_tokens value, the valid
    /// range of max_tokens is [1, 393216]"}`. The same value is
    /// returned by [`Self::max_tokens_probe_ceiling`] so the
    /// auto-probe short-circuits at `2^19 = 524_288` (the first
    /// `2^k > 393_216`) instead of probing values the upstream
    /// will never accept. Mirrors the
    /// `OPENCODE_GO_MAX_TOKENS_CAP` wiring on the opencode_go
    /// chat-completions path (`opencode_go.rs:152`).
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "deepseek" {
            return Err(Error::InvalidArgs(format!(
                "deepseek provider got kind '{}'",
                spec.kind
            )));
        }
        Ok(Self(OpenAiCompatProvider::new_with_kind_cap(
            spec,
            api_key,
            Some(DEEPSEEK_MAX_TOKENS_CAP),
        )?))
    }

    /// Build from config, resolving the API key via the unified
    /// helper (PR-B2). The helper honours `<MOAGAN_HOME>/api_keys.toml`
    /// first, then falls back to the direct `DEEPSEEK_API_KEY` env var
    /// so existing CI / shell setups keep working untouched.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = super::api_keys::lookup_key("deepseek", None)
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "DEEPSEEK_API_KEY not set; provide via env, --api-key, or api_keys.toml".into(),
                )
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey(msg) => Error::InvalidApiKey(format!(
                    "deepseek: {msg}; check api_keys.toml and the env var fallback"
                )),
                other => other,
            })?;
        Self::new(spec, SecretString::new(key))
    }
}

impl std::ops::Deref for DeepSeekProvider {
    type Target = OpenAiCompatProvider;

    fn deref(&self) -> &OpenAiCompatProvider {
        &self.0
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn model(&self) -> &str {
        self.0.model()
    }

    fn endpoint(&self) -> &str {
        self.0.endpoint()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Delegate to the wrapped OpenAI-compat. The inner impl
        // reads `self.name == "deepseek"` and returns the deepseek
        // variant; thin wrapper around it carries through.
        self.0.capabilities()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        self.0.send(req).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Delegate to the wrapped OpenAI-compat provider so the
        // clamp chain (operator override + `kind_hard_cap` + table)
        // is the same one `send()` applies. DeepSeek-direct wires
        // `kind_hard_cap = Some(DEEPSEEK_MAX_TOKENS_CAP)` at
        // construction time via [`Self::new`] so the per-provider
        // hard cap is honoured at every layer (audit-log hash
        // included). Mirrors the `OPENCODE_GO_MAX_TOKENS_CAP`
        // wiring on the opencode_go chat-completions path.
        self.0.effective_max_tokens(req)
    }

    /// Delegate to the wrapped `OpenAiCompatProvider` so the probe
    /// ceiling matches the per-provider hard cap wired at
    /// construction time. With the default wiring (this provider
    /// built via [`Self::new`]) the inner `kind_hard_cap` is
    /// `Some(DEEPSEEK_MAX_TOKENS_CAP)`, so the probe short-circuits
    /// at `2^19 = 524_288` instead of probing values DeepSeek
    /// rejects with HTTP 400. Mirrors the `OPENCODE_GO_MAX_TOKENS_CAP`
    /// delegation on `OpenCodeGoProvider`.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        self.0.max_tokens_probe_ceiling()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            kind: "deepseek".into(),
            endpoint: "https://api.deepseek.com/v1".into(),
            model: "deepseek-v4-flash".into(),
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
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY");
        }
        let result = DeepSeekProvider::from_config(&config());
        assert!(matches!(result, Err(Error::InvalidApiKey(_))));
    }

    #[test]
    fn provider_name_is_deepseek() {
        let provider = DeepSeekProvider::new(&config(), SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.name(), "deepseek");
        assert_eq!(provider.model(), "deepseek-v4-flash");
    }

    /// PR-473 regression pin: `DeepSeekProvider::new` must wire
    /// `DEEPSEEK_MAX_TOKENS_CAP` as the per-provider
    /// `kind_hard_cap` so the wire body carries `max_tokens =
    /// 393_216` even when the operator's TOML leaves
    /// `max_tokens = None` (per-role default 1_000_000). Without
    /// this wiring the upstream returns HTTP 400
    /// `invalid_request_error` (`{"message":"Invalid max_tokens
    /// value, the valid range of max_tokens is [1, 393216]"}`)
    /// — exactly the failure the --ignored CI job surfaced on a
    /// fresh runner with no cached probe result.
    #[test]
    fn new_wires_deepseek_max_tokens_cap() {
        // Use a config with `max_tokens = None` so the operator
        // cap is absent and the kind-level cap (DEEPSEEK_MAX_TOKENS_CAP)
        // becomes the active ceiling. The default `config()`
        // helper sets `max_tokens = Some(8192)` (the historical
        // DeepSeek-direct cap from pre-PR-473); we want to pin
        // the new wiring in isolation, so build a separate spec.
        let spec = ProviderConfig {
            kind: "deepseek".into(),
            endpoint: "https://api.deepseek.com/v1".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_save: true,
            plan: None,
        };
        let provider = DeepSeekProvider::new(&spec, SecretString::new("dummy".into())).unwrap();
        assert_eq!(
            provider.max_tokens_probe_ceiling(),
            DEEPSEEK_MAX_TOKENS_CAP,
            "DeepSeekProvider must surface DEEPSEEK_MAX_TOKENS_CAP as the probe ceiling"
        );
        // effective_max_tokens clamps the wire body to the cap
        // when the request asks for more than the upstream
        // accepts. The audit hash stays in sync with the wire
        // body because both call the same clamp chain.
        let req = Request {
            role: crate::llm::Role::Route,
            model: "deepseek-v4-flash".into(),
            system: String::new(),
            user: String::new(),
            max_tokens: 1_000_000,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        assert_eq!(
            provider.effective_max_tokens(&req),
            DEEPSEEK_MAX_TOKENS_CAP,
            "effective_max_tokens must clamp 1_000_000 → DEEPSEEK_MAX_TOKENS_CAP"
        );
    }
}
