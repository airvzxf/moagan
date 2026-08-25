//! `deepseek` provider — DeepSeek's OpenAI-compat API at
//! `https://api.deepseek.com/v1/chat/completions`.
//!
//! This is a thin wrapper around `OpenAICompatibleProvider` that pre-fills
//! the DeepSeek-specific defaults (endpoint, model, API key env).

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::{DEEPSEEK_MAX_TOKENS_CAP, ProviderCapabilities};
use super::openai_compatible::OpenAICompatibleProvider;
use super::provider::Provider;
use super::wire::{Request, Response};

/// DeepSeek provider backed by the generic OpenAI-compat implementation.
#[derive(Debug, Clone)]
pub struct DeepSeekProvider(OpenAICompatibleProvider);

impl DeepSeekProvider {
    /// Build from a DeepSeek provider config and a resolved API key.
    /// Kept for backwards compatibility with hand-rolled callers
    /// (legacy test fixtures); new dispatcher code goes through
    /// [`Self::from_resolved`].
    ///
    /// When `spec.models` is empty (the v0.9 fixture shape) the
    /// section-level `endpoint` is reused for a synthetic
    /// `ModelConfig` so the rest of the constructor (URL builder,
    /// clamp chain, `max_tokens_table` lookup by `(name, model)`)
    /// sees the same shape the v0.10 dispatcher passes in.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if let Some(ep) = spec.endpoint.as_deref()
            && !ep.contains("deepseek")
        {
            return Err(Error::InvalidArgs(format!(
                "deepseek provider requires an endpoint containing 'deepseek', got {ep:?}"
            )));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|e| Error::Provider {
                message: format!("build http client: {e}"),
                http_status: None,
            })?;
        let first = spec
            .models
            .first()
            .cloned()
            .unwrap_or_else(|| crate::config::ModelConfig {
                id: "deepseek-v4-flash".to_owned(),
                endpoint: spec.endpoint.clone(),
                max_tokens: None,
            });
        let name = "deepseek".to_owned();
        Ok(Self(OpenAICompatibleProvider {
            name: name.clone(),
            model: first.id.clone(),
            endpoint: first
                .endpoint
                .clone()
                .unwrap_or_else(|| "https://api.deepseek.com/v1/chat/completions".to_owned()),
            api_key,
            client,
            max_retries: 3,
            provider_max_tokens: first.max_tokens,
            kind_hard_cap: Some(DEEPSEEK_MAX_TOKENS_CAP),
            max_tokens_table: None,
        }))
    }

    /// Build from config, resolving the API key via the unified
    /// helper. Kept for backwards compatibility; new dispatcher
    /// code goes through [`Self::from_resolved`].
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = super::api_keys::lookup_key("deepseek", None)
            .ok_or_else(|| Error::InvalidApiKey {
                message: "DEEPSEEK_API_KEY not set; provide via env, --api-key, or api_keys.toml"
                    .into(),
                http_status: None,
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey { message, .. } => Error::InvalidApiKey {
                    message: format!(
                        "deepseek: {message}; check api_keys.toml and the env var fallback"
                    ),
                    http_status: None,
                },
                other => other,
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// v0.10 dispatcher entry point. Builds a `DeepSeekProvider` from
    /// a `ResolvedModelConfig`, wrapping an `OpenAICompatibleProvider`
    /// with `DEEPSEEK_MAX_TOKENS_CAP` as the `kind_hard_cap`. The
    /// key lookup falls back from the section name to the canonical
    /// `kind` so a per-model alias like `deepseek-v4-flash`
    /// (kind=`"deepseek"`) resolves against `DEEPSEEK_API_KEY`
    /// rather than the non-existent `DEEPSEEK-V4-FLASH_API_KEY`.
    /// The dispatcher routes DeepSeek's `/v1/chat/completions`
    /// URL to this constructor directly so the cap is wired at
    /// construction time.
    pub fn from_resolved(resolved: &crate::config::ResolvedModelConfig) -> Result<Self> {
        let kind = super::api_keys::lookup_kind_for_resolved(resolved);
        let key = super::api_keys::lookup_key(&kind, None)
            .ok_or_else(|| Error::InvalidApiKey {
                message: format!(
                    "{}_API_KEY not set; provide via env, --api-key, or api_keys.toml",
                    kind.to_ascii_uppercase()
                ),
                http_status: None,
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey { message, .. } => Error::InvalidApiKey {
                    message: format!(
                        "{}: {message}; check api_keys.toml and the env var fallback",
                        kind
                    ),
                    http_status: None,
                },
                other => other,
            })?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|e| Error::Provider {
                message: format!("build http client: {e}"),
                http_status: None,
            })?;
        Ok(Self(OpenAICompatibleProvider {
            name: resolved.section.clone(),
            model: resolved.id.clone(),
            endpoint: resolved.endpoint.clone(),
            api_key: SecretString::new(key),
            client,
            max_retries: 3,
            provider_max_tokens: resolved.max_tokens,
            kind_hard_cap: Some(DEEPSEEK_MAX_TOKENS_CAP),
            max_tokens_table: None,
        }))
    }
}

impl std::ops::Deref for DeepSeekProvider {
    type Target = OpenAICompatibleProvider;

    fn deref(&self) -> &OpenAICompatibleProvider {
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
        // included). Mirrors the `u32::MAX`
        // wiring on the opencode_go chat-completions path.
        self.0.effective_max_tokens(req)
    }

    /// Delegate to the wrapped `OpenAICompatibleProvider` so the probe
    /// ceiling matches the per-provider hard cap wired at
    /// construction time. With the default wiring (this provider
    /// built via [`Self::new`]) the inner `kind_hard_cap` is
    /// `Some(DEEPSEEK_MAX_TOKENS_CAP)`, so the probe short-circuits
    /// at `2^19 = 524_288` instead of probing values DeepSeek
    /// rejects with HTTP 400. Mirrors the `u32::MAX`
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
            endpoint: None,
            models: Vec::new(),
            temperature: Some(0.6),
            top_p: Some(0.95),
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
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
        assert!(matches!(result, Err(Error::InvalidApiKey { .. })));
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
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
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
            model: "deepseek-v4-flash".into(),
            role: crate::llm::Role::Route,
            system: String::new(),
            user: String::new(),
            max_tokens: Some(1_000_000),
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
