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
    /// Maximum tokens to generate. `None` lets the provider / upstream
    /// default apply — the wire builder omits the `max_tokens` field
    /// entirely (via `skip_serializing_if = "Option::is_none"`).
    /// `Some(n)` sends `n` verbatim, after the per-provider clamp
    /// chain. The auto-healing `param_rejections` table sets this
    /// to `None` when the cache says the upstream rejects the
    /// field; the omit step then retries with the field absent so
    /// the upstream accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
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
    /// Extra messages appended after the user message — used by the
    /// `PromptPrefill` JSON recovery strategy to inject an
    /// assistant prefill of `{` so the model continues with valid
    /// JSON. Empty for every strategy other than
    /// `PromptPrefill`; ignored by providers that do not honour
    /// prefill (the Anthropic-compatible wire, the Responses API).
    ///
    /// The cross-run cache key
    /// ([`crate::llm::wire::build_cache_key`]) deliberately
    /// IGNORES this field — the prefill is a response-side hint
    /// that does not change the request identity. A cached
    /// non-prefill response stays valid for the non-prefill call;
    /// the prefill call goes through the cache-bypass path in
    /// [`crate::phases::phase::RunContext::call_uncached`] so a
    /// prefill-induced response never poisons the steady-state
    /// cache.
    #[serde(default)]
    pub extra_messages: Vec<Message>,
    /// File attachments carried with the request (images,
    /// PDFs, audio clips, etc.). Each entry's [`Attachment::modality`]
    /// must match a modality the target model accepts; the
    /// [`crate::llm::modal_gate::ModalityGate`] enforces that
    /// contract before the request reaches the wire builder.
    ///
    /// Default-empty so call sites that do not need attachments
    /// keep their existing literal. `skip_serializing_if =
    /// "Vec::is_empty"` keeps the wire body byte-identical to
    /// pre-PR-5 requests when no attachment is present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Tool / function-call selection. The wire builder
    /// (`openai_compat`, `opencode_go_*`)
    /// translates this into the per-provider field name
    /// (`tool_choice`, `tools`, `functions`).
    ///
    /// `None` means "the model is being called without a tool
    /// selector"; the wire builder omits the field entirely.
    /// PR-5: when the model's `tool_call` capability is `false`
    /// the [`crate::llm::modal_gate::ModalityGate`] drops this
    /// to `None` so a tool selector does not reach a model that
    /// cannot honour it. Default-`None` so call sites that do
    /// not need tools keep their existing literal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

/// One file attachment carried with a [`Request`].
///
/// Modality is a free-form string (e.g. `"text"`, `"image"`,
/// `"pdf"`, `"audio"`) to match the upstream `models.dev`
/// `modalities.input` vocabulary verbatim. The gate matches by
/// string equality, so the caller is responsible for picking
/// the same spelling the catalog uses (lowercase, singular).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    /// MIME type or short label (e.g. `"image/png"`).
    pub mime: String,
    /// Modality tag from the upstream catalog vocabulary
    /// (e.g. `"image"`, `"pdf"`). Compared verbatim against
    /// [`crate::llm::models_dev::Modalities::input`].
    pub modality: String,
    /// Body of the attachment. Wire builders that need a
    /// base64-encoded payload convert the bytes themselves
    /// before serialising the body, so this struct keeps the
    /// raw form the caller hands over.
    pub data: Vec<u8>,
}

/// Tool / function-call selection on a [`Request`]. The
/// provider-specific wire builder maps this into the
/// per-protocol field name (`tool_choice`, `tools`,
/// `functions`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    Auto,
    /// The model must call exactly one of the supplied tools.
    Required,
    /// The model must not call any tool.
    None,
}

/// A single chat message used by the `extra_messages` field on
/// [`Request`]. Mirrors the OpenAI Chat-Completions message
/// shape (`{"role": "...", "content": "..."}`) so the OpenAI-compat
/// body builder can serialise prefill entries without an extra
/// conversion layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Message role (`"system"`, `"user"`, `"assistant"`). The
    /// `PromptPrefill` strategy uses `"assistant"` exclusively;
    /// other strategies leave the field empty.
    pub role: String,
    /// Message content. The `PromptPrefill` strategy uses the
    /// single character `{` so the model continues with a JSON
    /// object body.
    pub content: String,
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
        let total = self.input_tokens + self.output_tokens;
        tracing::trace!(
            input = self.input_tokens,
            output = self.output_tokens,
            cache_read = self.cache_read,
            cache_creation = self.cache_creation,
            total,
            "Usage::total"
        );
        total
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
        tracing::trace!(from = ?algo, "CacheHashAlgo::from");
        match algo {
            crate::cli::flags_batch::HashAlgo::Sha256 => Self::Sha256,
            crate::cli::flags_batch::HashAlgo::Blake3 => Self::Blake3,
        }
    }
}

/// Drop an optional wire field from a `Request` so the retry call
/// does not re-emit a parameter the upstream already rejected.
///
/// `param` is matched against the optional fields the dispatch path
/// can mutate at runtime — `temperature`, `top_p`, and
/// `max_tokens` (the latter is `Option<u32>`, so dropping it makes
/// the wire body omit the field entirely). Unknown parameters are
/// ignored silently so the helper is safe to call from a generic
/// loop even when the rejection detector picks up a field the
/// runtime cannot omit.
pub fn omit_param(req: &mut Request, param: &str) {
    tracing::debug!(param, "omit_param: clearing optional wire field");
    match param {
        "temperature" => req.temperature = None,
        "top_p" => req.top_p = None,
        // `max_tokens` → `None` drops the field from the wire body
        // (via `skip_serializing_if`). The auto-healing
        // `param_rejections` table records the rejection so the
        // next run omits the field from the first call, closing
        // the loop for upstreams that reject the *presence* of
        // `max_tokens` (e.g. `gpt-5.6-luna`).
        "max_tokens" => req.max_tokens = None,
        // Unknown parameters are a no-op so the runtime can record
        // the rejection (so the next run learns) without breaking
        // the current call.
        _ => {
            tracing::trace!(param, "omit_param: unknown parameter, no-op");
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
    tracing::trace!(
        provider,
        model,
        role = ?req.role,
        algo = ?algo,
        "build_cache_key"
    );
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
        &req.max_tokens.map(|n| n.to_string()).unwrap_or_default(),
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
    use proptest::prelude::*;

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
            max_tokens: Some(1024),
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&j).unwrap();
        assert_eq!(back.role, Role::Intake);
        assert_eq!(back.max_tokens, Some(1024));
        assert!(!back.stream, "default stream flag roundtrips as false");
        assert!(
            back.extra_messages.is_empty(),
            "default extra_messages is empty when the field is absent on the wire"
        );
    }

    #[test]
    fn request_extra_messages_default_to_empty_when_field_absent() {
        // The `extra_messages` field is `#[serde(default)]` so a
        // serialised Request written before this field existed
        // (or by a hand-rolled test fixture) still round-trips
        // without an explicit empty vector.
        let json = serde_json::json!({
            "role": "intake",
            "model": "minimax-m3",
            "system": "sys",
            "user": "user",
            "max_tokens": 1024,
            "temperature": 0.6,
            "top_p": 0.95,
            "stream": false,
        });
        let r: Request = serde_json::from_value(json).unwrap();
        assert!(r.extra_messages.is_empty());
    }

    #[test]
    fn request_extra_messages_round_trip() {
        // A Request with one prefill message round-trips
        // through serde without losing the field. Pins the
        // wire shape so the body builder can rely on
        // `Request.extra_messages` being serialised verbatim.
        let r = Request {
            role: Role::Intake,
            model: "deepseek-v4-flash".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: Some(1024),
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![Message {
                role: "assistant".into(),
                content: "{".into(),
            }],
            attachments: vec![],
            tool_choice: None,
        };
        let j = serde_json::to_value(&r).unwrap();
        let arr = j.get("extra_messages").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["role"], "assistant");
        assert_eq!(arr[0]["content"], "{");
        let back: Request = serde_json::from_value(j).unwrap();
        assert_eq!(back.extra_messages, r.extra_messages);
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
            max_tokens: Some(64),
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
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

    /// `Request::extra_messages` is part of the wire shape but NOT
    /// part of the cache identity. Two requests that differ only
    /// in their `extra_messages` (e.g. a normal call vs the
    /// `PromptPrefill` retry) must produce the SAME cache key so
    /// the steady-state cache stays valid when the prefill retry
    /// fires.
    #[test]
    fn cache_key_ignores_extra_messages() {
        let base = Request {
            role: Role::Sketch,
            model: "MiniMax-M3".into(),
            system: "system".into(),
            user: "user".into(),
            max_tokens: Some(64),
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let with_prefill = Request {
            extra_messages: vec![Message {
                role: "assistant".into(),
                content: "{".into(),
            }],
            ..base.clone()
        };
        assert_eq!(
            build_cache_key(&base, "minimax", "MiniMax-M3", CacheHashAlgo::Blake3),
            build_cache_key(
                &with_prefill,
                "minimax",
                "MiniMax-M3",
                CacheHashAlgo::Blake3
            ),
            "extra_messages MUST NOT contribute to the cache key"
        );
        // SHA-256 path honours the same invariant.
        assert_eq!(
            build_cache_key(&base, "minimax", "MiniMax-M3", CacheHashAlgo::Sha256),
            build_cache_key(
                &with_prefill,
                "minimax",
                "MiniMax-M3",
                CacheHashAlgo::Sha256
            ),
            "extra_messages MUST NOT contribute to the SHA-256 cache key"
        );
    }

    #[test]
    fn cache_hash_algo_default_is_blake3() {
        // The cache module's `Cache::cache_key` already pins BLAKE3
        // via `canonical_hash`; this constant documents the same
        // choice here so the wire-layer helper doesn't drift.
        assert_eq!(CacheHashAlgo::default(), CacheHashAlgo::Blake3);
    }

    /// `Request::max_tokens = None` must round-trip as field-absent
    /// on the wire (the `skip_serializing_if` attribute pins this).
    /// The auto-healing `param_rejections` path relies on the
    /// field being absent on the retry so upstreams that reject
    /// the *presence* of `max_tokens` (e.g. `gpt-5.6-luna`) accept
    /// the request.
    #[test]
    fn request_omits_max_tokens_when_none() {
        let r = Request {
            role: Role::Intake,
            model: "m".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let j: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(
            j.get("max_tokens").is_none(),
            "max_tokens must be absent from the wire when None, got: {j}"
        );
        // Round-trip via deserialise honours `#[serde(default)]`:
        // a payload that omits the field deserialises as `None`.
        let back: Request = serde_json::from_value(j).unwrap();
        assert_eq!(back.max_tokens, None);
    }

    /// `Request::max_tokens = Some(n)` must round-trip with the
    /// numeric value preserved. Pins the byte-identity contract the
    /// pre-existing wire body builders rely on (`Some(n)` produces
    /// the same wire JSON that `u32 = n` did before this PR).
    #[test]
    fn request_includes_max_tokens_when_some() {
        let r = Request {
            role: Role::Intake,
            model: "m".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: Some(1024),
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let j: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(
            j.get("max_tokens"),
            Some(&serde_json::json!(1024)),
            "max_tokens must serialise as a numeric JSON value, got: {j}"
        );
        let back: Request = serde_json::from_value(j).unwrap();
        assert_eq!(back.max_tokens, Some(1024));
    }

    /// `omit_param` clears `max_tokens` to `None` so the wire body
    /// drops the field. Pins the auto-healing close-the-loop: the
    /// dispatch path can now omit a `max_tokens` rejection on the
    /// retry without operator intervention.
    #[test]
    fn omit_param_clears_max_tokens_to_none() {
        let mut req = Request {
            role: Role::Intake,
            model: "m".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: Some(1024),
            temperature: Some(0.6),
            top_p: Some(0.95),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        super::omit_param(&mut req, "max_tokens");
        assert_eq!(
            req.max_tokens, None,
            "omit_param must clear max_tokens to None"
        );
        // Other fields stay untouched.
        assert_eq!(req.temperature, Some(0.6));
        assert_eq!(req.top_p, Some(0.95));
    }

    // -----------------------------------------------------------------
    // Property-based tests (proptest 1.4, dev-only per ADR-0001).
    // These pin the invariants of `build_cache_key` for both
    // hash algorithms. The function must:
    // 1. produce a deterministic 64-char lowercase hex output,
    // 2. distinguish every identity field (system / user /
    //    max_tokens / temperature / top_p / provider / model),
    // 3. honour the existing `extra_messages` invariant (the
    //    prefill retry MUST NOT collide with the steady-state
    //    cache key), and
    // 4. produce different digests for the two algorithms even
    //    on the same input (they share the canonical join but
    //    the digests are different families).
    // -----------------------------------------------------------------

    /// Build a `Request` whose fields proptest controls. Same
    /// shape as the `req()` helper in `cache/mod.rs` but local
    /// to keep the property tests self-contained.
    fn req_with_all(
        system: &str,
        user: &str,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
        top_p: Option<f32>,
    ) -> Request {
        Request {
            role: Role::Intake,
            model: "m".into(),
            system: system.into(),
            user: user.into(),
            max_tokens,
            temperature,
            top_p,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    proptest::proptest! {
        /// Both algorithms are deterministic on the same
        /// request: re-hashing the same tuple returns the same
        /// key. This is what makes `build_cache_key` usable as
        /// a cache key in the first place.
        #[test]
        fn prop_build_cache_key_is_deterministic_blake3(
            system in ".*", user in ".*", max_tokens in proptest::option::of(1u32..4096),
            temperature in proptest::option::of(0.0f32..2.0),
            top_p in proptest::option::of(0.0f32..1.0),
        ) {
            let r = req_with_all(&system, &user, max_tokens, temperature, top_p);
            let k1 = build_cache_key(&r, "mock", "m", CacheHashAlgo::Blake3);
            let k2 = build_cache_key(&r, "mock", "m", CacheHashAlgo::Blake3);
            prop_assert_eq!(&k1, &k2);
            prop_assert_eq!(k1.len(), 64);
            prop_assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn prop_build_cache_key_is_deterministic_sha256(
            system in ".*", user in ".*", max_tokens in proptest::option::of(1u32..4096),
            temperature in proptest::option::of(0.0f32..2.0),
            top_p in proptest::option::of(0.0f32..1.0),
        ) {
            let r = req_with_all(&system, &user, max_tokens, temperature, top_p);
            let k1 = build_cache_key(&r, "mock", "m", CacheHashAlgo::Sha256);
            let k2 = build_cache_key(&r, "mock", "m", CacheHashAlgo::Sha256);
            prop_assert_eq!(&k1, &k2);
            prop_assert_eq!(k1.len(), 64);
            prop_assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
        }

        /// BLAKE3 and SHA-256 paths share the canonical join
        /// but use different digests. Property: the two keys
        /// are never equal for any non-empty user prompt (the
        /// chance of a coincidental 64-hex-char collision is
        /// astronomically small — 16^-64).
        #[test]
        fn prop_build_cache_key_algorithms_disagree(
            user in ".+",
        ) {
            let r = req_with_all("s", &user, Some(16), None, None);
            let blake = build_cache_key(&r, "mock", "m", CacheHashAlgo::Blake3);
            let sha = build_cache_key(&r, "mock", "m", CacheHashAlgo::Sha256);
            prop_assert_ne!(blake, sha);
        }

        /// The `user` field is part of the cache identity in
        /// both algorithms. Pins that the wire-layer helper
        /// honours the same discrimination contract as
        /// `Cache::cache_key`.
        #[test]
        fn prop_build_cache_key_distinguishes_user_blake3(
            user_a in ".+", user_b in ".+",
        ) {
            prop_assume!(user_a != user_b);
            let ra = req_with_all("s", &user_a, Some(16), None, None);
            let rb = req_with_all("s", &user_b, Some(16), None, None);
            prop_assert_ne!(
                build_cache_key(&ra, "mock", "m", CacheHashAlgo::Blake3),
                build_cache_key(&rb, "mock", "m", CacheHashAlgo::Blake3),
            );
        }

        #[test]
        fn prop_build_cache_key_distinguishes_user_sha256(
            user_a in ".+", user_b in ".+",
        ) {
            prop_assume!(user_a != user_b);
            let ra = req_with_all("s", &user_a, Some(16), None, None);
            let rb = req_with_all("s", &user_b, Some(16), None, None);
            prop_assert_ne!(
                build_cache_key(&ra, "mock", "m", CacheHashAlgo::Sha256),
                build_cache_key(&rb, "mock", "m", CacheHashAlgo::Sha256),
            );
        }

        /// `extra_messages` is a wire-only field and MUST NOT
        /// contribute to the cache key. The `PromptPrefill`
        /// retry path adds one assistant prefill entry; if that
        /// flipped the key, the steady-state cache would be
        /// invalidated every time the prefill retry fired.
        #[test]
        fn prop_build_cache_key_ignores_extra_messages_blake3(
            user in ".*", max_tokens in proptest::option::of(1u32..4096),
        ) {
            let base = req_with_all("s", &user, max_tokens, None, None);
            let with_prefill = Request {
                extra_messages: vec![Message {
                    role: "assistant".into(),
                    content: "{".into(),
                }],
                ..base.clone()
            };
            prop_assert_eq!(
                build_cache_key(&base, "mock", "m", CacheHashAlgo::Blake3),
                build_cache_key(&with_prefill, "mock", "m", CacheHashAlgo::Blake3),
                "extra_messages MUST NOT contribute to the BLAKE3 cache key"
            );
        }

        #[test]
        fn prop_build_cache_key_ignores_extra_messages_sha256(
            user in ".*", max_tokens in proptest::option::of(1u32..4096),
        ) {
            let base = req_with_all("s", &user, max_tokens, None, None);
            let with_prefill = Request {
                extra_messages: vec![Message {
                    role: "assistant".into(),
                    content: "{".into(),
                }],
                ..base.clone()
            };
            prop_assert_eq!(
                build_cache_key(&base, "mock", "m", CacheHashAlgo::Sha256),
                build_cache_key(&with_prefill, "mock", "m", CacheHashAlgo::Sha256),
                "extra_messages MUST NOT contribute to the SHA-256 cache key"
            );
        }
    }
}
