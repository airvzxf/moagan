//! `opencode_go` provider — OpenCode Go's OpenAI-compat API at
//! `https://opencode.ai/zen/go/v1`.
//!
//! Operator policy (documented, not enforced by code): this
//! subscription is intended for non-MiniMax and non-Direct-DeepSeek
//! models. The minimax-* family is blocked because the operator
//! prefers direct MiniMax access; the deepseek-* family is a
//! fallback when the direct DeepSeek credits run out.
//!
//! Allowed models (24 total per GET /v1/models):
//! - kimi-k3, kimi-k2.7-code, kimi-k2.6, kimi-k2.5
//! - glm-5.2, glm-5.1, glm-5
//! - qwen3.7-max, qwen3.8-max, qwen3.7-plus, qwen3.6-plus, qwen3.5-plus
//! - mimo-v2-pro, mimo-v2-omni, mimo-v2.5-pro, mimo-v2.5
//! - hy3, hy3-preview
//! - gpt-5.6-luna
//! - grok-4.5
//! - deepseek-v4-pro, deepseek-v4-flash (fallback when direct DeepSeek runs out)

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::openai_compat::OpenAiCompatProvider;
use super::provider::Provider;
use super::wire::{Request, Response};

/// OpenCode Go provider backed by the generic OpenAI-compat implementation.
#[derive(Debug, Clone)]
pub struct OpenCodeGoProvider(OpenAiCompatProvider);

impl OpenCodeGoProvider {
    /// Build from an OpenCode Go provider config and a resolved API key.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "opencode_go" {
            return Err(Error::InvalidArgs(format!(
                "opencode_go provider got kind '{}'",
                spec.kind
            )));
        }
        Ok(Self(OpenAiCompatProvider::new(spec, api_key)?))
    }

    /// Build from config using `OPENCODE_GO_API_KEY`.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = std::env::var("OPENCODE_GO_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "OPENCODE_GO_API_KEY not set; provide via env, --api-key, or config file"
                        .into(),
                )
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// Hard-blocked model names. OpenCode Go offers 24 models but
    /// the operator has a policy: never use the minimax-* family via
    /// this subscription (prefer direct MiniMax).
    pub const BLOCKED_MODELS: &'static [&'static str] =
        &["minimax-m3", "minimax-m2.7", "minimax-m2.5"];

    /// True when the given model name is in the blocked list.
    pub fn is_blocked(model: &str) -> bool {
        Self::BLOCKED_MODELS.contains(&model)
    }
}

impl std::ops::Deref for OpenCodeGoProvider {
    type Target = OpenAiCompatProvider;

    fn deref(&self) -> &OpenAiCompatProvider {
        &self.0
    }
}

#[async_trait]
impl Provider for OpenCodeGoProvider {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn model(&self) -> &str {
        self.0.model()
    }

    fn endpoint(&self) -> &str {
        self.0.endpoint()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        self.0.send(req).await
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
        assert!(!OpenCodeGoProvider::is_blocked("grok-4.5"));
        assert!(!OpenCodeGoProvider::is_blocked("glm-5.2"));
        assert!(!OpenCodeGoProvider::is_blocked("qwen3.7-max"));
        assert!(!OpenCodeGoProvider::is_blocked("mimo-v2-pro"));
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
    fn provider_name_is_opencode_go() {
        let provider =
            OpenCodeGoProvider::new(&config(), SecretString::new("dummy".into())).unwrap();
        assert_eq!(provider.name(), "opencode_go");
        assert_eq!(provider.model(), "kimi-k2.7-code");
    }
}
