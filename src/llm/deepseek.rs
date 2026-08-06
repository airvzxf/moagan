//! `deepseek` provider — DeepSeek's OpenAI-compat API at
//! `https://api.deepseek.com/v1/chat/completions`.
//!
//! This is a thin wrapper around `OpenAiCompatProvider` that pre-fills
//! the DeepSeek-specific defaults (endpoint, model, API key env).

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::ProviderCapabilities;
use super::openai_compat::OpenAiCompatProvider;
use super::provider::Provider;
use super::wire::{Request, Response};

/// DeepSeek provider backed by the generic OpenAI-compat implementation.
#[derive(Debug, Clone)]
pub struct DeepSeekProvider(OpenAiCompatProvider);

impl DeepSeekProvider {
    /// Build from a DeepSeek provider config and a resolved API key.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "deepseek" {
            return Err(Error::InvalidArgs(format!(
                "deepseek provider got kind '{}'",
                spec.kind
            )));
        }
        Ok(Self(OpenAiCompatProvider::new(spec, api_key)?))
    }

    /// Build from config using `DEEPSEEK_API_KEY`.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "DEEPSEEK_API_KEY not set; provide via env, --api-key, or config file".into(),
                )
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
}
