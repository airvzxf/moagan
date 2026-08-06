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
    /// Whether the provider should stream tokens as they arrive.
    /// Defaults to `false`. Only providers whose capability matrix
    /// advertises streaming honour this flag.
    #[serde(default)]
    pub stream: bool,
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

/// Hash algorithm selector for [`build_cache_key`]. Mirrors
/// `crate::cli::flags_batch::HashAlgo` (the canonical CLI
/// type) so the dispatcher can pass through the user's
/// `--hash-algo` choice without an extra conversion layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheHashAlgo {
    /// SHA-256 (audit-friendly; human-readable with the usual
    /// CLI tooling).
    Sha256,
    /// BLAKE3 (the day-to-day internal hash; ~5–10x faster on
    /// hot paths than SHA-256).
    #[default]
    Blake3,
}

impl From<crate::cli::flags_batch::HashAlgo> for CacheHashAlgo {
    fn from(algo: crate::cli::flags_batch::HashAlgo) -> Self {
        match algo {
            crate::cli::flags_batch::HashAlgo::Sha256 => Self::Sha256,
            crate::cli::flags_batch::HashAlgo::Blake3 => Self::Blake3,
        }
    }
}

/// Build a cache key for `req` using the requested hash
/// algorithm. The canonical input set is `(role, provider,
/// model, system, user, max_tokens, temperature, top_p,
/// prompt_set_hash)` — the same tuple that
/// [`crate::llm::cache::Cache::cache_key`] hashes with BLAKE3.
/// This helper exists so callers that want to honour a
/// `--hash-algo` flag (or otherwise pick the algorithm) can do
/// so without duplicating the input shape.
///
/// BLAKE3 is the default (`CacheHashAlgo::default()`); pass
/// [`CacheHashAlgo::Sha256`] to opt into the SHA-256 key for
/// audit-friendly export. The two algorithms produce
/// structurally different keys for the same input (different
/// digest families), so this function's output must not be
/// mixed with [`crate::llm::cache::Cache::cache_key`].
pub fn build_cache_key(req: &Request, provider: &str, model: &str, algo: CacheHashAlgo) -> String {
    use crate::ids::{canonical_hash, sha256_hex};
    use crate::llm::prompts::prompt_set_hash;
    let prompt_set_hash = prompt_set_hash();
    let parts = [
        "role",
        req.role.as_str(),
        "provider",
        provider,
        "model",
        model,
        "system",
        &req.system,
        "user",
        &req.user,
        "max_tokens",
        &req.max_tokens.to_string(),
        "temperature",
        &req.temperature.map(|t| t.to_string()).unwrap_or_default(),
        "top_p",
        &req.top_p.map(|t| t.to_string()).unwrap_or_default(),
        "prompt_set_hash",
        &prompt_set_hash,
    ];
    match algo {
        CacheHashAlgo::Blake3 => canonical_hash(&parts),
        CacheHashAlgo::Sha256 => {
            // Re-derive the canonical join manually so the
            // BLAKE3 and SHA-256 paths produce the same input
            // byte sequence. canonical_hash already joins the
            // parts; sha256 over the same join is just another
            // digest on the joined bytes.
            let mut buf = Vec::new();
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    buf.push(0x1f);
                }
                buf.extend_from_slice(p.as_bytes());
            }
            sha256_hex(&buf)
        }
    }
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
            stream: false,
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&j).unwrap();
        assert_eq!(back.role, Role::Intake);
        assert_eq!(back.max_tokens, 1024);
        assert!(!back.stream, "default stream flag roundtrips as false");
    }

    #[test]
    fn wire_cache_key_blake3_differs_from_sha256() {
        // The two algorithms must produce different keys for the
        // same input. They share the canonical join, but the
        // digests are different families (BLAKE3 → 32 bytes, SHA-256
        // → 32 bytes; the bit patterns collide with negligible
        // probability).
        let r = Request {
            role: Role::Sketch,
            model: "MiniMax-M3".into(),
            system: "system".into(),
            user: "user".into(),
            max_tokens: 64,
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
        };
        let blake = build_cache_key(&r, "minimax", "MiniMax-M3", CacheHashAlgo::Blake3);
        let sha = build_cache_key(&r, "minimax", "MiniMax-M3", CacheHashAlgo::Sha256);
        assert_ne!(blake, sha);
        // Sanity: both keys are 64 lowercase hex chars.
        assert_eq!(blake.len(), 64);
        assert_eq!(sha.len(), 64);
        assert!(blake.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        // Determinism: same input → same key.
        let blake_again = build_cache_key(&r, "minimax", "MiniMax-M3", CacheHashAlgo::Blake3);
        assert_eq!(blake, blake_again);
    }

    #[test]
    fn cache_hash_algo_default_is_blake3() {
        // The cache module's `Cache::cache_key` already pins BLAKE3
        // via `canonical_hash`; this constant documents the same
        // choice here so the wire-layer helper doesn't drift.
        assert_eq!(CacheHashAlgo::default(), CacheHashAlgo::Blake3);
    }
}
