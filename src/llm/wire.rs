//! LLM wire types — provider-agnostic. Provider implementations translate
//! these to/from their own JSON shapes.

use serde::{Deserialize, Serialize};

use super::role::Role;

/// Provider-agnostic request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Which role this call plays in the pipeline.
    pub role: Role,
    /// Model identifier (e.g. `"MiniMax-M3"`).
    pub model: String,
    /// System prompt. Stable across calls of the same role.
    pub system: String,
    /// User prompt. The actual content the model reacts to.
    pub user: String,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature (e.g. 0.6). `None` lets the provider choose.
    pub temperature: Option<f32>,
    /// Nucleus sampling top-p (e.g. 0.95). `None` lets the provider choose.
    pub top_p: Option<f32>,
    /// Optional JSON schema for structured output. Providers that
    /// support native JSON mode honour it; others only get a textual
    /// hint in the prompt.
    pub response_schema: Option<serde_json::Value>,
}

/// Provider-agnostic response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// The model output text.
    pub text: String,
    /// Stop reason reported by the provider (`"end_turn"`, `"max_tokens"`, etc.).
    pub finish_reason: Option<String>,
    /// Convenience flag: `true` when the response was cut at
    /// `max_tokens`. Provider implementations set this based on
    /// `finish_reason`, so the rest of the pipeline can branch on
    /// it without re-parsing the finish string.
    #[serde(default)]
    pub truncated: bool,
    /// Token usage.
    pub usage: Usage,
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
        self.input_tokens + self.output_tokens
    }
}

/// What happened during an LLM call. Used by the cache layer.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_total_is_input_plus_output() {
        let u = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read: 0,
            cache_creation: 0,
        };
        assert_eq!(u.total(), 150);
    }

    #[test]
    fn request_serializes_clean() {
        let r = Request {
            role: Role::Intake,
            model: "MiniMax-M3".into(),
            system: "system".into(),
            user: "user".into(),
            max_tokens: 1024,
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&j).unwrap();
        assert_eq!(back.role, Role::Intake);
        assert_eq!(back.max_tokens, 1024);
    }
}
