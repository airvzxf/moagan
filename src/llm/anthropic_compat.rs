//! D.19.13: AnthropicCompatProvider — generic Anthropic-compatible
//! provider driven by config (base URL + api key + model).
//!
//! The existing `opencode_go_anthropic` is hard-coded to the
//! opencode-go gateway. This module provides a config-driven
//! counterpart so operators can point to any Anthropic-compatible
//! endpoint.

use crate::error::{Error, Result};
use crate::llm::provider::Provider;
use crate::llm::role::Role;
use crate::llm::wire::{Request, Response};

pub struct AnthropicCompatProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl AnthropicCompatProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self { base_url, api_key, model }
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicCompatProvider {
    fn name(&self) -> &str { "anthropic_compat" }
    fn model(&self) -> &str { &self.model }
    fn endpoint(&self) -> &str { &self.base_url }
    async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
        Err(Error::InvalidArgs(
            "AnthropicCompatProvider.send is stub-only; the opencode_go_anthropic provider is the canonical path".into()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_request() -> Request {
        Request {
            role: Role::Sketch,
            model: "claude-3-5-sonnet".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: 1024,
            temperature: Some(0.7),
            top_p: Some(0.95),
            response_schema: None,
        }
    }

    #[test]
    fn anthropic_compat_provider_name() {
        let p = AnthropicCompatProvider::new(
            "https://api.example.com".into(),
            "sk-test".into(),
            "claude-3-5-sonnet".into(),
        );
        assert_eq!(p.name(), "anthropic_compat");
        assert_eq!(p.model(), "claude-3-5-sonnet");
        assert_eq!(p.endpoint(), "https://api.example.com");
    }

    #[test]
    fn anthropic_compat_provider_send_returns_invalid_args() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let p = AnthropicCompatProvider::new(
                "https://api.example.com".into(),
                "sk-test".into(),
                "claude-3-5-sonnet".into(),
            );
            let result = p.send(&fake_request()).await;
            assert!(result.is_err());
            assert!(matches!(result.unwrap_err(), Error::InvalidArgs(_)));
        });
    }
}
