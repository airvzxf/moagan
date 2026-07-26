//! Wire-format trait. Lets us share request/response shapes between
//! providers that share a protocol (e.g. Anthropic-compatible, OpenAI).
//!
//! Compliance: 10-integrada-v0 §D.1.2 (WireFormat).

use async_trait::async_trait;

use crate::error::Result;

use super::wire::{Request, Response};

/// A wire format translates a moagan [`Request`] into the JSON shape
/// the provider expects, and parses the raw response back.
#[async_trait]
pub trait WireFormat: Send + Sync {
    /// Stable name (e.g. `"anthropic"`, `"openai"`, `"custom"`).
    fn name(&self) -> &str;

    /// Serialize the request body for the provider's HTTP endpoint.
    fn encode_body(&self, req: &Request) -> Result<Vec<u8>>;

    /// Parse the raw response body into a moagan [`Response`].
    fn decode(&self, body: &[u8]) -> Result<Response>;
}

/// Anthropic-compatible wire format. Compatible with the `minimax`
/// endpoint at `/v1/messages`.
pub struct AnthropicWire;

#[async_trait]
impl WireFormat for AnthropicWire {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn encode_body(&self, req: &Request) -> Result<Vec<u8>> {
        let body = super::http::body_from_request(req);
        serde_json::to_vec(&body).map_err(|e| crate::error::Error::Provider(format!("encode: {e}")))
    }

    fn decode(&self, body: &[u8]) -> Result<Response> {
        let parsed: super::http::MessagesResponseBody = serde_json::from_slice(body)
            .map_err(|e| crate::error::Error::Provider(format!("decode: {e}")))?;
        parsed
            .into_response()
            .map_err(|e| crate::error::Error::Provider(e.to_string()))
    }
}

/// OpenAI-compatible wire format. Bare-bones, suitable for any
/// provider that mirrors the OpenAI `/chat/completions` shape.
pub struct OpenAiWire;

#[async_trait]
impl WireFormat for OpenAiWire {
    fn name(&self) -> &str {
        "openai"
    }

    fn encode_body(&self, _req: &Request) -> Result<Vec<u8>> {
        Err(crate::error::Error::Provider(
            "openai wire format not implemented in MVP v0.1".into(),
        ))
    }

    fn decode(&self, _body: &[u8]) -> Result<Response> {
        Err(crate::error::Error::Provider(
            "openai wire format not implemented in MVP v0.1".into(),
        ))
    }
}

/// Custom wire format — user-supplied. Holder for an opaque handler.
pub struct CustomWire {
    /// Name of the custom format.
    pub id: String,
}

#[async_trait]
impl WireFormat for CustomWire {
    fn name(&self) -> &str {
        &self.id
    }

    fn encode_body(&self, _req: &Request) -> Result<Vec<u8>> {
        Err(crate::error::Error::InvalidArgs(format!(
            "custom wire '{}' not configured",
            self.id
        )))
    }

    fn decode(&self, _body: &[u8]) -> Result<Response> {
        Err(crate::error::Error::InvalidArgs(format!(
            "custom wire '{}' not configured",
            self.id
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::role::Role;

    #[tokio::test]
    async fn anthropic_wire_roundtrips() {
        let req = Request {
            role: Role::Intake,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
        };
        let wire = AnthropicWire;
        let body = wire.encode_body(&req).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "m");
        assert_eq!(json["system"], "s");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "u");
    }

    #[test]
    fn openai_wire_returns_not_implemented() {
        let req = Request {
            role: Role::Intake,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
        };
        let wire = OpenAiWire;
        assert!(wire.encode_body(&req).is_err());
    }
}
