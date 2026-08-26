//! Auto-detection of `max_tokens` per (provider, model) via runtime probes.
//!
//! Hardcoded caps (e.g. `MINIMAX_MAX_TOKENS_CAP = 524_288`,
//! `u32::MAX = 16_384`) are brittle: third-party relays
//! can lower the upstream's documented ceiling without warning, and a
//! downstream provider that no longer matches the operator's documented
//! cap becomes a regression that requires another code patch.
//!
//! `probe` removes that dependency on the operator's mental map of
//! per-model ceilings. The runtime flow is:
//!
//! 1. **Phase 1 — exponential search**: fire 30 sequential probes at
//!    `2^1..2^30` (each step doubles the candidate). The first failure
//!    breaks the loop; `lo` is the last confirmed OK, `hi` is the
//!    first failure. Worst case (the provider accepts every value up
//!    to `2^30`) hits the [`MAX_AUTOPROBE_CEILING`] constant and the
//!    discovered value lands at exactly that ceiling.
//!
//! 2. **Phase 2 — tightening**: bisect `[lo + 1, hi - 1]` in 20-point
//!    parallel batches. Iterate up to 32 rounds. The 20-point fan-out
//!    keeps wall-clock proportional to `O(log n)` rather than
//!    `O(n)` (which would be `2^20` requests in the worst case).
//!
//! 3. **Floor + clamp**: `discovered.max(floor).clamp(MIN, MAX)` so the
//!    caller-supplied floor (the `Option<u32>` from
//!    `ProviderConfig::max_token_auto`) cannot shrink the discovered
//!    value and the safety ceiling cannot be exceeded.
//!
//! The probe is **not** a regular LLM call: it runs over a dedicated
//! `reqwest::Client` with a 5-second timeout, never writes to
//! `calls.jsonl`, never opens the circuit breaker, and never counts
//! against `provider_usage`. The caller can therefore probe every
//! provider-model pair on every startup without skewing telemetry.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::error::{Error, Result};
use crate::llm::provider::Provider;
use crate::llm::role::Role;
use crate::llm::wire::Request;

/// Post-process the body emitted by `toml::to_string_pretty` so every
/// key under the `[providers.<name>.<model>]` headers is
/// double-quoted. The `toml` crate only quotes a key when its
/// contents contain a character that requires it (e.g. a `.` inside
/// `mimo-v2.5`); bare keys like `kimi-k3` come out unquoted, which
/// makes a TOML diff between providers with similar names hard to
/// read. We normalise the output here so every key under
/// `[providers.*.*]` matches the form `provider."<name>"."<model>"`
/// that the operator expects (mirrors the style in
/// `~/.config/moagan/config.toml`).
///
/// The regex is intentionally conservative: it only matches keys
/// consisting of `[A-Za-z0-9_-]+` (the bare-key character class in
/// TOML). Keys with special characters (which the `toml` crate
/// already quotes) are left untouched.
///
/// The helper is duplicated from [`crate::llm::temperature_probe`]
/// rather than moved to a shared module because the fix is a small
/// cosmetic normalisation specific to the sidecar format and
/// keeping it local to each writer makes the surrounding code
/// self-contained.
fn quote_provider_model_keys(body: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\[providers\.([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)\]")
            .expect("quote_provider_model_keys regex compiles")
    });
    re.replace_all(body, |caps: &regex::Captures<'_>| {
        format!("[providers.\"{}\".\"{}\"]", &caps[1], &caps[2])
    })
    .into_owned()
}

/// Maximum exponent used by the exponential probe. With `k <= 30` the
/// value `2^k` always fits in `u32`, so `1u32 << k` cannot overflow
/// and we do not need `checked_shl` on the hot path.
pub const MAX_PROBE_SHIFT: u32 = 30;

/// Hard ceiling for the discovered `max_tokens`. Computed as
/// `1u32 << MAX_PROBE_SHIFT` so the constant and the loop bound stay
/// in lockstep: bump one, the other follows.
///
/// `1u32 << 30 = 1_073_741_824` (≈ 1.07G tokens). No real provider
/// accepts more than this today; if one does, the discovered value
/// sits exactly at the ceiling, and `phase.rs` continues to use the
/// discovered value verbatim.
pub const MAX_AUTOPROBE_CEILING: u32 = 1u32 << MAX_PROBE_SHIFT;

/// Hard floor for the discovered `max_tokens`. Anything below this is
/// treated as "provider rejected everything" and the algorithm
/// surfaces an error rather than returning a degenerate value. 1024
/// is enough for the tiniest legitimate `propose` payload while
/// keeping the floor well above any accidental `1` or `2` value.
pub const MIN_AUTOPROBE_FLOOR: u32 = 1024;

/// HTTP timeout for a single probe. 15 s is enough for a healthy
/// upstream to answer the tiny `1`-token request even when the
/// model spends a few seconds on a thinking pass; anything longer
/// means the provider is in trouble and we should fall through to
/// the next probe rather than block the loop.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Probe request body. Tiny, deterministic, fits in any model
/// window. The model is asked to reply with the literal `1`; the
/// response text is discarded — only the HTTP status carries
/// signal.
pub const PROBE_USER: &str = "Reply with the single character: 1";

/// Empty system prompt for the probe. The model only needs the
/// user-side `Reply with the single character: 1` instruction.
pub const PROBE_SYSTEM: &str = "";

/// Trait that the probe uses to send its tiny request. `Provider`
/// already implements this shape via `send`, but the probe needs to
/// bypass the breaker / rate-limit / circuit-breaker layer so a
/// failing probe does not poison the steady-state pool. The trait
/// exists so the probe can be unit-tested with a fake that returns
/// canned statuses without standing up a wiremock server.
#[async_trait]
pub trait ProbeTransport: Send + Sync {
    /// Send a probe with the supplied `max_tokens` and report
    /// whether the upstream accepted it.
    async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome;

    /// Variant of [`Self::probe_send`] that returns the response body
    /// alongside the classified outcome. Default impl returns an
    /// empty body so test transports do not need to override; the
    /// production transport overrides this so Phase 0 can parse the
    /// upstream-reported cap from a `400` error body without
    /// re-issuing the request. The body is also discarded by
    /// Phase 1 / Phase 2 callers — only Phase 0 reads it.
    async fn probe_send_with_body(&self, max_tokens: u32) -> ProbeResult {
        let outcome = self.probe_send(max_tokens).await;
        ProbeResult {
            outcome,
            body: String::new(),
        }
    }
}

/// Combined outcome + body from a single probe call. Phase 0 reads
/// `body` to extract the upstream-reported cap; Phase 1 / Phase 2
/// ignore it.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// Classified outcome (`Accepted` / `Rejected` / `Indeterminate`).
    pub outcome: ProbeOutcome,
    /// Raw response body, when the transport captured one. Empty
    /// when the probe was classified as `Indeterminate` or when the
    /// transport did not bother to capture the body (test doubles).
    pub body: String,
}

/// Result of a single probe HTTP call. `Accepted` means the wire
/// body succeeded (status < 400 AND body does not carry the
/// `max_tokens` rejection signature). Everything else is treated as
/// a rejection so the algorithm can find the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Provider accepted `max_tokens` for the probe.
    Accepted,
    /// Provider rejected `max_tokens` with a 4xx carrying the
    /// max-tokens signature in the body.
    Rejected,
    /// Provider errored out for a reason other than max-tokens
    /// (network error, 5xx storm, malformed response). The
    /// algorithm treats this the same as `Rejected` so a flaky
    /// upstream cannot blow the loop.
    Indeterminate,
}

/// Default transport: wraps an existing `Provider` and fires a probe
/// against it. The probe deliberately bypasses the breaker (no
/// `BreakeredProvider` wrapping) so a 400 rejection does not count
/// against the circuit-breaker window.
pub struct ProviderProbeTransport {
    provider: Arc<dyn Provider>,
}

impl ProviderProbeTransport {
    /// Build a transport from a provider. The `client` field used
    /// to live here for an explicit per-probe timeout, but the
    /// transport reuses `provider.send` so the timeout is applied
    /// around the call inside [`Self::probe_send`].
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self> {
        Ok(Self { provider })
    }

    /// Borrow the underlying provider. Useful for tests that want
    /// to inspect call counts.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }
}

#[async_trait]
impl ProbeTransport for ProviderProbeTransport {
    async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
        self.probe_send_with_body(max_tokens).await.outcome
    }

    async fn probe_send_with_body(&self, max_tokens: u32) -> ProbeResult {
        let req = Request {
            role: Role::Sketch, // F1: see investigation report
            model: self.provider.model().to_owned(),
            system: PROBE_SYSTEM.to_owned(),
            user: PROBE_USER.to_owned(),
            // The probe always sets `Some(...)` so the wire body
            // carries the candidate value. The auto-healing
            // `param_rejections` path is the only code that sets
            // `None`, and it never fires during a probe.
            max_tokens: Some(max_tokens),
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let res = timeout(PROBE_TIMEOUT, self.provider.send_probe(&req)).await;
        match res {
            Ok(Ok((status, body))) => {
                // Classify:
                //   - 2xx / 3xx                       → Accepted
                //   - 4xx + body carries max_tokens   → Rejected (boundary)
                //   - 4xx + body does NOT carry it    → Indeterminate
                //                                       (e.g. 401/403 auth,
                //                                       model-not-found —
                //                                       not a max_tokens signal)
                //   - 5xx / network                  → Indeterminate
                let outcome = if (200..400).contains(&status) {
                    ProbeOutcome::Accepted
                } else if (400..500).contains(&status) {
                    if body_carries_max_tokens_rejection(&body.text) {
                        ProbeOutcome::Rejected
                    } else {
                        // C2: a generic 4xx (auth, model-not-found) is
                        // not a max-tokens boundary. Treating it as
                        // Rejected would collapse the discovered ceiling
                        // to the probe's exact value, which is wrong.
                        ProbeOutcome::Indeterminate
                    }
                } else {
                    ProbeOutcome::Indeterminate
                };
                ProbeResult {
                    outcome,
                    body: body.text,
                }
            }
            Ok(Err(err)) => {
                // The providers convert 4xx responses into
                // `Error::Provider { message, http_status }` before
                // returning. For Phase 0 we need the response body
                // (the upstream-reported cap), and the providers
                // embed it in `message` as
                // `"http 400 Bad Request: {body}"`. Recover the
                // body from the message so Phase 0 can parse the
                // cap without forcing a wire-format refactor on
                // every provider.
                let body = body_from_provider_error(&err);
                let outcome = if is_max_tokens_rejection_error(&err) {
                    ProbeOutcome::Rejected
                } else {
                    ProbeOutcome::Indeterminate
                };
                ProbeResult { outcome, body }
            }
            Err(_) => ProbeResult {
                outcome: ProbeOutcome::Indeterminate,
                body: String::new(),
            },
        }
    }
}

/// Lightweight classify-by-status helper used by the wire-mock tests
/// when the test transport does not have a body to inspect. Real
/// providers carry the rejection signature in the response body; this
/// helper accepts everything that came back with `status < 400` and
/// rejects the rest. The wire-mock integration test
/// (`tests/integration_max_tokens_auto.rs`) covers the body-bearing
/// path separately.
pub fn classify_status(status: u16, _body: &[u8]) -> ProbeOutcome {
    let out = if (200..400).contains(&status) {
        ProbeOutcome::Accepted
    } else {
        ProbeOutcome::Rejected
    };
    tracing::trace!(status, outcome = ?out, "probe::classify_status");
    out
}

/// Heuristic: does the response body carry the "max_tokens rejected"
/// signature? Real providers (Anthropic-compat, OpenAI-compat, OpenAI
/// Responses) all converge on the substring `max_tokens` somewhere in
/// the error body when the upstream rejects the request for that
/// reason. Other 400s (e.g. `model not found`) do not carry that
/// substring, so the heuristic cleanly separates the two cases.
pub fn body_carries_max_tokens_rejection(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let hit = lower.contains("max_tokens")
        || lower.contains("max tokens")
        || lower.contains("max_tokens_override")
        || lower.contains("tokens limit")
        || lower.contains("maximum context length");
    tracing::trace!(
        body_len = body.len(),
        hit,
        "probe::body_carries_max_tokens_rejection"
    );
    hit
}

/// Recover the response body from a `Provider::send_probe` error.
/// Providers convert 4xx responses into
/// `Error::Provider { message, http_status }` and embed the body
/// in `message`. Phase 0 needs the body to parse the upstream-
/// reported cap, so we recover it here without forcing a wire-
/// format refactor on every provider.
///
/// Two message shapes are recognised:
///
/// - `"http 400 Bad Request: {body}"` (the Anthropic-compat and
///   OpenCode Responses providers via the shared `classify_status`
///   helper in `super::http`).
/// - `"openai-compat: HTTP 400 after 1 attempts: {body}"` (the
///   OpenCode Go chat-completions wire in `openai_compatible.rs`,
///   where the path is `/v1/chat/completions`).
///
/// Both shapes share the same `: {body}` suffix; we strip everything
/// up to and including the first `": "` to recover the body. When
/// neither prefix matches we return an empty string and the caller
/// falls back to `Indeterminate`.
fn body_from_provider_error(err: &Error) -> String {
    let Error::Provider { message, .. } = err else {
        return String::new();
    };
    // Try the Anthropic-compat `http <status>: <body>` prefix
    // first; fall through to the OpenAI-compat `openai-compat:
    // HTTP <code> after <n> attempts: <body>` prefix; otherwise
    // treat the whole message as the body (the
    // `openai_compatible.rs` test fixture sometimes uses a
    // different shape during retries).
    if let Some(rest) = message.strip_prefix("http ")
        && let Some((_, body)) = rest.split_once(": ")
    {
        return body.to_owned();
    }
    if let Some(rest) = message.strip_prefix("openai-compat: ") {
        // The OpenAI-compat shape is `HTTP {code} after {n}
        // attempts: {body}`. Strip the `HTTP ... attempts:` prefix
        // to recover the body.
        if let Some(idx) = rest.find(": ") {
            return rest[idx + 2..].to_owned();
        }
    }
    String::new()
}

/// Decide whether an error returned by `Provider::send_probe` looks
/// like a max-tokens rejection. Used by
/// [`ProviderProbeTransport::probe_send_with_body`] to translate a
/// `Err(Error::Provider{...})` from the production providers into
/// `ProbeOutcome::Rejected` (the regions of the algorithm that
/// previously classified by status alone cannot tell `Rejected`
/// from `Indeterminate` once the body is folded into the error).
fn is_max_tokens_rejection_error(err: &Error) -> bool {
    let Error::Provider {
        message,
        http_status,
    } = err
    else {
        return false;
    };
    // Match the same set of HTTP statuses the existing
    // body-classifying path covers.
    let Some(status) = http_status else {
        return false;
    };
    if !(400..500).contains(status) {
        return false;
    }
    body_carries_max_tokens_rejection(message)
}

/// Extract the upstream-reported `max_tokens` cap from a 4xx error
/// body. Used by the Phase 0 / Phase 0.5 short-circuit so the
/// algorithm can converge on the upstream's real boundary in three
/// HTTP round-trips (1 initial + 2 validation) instead of walking
/// the full 30-step exponential phase.
///
/// Three patterns are recognised:
///
/// 1. **Anthropic-compat** (MiniMax direct, OpenCode Go Anthropic
///    routing, qwen3.x): the body carries
///    `"model[<name>] does not support max tokens > N"`. `N` is the
///    boundary value the upstream accepts; we return `N` verbatim.
/// 2. **OpenAI-compat / Responses API** (longcat, gpt-5.6-luna,
///    DeepSeek-direct error path): the body carries
///    `"max_tokens is too large: M. This model supports at most N
///    completion tokens, whereas you provided M"`. `N` is the cap;
///    `M` is the rejected value (we ignore it).
/// 3. **OpenCode Go relay**: the body wraps the upstream's error in
///    `Error from provider (Console Go): Upstream request failed:
///    [invalid_parameter] 参数校验失败: \n/max_tokens: 4294967295 is
///    not less or equal to 131072\n`. The cap `N` follows the
///    literal `is not less or equal to ` substring. Used by the
///    OpenCode Go relay for any model whose upstream rejects with
///    the JSON-schema-validation style `is not less or equal to`
///    phrasing (qwen3.x via OpenCode Go, DeepSeek direct, ...).
///
/// The body is JSON-decoded first so JSON escapes (`\u003e` for
/// `>`, `\u003c` for `<`) in the upstream's `error.message` field
/// match the regex. The raw body still works for the OpenAI-compat
/// case because the message field there uses ASCII punctuation
/// directly.
///
/// Returns `None` when:
/// - The body does not match any pattern (generic 4xx, auth
///   failure, model-not-found, transient 5xx, network error).
/// - The parsed value is `< MIN_AUTOPROBE_FLOOR` (1024) — too small
///   to be a usable cap; reject and fall back to Phase 1.
/// - The parsed value is `>= u32::MAX` — sentinel for "no real cap";
///   reject and fall back to Phase 1.
///
/// Conservative by design: any uncertainty falls through to the
/// existing exponential / bisect algorithm, which still produces
/// the right answer at the cost of more round-trips.
pub fn parse_cap_from_error_body(body: &str) -> Option<u32> {
    use std::sync::OnceLock;

    static RE_ANTHROPIC: OnceLock<regex::Regex> = OnceLock::new();
    static RE_OPENAI: OnceLock<regex::Regex> = OnceLock::new();
    static RE_OPENCODE_GO: OnceLock<regex::Regex> = OnceLock::new();

    // Anthropic-compat: `model[<name>] does not support max tokens > N`.
    // The model name can be hyphenated (qwen3.8-max) or contain dots
    // (minimax-v2.5), so we accept any non-`]` character class.
    let anthropic = RE_ANTHROPIC.get_or_init(|| {
        regex::Regex::new(r"does not support max tokens > (\d+)")
            .expect("parse_cap_from_error_body: anthropic regex compiles")
    });

    // OpenAI-compat / Responses API: `supports at most N completion tokens`.
    // The `completion` adjective lets us skip the `max_tokens is too
    // large: M` segment of the message (we don't care about the
    // rejected value M, only the cap N).
    let openai = RE_OPENAI.get_or_init(|| {
        regex::Regex::new(r"supports at most (\d+) completion tokens")
            .expect("parse_cap_from_error_body: openai regex compiles")
    });

    // OpenCode Go relay: the upstream JSON-schema validation message
    // uses `is not less or equal to N` (note the missing `than` —
    // it's a translation of the Chinese 参数校验失败). Match that
    // variant verbatim so qwen3.x, longcat, and any other model
    // routed through OpenCode Go short-circuit in 3 round-trips.
    let opencode_go = RE_OPENCODE_GO.get_or_init(|| {
        regex::Regex::new(r"is not less or equal to (\d+)")
            .expect("parse_cap_from_error_body: opencode_go regex compiles")
    });

    // The Anthropic upstream (MiniMax) emits `max tokens \u003e N`
    // with the JSON escape for `>` in `error.message`. Unescape the
    // JSON so the regex matches without needing to special-case the
    // escape sequence. We try the JSON-decoded message first; if the
    // body is not JSON (or has no `error.message` field), we fall
    // back to the raw body so the OpenAI-compat shape (which uses
    // ASCII punctuation directly) still works.
    let decoded_message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_owned)
        });
    let search_texts: [&str; 2] = match decoded_message.as_deref() {
        Some(d) => [d, body],
        None => [body, body],
    };

    let raw = search_texts.iter().find_map(|t| {
        anthropic
            .captures(t)
            .and_then(|c| c.get(1))
            .or_else(|| openai.captures(t).and_then(|c| c.get(1)))
            .or_else(|| opencode_go.captures(t).and_then(|c| c.get(1)))
            .and_then(|m| m.as_str().parse::<u64>().ok())
    })?;

    if raw >= u32::MAX as u64 {
        // Sentinel: the upstream reported a value at the u32 boundary,
        // which usually means "no real cap" rather than a usable one.
        return None;
    }
    if raw < MIN_AUTOPROBE_FLOOR as u64 {
        // Too small to be a useful cap (1024 is the algorithm floor).
        return None;
    }
    Some(raw as u32)
}

/// Run the full probe algorithm against an `Arc<dyn ProbeTransport>`.
/// `floor` is the caller-supplied minimum (the `Option<u32>` from
/// `ProviderConfig::max_token_auto`); `ceiling` is the per-provider
/// upper bound (returned by
/// [`crate::llm::provider::Provider::max_tokens_probe_ceiling`]).
/// Returns the discovered `max_tokens` clamped into `[floor,
/// ceiling]`.
///
/// Algorithm overview:
///
/// - **Phase 0 — single-request cap probe.** Fire one probe at
///   `max_tokens = u32::MAX`. Most upstreams reject the request
///   with HTTP 400 and embed the boundary in the error body
///   (`"model[X] does not support max tokens > N"` for
///   Anthropic-compat; `"supports at most N completion tokens"` for
///   OpenAI-compat). When [`parse_cap_from_error_body`] parses a
///   usable value, the algorithm jumps to Phase 0.5; otherwise it
///   falls through to Phase 1 unchanged.
/// - **Phase 0.5 — parallel validation.** Fire two parallel probes
///   at the candidate cap (`N`) and one above it (`N + 1`). If `N`
///   is accepted and `N + 1` is rejected, the candidate is the
///   upstream's true ceiling and the algorithm returns immediately.
///   If both succeed, `N` is treated as a floor (still a usable
///   discovered value). If `N` is rejected, the upstream lied
///   and Phase 1 takes over.
/// - **Phase 1 — exponential search.** Walk `2^1..2^MAX_PROBE_SHIFT`
///   sequentially. The first rejection breaks; `lo` is the last
///   accepted value, `hi` is the first rejection.
/// - **Phase 2 — tightening.** Bisect `[lo + 1, hi - 1]` in
///   20-point parallel batches.
///
/// The exponential phase stops at the smallest `2^k > ceiling`
/// rather than burning a probe round-trip on a value the upstream
/// will reject with HTTP 400 (DeepSeek-direct caps at 393_216;
/// MiniMax-M3 caps at 524_288; OpenCode Go's per-model caps pin
/// the OpenAI-compat and Anthropic-compat and Responses paths at
/// 16_384). Without this short-circuit the probe would otherwise
/// walk all 30 sequential `2^k` values and every value above the
/// real bound would be rejected with the `max_tokens` signature —
/// which the body-classifying branch treats as `Indeterminate` per
/// the v0.7.1 contract, collapsing the discovered ceiling to the
/// last accepted probe.
///
/// The algorithm is independent of the transport — tests use a
/// `MockProbeTransport` and production code uses the
/// `ProviderProbeTransport`. The transport is the only place that
/// talks to the network.
pub async fn detect_max_tokens(
    transport: Arc<dyn ProbeTransport>,
    floor: u32,
    ceiling: u32,
) -> Result<u32> {
    tracing::info!(floor, ceiling, "probe::detect_max_tokens: starting");
    detect_max_tokens_with_phase0_cap_callback(transport, floor, ceiling, |_cap| {}).await
}

/// Like [`detect_max_tokens`] but invokes `on_phase_0_cap(cap)` when
/// Phase 0 / Phase 0.5 successfully extracted a parseable cap from
/// the upstream's error body. The callback is a side-channel that
/// lets [`crate::llm::probe_table::MaxTokensTable::probe_and_store`]
/// persist the upstream-reported cap alongside the discovered
/// value, without breaking the established `Result<u32>` signature
/// of [`detect_max_tokens`]. The callback fires at most once per
/// call and only when Phase 0 / 0.5 short-circuits the algorithm.
///
/// Production callers that do not care about the cap can ignore
/// this overload and call [`detect_max_tokens`] directly.
pub async fn detect_max_tokens_with_phase0_cap_callback<F>(
    transport: Arc<dyn ProbeTransport>,
    floor: u32,
    ceiling: u32,
    on_phase_0_cap: F,
) -> Result<u32>
where
    F: FnOnce(u32),
{
    debug_assert!(
        ceiling >= MIN_AUTOPROBE_FLOOR,
        "ceiling ({ceiling}) must be at least MIN_AUTOPROBE_FLOOR ({MIN_AUTOPROBE_FLOOR})"
    );

    // Phase 0 + Phase 0.5: single-request cap probe with parallel
    // validation. Short-circuits Phase 1 + Phase 2 when the upstream
    // tells us its real boundary up-front (the qwen3.8-max /
    // longcat-2.0 case). Falls back to Phase 1 + Phase 2 when the
    // body is unparseable, the candidate fails validation, or the
    // transport returns `Indeterminate` mid-validation.
    if let Some(cap) = phase_0_short_circuit(transport.clone()).await {
        on_phase_0_cap(cap);
        return Ok(cap.max(floor).min(ceiling));
    }

    // Phase 1: exponential search 2^1..2^MAX_PROBE_SHIFT, with an
    // early break when `n > ceiling` (the smallest `2^k` past the
    // per-provider bound — DeepSeek at k=19 = 524_288, MiniMax at
    // k=20 = 1_048_576, OpenCode Go at k=15 = 32_768). M2: each
    // probe that comes back as `Indeterminate` (transient 5xx /
    // network blip) is retried once at the same `n` before we
    // commit the outcome. A single timeout mid-Phase-1 would
    // otherwise collapse the discovered ceiling by ~½, which is
    // exactly the regression M2 was filed against.
    let mut lo: u32 = 0;
    let mut hi: u32 = ceiling;

    for k in 1..=MAX_PROBE_SHIFT {
        let n = 1u32 << k;
        if n > ceiling {
            // First `2^k` past the per-provider bound. No probe
            // is sent — sending `n` would either be rejected by
            // the upstream (HTTP 400 with `max_tokens` body) or
            // get clamped at the wire, neither of which carries
            // signal for the algorithm. The ceiling is the
            // upper-bound sentinel from this point on.
            break;
        }
        match retry_once_on_indeterminate(transport.as_ref(), n).await {
            ProbeOutcome::Accepted => lo = n,
            _ => {
                // PR-x23: do NOT commit `hi = n; break` until we
                // have observed at least one Accepted value. Some
                // upstreams reject small `max_tokens` values with
                // the same HTTP 400 + `max_tokens` body signature
                // they use for the upper bound (the upstream's
                // valid range starts above 2). Breaking on the
                // first non-Accepted would collapse Phase 1 to
                // `lo = 0, hi = 2` and Phase 2 has no room to
                // search, surfacing the misleading "rejected
                // every probe" error. Keep walking upward until
                // either we see an Accepted (the valid range is
                // above us) or we exhaust the `n > ceiling`
                // short-circuit (every probed value was
                // rejected — `discovered = 0` and the existing
                // error message still fires).
                if lo > 0 {
                    hi = n;
                    break;
                }
            }
        }
    }
    // If every probe through `MAX_PROBE_SHIFT` accepted (only
    // possible when `ceiling >= MAX_AUTOPROBE_CEILING`), `hi`
    // stays at the initial `ceiling`. No separate "all probes
    // passed" fallback is needed — `hi = ceiling` is the right
    // sentinel because Phase 2 searches `[lo + 1, hi - 1]` =
    // `[lo + 1, ceiling - 1]`.

    // Phase 2: tighten with 20-point parallel batches. The user's
    // algorithm spec is "sum 1 to the successful value, subtract 1
    // from the failed value" so we search in (lo, hi). The
    // discovered value is the largest accepted value found by
    // Phase 2 — if Phase 2 confirms everything above `lo` is
    // rejected, the discovered value falls back to `lo` itself
    // (Phase 1's last accepted).
    //
    // Phase 2 keeps stepping down the range until the gap is small
    // enough that step=1 still hits the boundary (i.e. span <= 20).
    // When the span drops below 20, we fall through to linear
    // probes so the discovered value lands exactly on the boundary
    // rather than at some coarse-grained bucket offset.
    let mut lo_strict = lo.saturating_add(1);
    let mut hi_strict = hi.saturating_sub(1);
    let mut phase2_accepted = false;
    let mut phase2_rounds = 0usize;
    for _round in 0..32 {
        if lo_strict >= hi_strict || hi_strict == 0 {
            break;
        }
        let span = hi_strict.saturating_sub(lo_strict);
        let points: Vec<u32> = if span <= 20 {
            // Linear sweep: probe every value in [lo_strict, hi_strict].
            (lo_strict..=hi_strict).collect()
        } else {
            // 20-point parallel batch: step = span / 20.
            let step = span / 20;
            if step == 0 {
                break;
            }
            (1..=20).map(|i| lo_strict + i * step).collect()
        };
        phase2_rounds += 1;
        tracing::trace!(
            round = phase2_rounds,
            lo_strict,
            hi_strict,
            span,
            points = points.len(),
            "probe::detect_max_tokens: phase 2 round"
        );
        let results = parallel_probe(transport.clone(), &points).await;
        let mut new_lo = lo_strict;
        let mut new_hi = hi_strict;
        for (pt, outcome) in points.iter().zip(results.iter()) {
            // M3: re-probe the same point once if the fan-out
            // returned Indeterminate. The Phase-2 boundary is a
            // single 4xx-carrying-max_tokens response; a transient
            // blip should not collapse the entire ceiling.
            let committed = match outcome {
                ProbeOutcome::Indeterminate => {
                    retry_once_on_indeterminate(transport.as_ref(), *pt).await
                }
                other => other.clone(),
            };
            match committed {
                ProbeOutcome::Accepted => {
                    new_lo = *pt;
                    phase2_accepted = true;
                }
                _ => {
                    new_hi = pt.saturating_sub(1);
                    break;
                }
            }
        }
        if new_lo == lo_strict && new_hi == hi_strict {
            break;
        }
        lo_strict = new_lo;
        hi_strict = new_hi;
    }

    let discovered = if phase2_accepted { lo_strict } else { lo };
    tracing::debug!(
        lo,
        lo_strict,
        hi,
        phase2_rounds,
        phase2_accepted,
        discovered,
        "probe::detect_max_tokens: phase 2 done"
    );
    if discovered < MIN_AUTOPROBE_FLOOR {
        tracing::warn!(
            discovered,
            "probe::detect_max_tokens: discovered below floor"
        );
        return Err(Error::Provider {
            message: format!(
                "auto-probe failed to discover a usable max_tokens (got {discovered}); provider likely rejected every probe"
            ),
            http_status: None,
        });
    }
    let out = discovered.max(floor).min(ceiling);
    tracing::info!(discovered, floor, ceiling, final = out, "probe::detect_max_tokens: completed");
    Ok(out)
}

/// M2/M3 helper: re-fire the same probe once when the first
/// attempt comes back `Indeterminate`. The retry is gated on the
/// outcome, not on a timer, so a clean Accepted or Rejected never
/// pays the second round-trip.
async fn retry_once_on_indeterminate(transport: &dyn ProbeTransport, n: u32) -> ProbeOutcome {
    match transport.probe_send(n).await {
        ProbeOutcome::Indeterminate => transport.probe_send(n).await,
        other => other,
    }
}

/// Phase 0 + Phase 0.5: single-request cap probe with parallel
/// validation. Returns the discovered cap when the upstream
/// reports its boundary in the error body and the candidate
/// survives the parallel probe pair; returns `None` when the
/// algorithm should fall through to Phase 1 + Phase 2.
///
/// Algorithm:
///
/// 1. Fire one probe at `max_tokens = u32::MAX`. If the body does
///    not parse to a usable cap via
///    [`parse_cap_from_error_body`], return `None` and let the
///    caller fall through to Phase 1.
/// 2. Otherwise fire two probes in parallel: `max_tokens = N` and
///    `max_tokens = N + 1`.
/// 3. Decision matrix:
///    - `A` accepted, `B` rejected → cap confirmed.
///    - `A` accepted, `B` accepted → `N` is a known floor; still a
///      valid discovered value.
///    - `A` accepted, `B` indeterminate → retry `B` once. On the
///      second `Indeterminate`, use `N` as best-effort.
///    - `A` rejected or indeterminate → upstream lied; the parse
///      was wrong. Fall back to Phase 1.
///
/// Returned value is the cap itself, before the caller's
/// `floor.max(...).min(ceiling)` clamp — `detect_max_tokens`
/// applies the clamp at the end so the contract with Phase 1 is
/// identical (a single `max(floor).min(ceiling)` line in both
/// paths).
async fn phase_0_short_circuit(transport: Arc<dyn ProbeTransport>) -> Option<u32> {
    // Phase 0: one request with max_tokens = u32::MAX. The probe
    // deliberately bypasses the per-provider safety clamp
    // (`Provider::send_probe` exists for this reason) so the wire
    // body reaches the upstream with `max_tokens = u32::MAX`.
    let initial = transport.probe_send_with_body(u32::MAX).await;
    let cap_from_body = parse_cap_from_error_body(&initial.body);

    // Body parse failed (generic 4xx, auth error, network blip,
    // 200 OK, ...). Fall through to Phase 1.
    let n = cap_from_body?;

    // Phase 0.5: parallel validation. Send `max_tokens = N` (the
    // candidate) and `max_tokens = N + 1` (one past the candidate).
    // We use `parallel_probe_with_cancel` with no cancellation
    // token so the two requests run concurrently and the
    // wall-clock is bounded by the slower of the two.
    let candidate_plus_one = n.saturating_add(1);
    let outcomes = parallel_probe(transport.clone(), &[n, candidate_plus_one]).await;
    let outcome_a = outcomes
        .first()
        .cloned()
        .unwrap_or(ProbeOutcome::Indeterminate);
    let outcome_b = outcomes
        .get(1)
        .cloned()
        .unwrap_or(ProbeOutcome::Indeterminate);

    match (outcome_a, outcome_b) {
        // A accepted, B rejected — cap confirmed. B may be
        // Indeterminate here too, but the strict `(Accepted,
        // Rejected)` pair is the textbook confirmation.
        (ProbeOutcome::Accepted, ProbeOutcome::Rejected) => Some(n),

        // A accepted, B accepted — upstream allows more than N.
        // N is still a known floor of the upstream's valid range
        // and a safe discovered value.
        (ProbeOutcome::Accepted, ProbeOutcome::Accepted) => Some(n),

        // A accepted, B indeterminate — retry B once. M-flake
        // tolerance: a single transient blip on the validation
        // probe must not force a full Phase 1 walk.
        (ProbeOutcome::Accepted, ProbeOutcome::Indeterminate) => {
            let retry = transport.probe_send_with_body(candidate_plus_one).await;
            match retry.outcome {
                ProbeOutcome::Rejected | ProbeOutcome::Accepted => Some(n),
                // Still indeterminate after one retry. Treat `N`
                // as best-effort: the upstream answered with a
                // parseable cap once and the body is the source
                // of truth we cannot re-derive otherwise.
                ProbeOutcome::Indeterminate => Some(n),
            }
        }

        // A rejected (upstream lied about accepting N) or A
        // indeterminate (transient on the candidate probe).
        // Either way, the parse was wrong and Phase 1 has to
        // walk the full algorithm.
        (ProbeOutcome::Rejected, _) | (ProbeOutcome::Indeterminate, _) => None,
    }
}

/// 20-point parallel fan-out. Each point runs its own probe against
/// the transport; we collect every outcome before deciding where the
/// boundary is. Failure of any single probe degrades to
/// `Indeterminate` so a transient network blip cannot lock the
/// algorithm.
///
/// M9: each spawned task is wrapped in `tokio::select!` against a
/// `CancellationToken` so a graceful shutdown aborts the fan-out
/// instead of leaking orphaned tasks. Tasks that lose the race
/// report `Indeterminate` so the algorithm can re-probe them.
pub async fn parallel_probe(
    transport: Arc<dyn ProbeTransport>,
    points: &[u32],
) -> Vec<ProbeOutcome> {
    tracing::trace!(count = points.len(), "probe::parallel_probe");
    parallel_probe_with_cancel(transport, points, None).await
}

/// Same as [`parallel_probe`] but with an explicit cancellation
/// handle. When `cancel` is `None` the calls run to completion; when
/// `Some`, a fired token causes every pending probe to return
/// `Indeterminate` so the caller can short-circuit.
pub async fn parallel_probe_with_cancel(
    transport: Arc<dyn ProbeTransport>,
    points: &[u32],
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Vec<ProbeOutcome> {
    let mut handles = Vec::with_capacity(points.len());
    for &pt in points {
        let t = transport.clone();
        let cancel_child = cancel.as_ref().map(|c| c.child_token());
        handles.push(tokio::spawn(async move {
            if let Some(c) = cancel_child {
                tokio::select! {
                    biased;
                    _ = c.cancelled() => ProbeOutcome::Indeterminate,
                    outcome = t.probe_send(pt) => outcome,
                }
            } else {
                t.probe_send(pt).await
            }
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(o) => out.push(o),
            Err(e) => {
                // M8: log the join error so a panic does not vanish.
                tracing::warn!(error = %e, "max_tokens_auto: parallel_probe task join failed");
                out.push(ProbeOutcome::Indeterminate);
            }
        }
    }
    out
}

/// Serialised shape of the persisted table. Lives in
/// `<MOAGAN_HOME>/max_tokens_auto.toml` and is read once at
/// startup; subsequent runs verify the cached value with a single
/// probe before trusting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaxTokensTableFile {
    /// Schema version. Bumped whenever the file shape changes
    /// incompatibly so a future `moagan` refuses to read a stale
    /// file instead of silently misinterpreting it.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// `provider_name -> model_name -> entry`. The nested `BTreeMap`
    /// gives deterministic on-disk ordering so a manual diff after a
    /// probe-run is meaningful.
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Entry>>,
    /// Operator-pinned per-provider cap written by
    /// `moagan probe max_tokens --persist-min`. The cap is the
    /// minimum across every model the operator has probed under
    /// the same provider and is the value the runtime will use
    /// as the hard ceiling on the next run. `#[serde(default)]`
    /// keeps the loader backward-compatible with v1 sidecars
    /// written before this field existed.
    #[serde(default)]
    pub operator_caps: std::collections::BTreeMap<String, OperatorCap>,
}

fn default_schema_version() -> u32 {
    1
}

impl MaxTokensTableFile {
    /// Current schema version this binary knows how to read.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Build an empty table. Useful for tests that bypass the
    /// on-disk file.
    pub fn new_empty() -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            providers: std::collections::BTreeMap::new(),
            operator_caps: std::collections::BTreeMap::new(),
        }
    }

    /// Read from a TOML file. Missing file is `Ok(new_empty())`;
    /// malformed file is `Err(Provider(...))` so a typo in
    /// operator-land cannot silently break startup.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        tracing::trace!(path = %path.display(), "MaxTokensTableFile::load");
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let parsed: Self = toml::from_str(&s).map_err(|e| {
                    tracing::warn!(error = %e, path = %path.display(), "max_tokens_auto.toml malformed");
                    Error::Provider {
                        message: format!(
                            "max_tokens_auto.toml at {} is malformed: {e}",
                            path.display()
                        ),
                        http_status: None,
                    }
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    tracing::warn!(
                        file_version = parsed.schema_version,
                        max_supported = Self::CURRENT_SCHEMA_VERSION,
                        "max_tokens_auto.toml schema_version too new"
                    );
                    return Err(Error::Provider {
                        message: format!(
                            "max_tokens_auto.toml at {} has schema_version={}, this binary only knows up to {}",
                            path.display(),
                            parsed.schema_version,
                            Self::CURRENT_SCHEMA_VERSION
                        ),
                        http_status: None,
                    });
                }
                tracing::debug!(
                    path = %path.display(),
                    providers = parsed.providers.len(),
                    "MaxTokensTableFile::load: ok"
                );
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::trace!(path = %path.display(), "MaxTokensTableFile::load: missing");
                Ok(Self::new_empty())
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "MaxTokensTableFile::load: io error");
                Err(Error::Io(crate::error::IoError::Raw(e)))
            }
        }
    }

    /// Persist to disk. Writes via `tempfile` then renames so a crash
    /// mid-write cannot leave a truncated file.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Provider {
                message: format!(
                    "create dir for max_tokens_auto.toml at {}: {e}",
                    parent.display()
                ),
                http_status: None,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| Error::Provider {
            message: format!("encode max_tokens_auto.toml: {e}"),
            http_status: None,
        })?;
        let body = quote_provider_model_keys(&body);
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(|e| Error::Provider {
                message: format!("tempfile for max_tokens_auto.toml: {e}"),
                http_status: None,
            })?;
        std::fs::write(tmp.path(), body).map_err(|e| Error::Provider {
            message: format!("write max_tokens_auto.toml: {e}"),
            http_status: None,
        })?;
        tmp.persist(path).map_err(|e| Error::Provider {
            message: format!("rename max_tokens_auto.toml into place: {e}"),
            http_status: None,
        })?;
        Ok(())
    }
}

/// One row of the persisted table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Last successfully probed value (i.e., the value the upstream
    /// accepted without a `max_tokens` rejection).
    pub max_tokens: u32,
    /// ISO-8601 timestamp of the last successful probe.
    pub detected_at: String,
    /// ISO-8601 timestamp of the last successful verification probe
    /// (i.e., the cached value was re-probed and still passed).
    /// Equal to `detected_at` on the first probe of a fresh model.
    #[serde(default)]
    pub verified_at: String,
    /// Always `true` for entries the probe produced. The field is
    /// explicit so a human reading the file can tell at a glance
    /// which entries came from auto-detection vs. operator-pinned
    /// overrides.
    pub auto: bool,
    /// How many probes the algorithm ran to discover this value.
    /// Useful for telemetry; the algorithm makes 30 sequential
    /// probes plus 20 per tightening round.
    #[serde(default)]
    pub attempts: u32,
    /// Upstream-reported hard ceiling parsed from the Phase 0
    /// single-request error body
    /// (`"model[X] does not support max tokens > N"` for
    /// Anthropic-compat; `"supports at most N completion tokens"`
    /// for OpenAI-compat). When `Some`, the cached value is the
    /// upstream's own reported boundary — no future run needs to
    /// walk the exponential phase to rediscover it. `None` for
    /// entries discovered via Phase 1 / Phase 2 (the algorithm's
    /// last accepted value is the discovered value, not a verified
    /// upstream boundary) and for backward-compat reads of v1
    /// sidecars written before Phase 0 was added.
    #[serde(default)]
    pub ceiling: Option<u32>,
}

/// Operator-pinned per-provider cap. Written by
/// `moagan probe max_tokens --persist-min` to record the minimum
/// across every model probed under one provider; the runtime
/// reads the cap on next start so a fresh run never has to
/// re-probe the same models to land at the same answer. `auto`
/// is always `false` so a human reading the file can tell the
/// entry was pinned by an operator (not discovered by the
/// algorithm).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorCap {
    /// The pinned cap in tokens.
    pub min: u32,
    /// Always `false` for an operator-pinned entry. Explicit so a
    /// grep-friendly TOML diff between auto-discovered and
    /// operator-pinned entries stays trivial.
    pub auto: bool,
    /// ISO-8601 timestamp the cap was written.
    pub detected_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Canned transport: accepts at and below `accept_up_to`,
    /// rejects strictly above. Models the typical
    /// `upstream rejects max_tokens > N` semantic.
    #[derive(Clone)]
    struct CappedTransport {
        accept_up_to: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ProbeTransport for CappedTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            if max_tokens <= self.accept_up_to.load(Ordering::SeqCst) {
                ProbeOutcome::Accepted
            } else {
                ProbeOutcome::Rejected
            }
        }
    }

    fn cap(n: u32) -> Arc<dyn ProbeTransport> {
        Arc::new(CappedTransport {
            accept_up_to: Arc::new(AtomicU32::new(n)),
        })
    }

    #[tokio::test]
    async fn detect_finds_cap_at_8k() {
        let got = detect_max_tokens(cap(8192), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 8192);
    }

    #[tokio::test]
    async fn detect_finds_cap_at_524k() {
        let got = detect_max_tokens(cap(524_288), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert!((524_000..=524_288).contains(&got), "got {got}");
    }

    #[tokio::test]
    async fn detect_caps_at_ceiling_when_provider_accepts_everything() {
        let got = detect_max_tokens(cap(u32::MAX), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, MAX_AUTOPROBE_CEILING);
    }

    #[tokio::test]
    async fn detect_returns_error_when_provider_rejects_everything() {
        // accept_up_to = MIN_AUTOPROBE_FLOOR - 1 means the probe
        // at k=10 (2^10 = 1024) is the first to reject, and the
        // discovered value lands below the floor. The function
        // surfaces an error rather than returning a degenerate
        // value.
        let err = detect_max_tokens(
            cap(MIN_AUTOPROBE_FLOOR - 1),
            MIN_AUTOPROBE_FLOOR,
            MAX_AUTOPROBE_CEILING,
        )
        .await
        .expect_err("provider rejects everything must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("auto-probe failed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn floor_is_respected() {
        // Provider caps at 1024; the operator wants at least 8192.
        // The discovered value is 1024, but the floor pushes it to
        // 8192 so the wire body always carries at least the floor.
        let got = detect_max_tokens(cap(1024), 8192, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 8192);
    }

    #[test]
    fn ceiling_is_one_shifted_by_probe_shift() {
        assert_eq!(MAX_AUTOPROBE_CEILING, 1u32 << MAX_PROBE_SHIFT);
        assert_eq!(MAX_AUTOPROBE_CEILING, 1_073_741_824);
    }

    #[test]
    fn floor_is_documented_value() {
        assert_eq!(MIN_AUTOPROBE_FLOOR, 1024);
    }

    #[test]
    fn body_carries_max_tokens_signature() {
        assert!(body_carries_max_tokens_rejection(
            r#"{"type":"error","error":{"message":"max_tokens > 524288"}}"#
        ));
        assert!(body_carries_max_tokens_rejection(
            "max tokens limit reached"
        ));
        assert!(body_carries_max_tokens_rejection(
            "maximum context length exceeded"
        ));
        assert!(!body_carries_max_tokens_rejection(
            r#"{"error":"model not found"}"#
        ));
        assert!(!body_carries_max_tokens_rejection(""));
    }

    #[test]
    fn table_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        let mut file = MaxTokensTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    max_tokens: 524_288,
                    detected_at: "2026-08-11T00:00:00Z".to_owned(),
                    verified_at: "2026-08-11T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 35,
                    ceiling: None,
                },
            );
        file.save(&path).unwrap();
        let back = MaxTokensTableFile::load(&path).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn load_missing_file_returns_empty_table() {
        let path = std::path::PathBuf::from("/nonexistent/max_tokens_auto.toml");
        let t = MaxTokensTableFile::load(&path).unwrap();
        assert!(t.providers.is_empty());
        assert_eq!(t.schema_version, 1);
    }

    #[test]
    fn load_rejects_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        std::fs::write(&path, "schema_version = 999\n[providers]\n").unwrap();
        let err = MaxTokensTableFile::load(&path).expect_err("future schema must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("schema_version")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        std::fs::write(&path, "this is = not valid toml = at all").unwrap();
        let err = MaxTokensTableFile::load(&path).expect_err("malformed must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("malformed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    /// `Response` is unused in `probe_send` but the trait surface
    /// needs to compile against the shared type. Pin the field
    /// layout here so a refactor of `wire::Response` cannot silently
    /// break the probe.
    #[test]
    fn response_layout_compiles() {
        let r = crate::llm::wire::Response {
            text: String::new(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: crate::llm::wire::Usage::default(),
        };
        assert_eq!(r.text.len(), 0);
    }

    // -----------------------------------------------------------------
    // M-bug follow-up tests
    // -----------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;

    /// Transport that returns Indeterminate once for the *first* call
    /// ever, then behaves like `CappedTransport` thereafter. Models a
    /// transient 5xx that resolves itself after one retry.
    struct FlakyTransport {
        accept_up_to: u32,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProbeTransport for FlakyTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            let call_idx = self.calls.fetch_add(1, Ordering::SeqCst);
            // The very first call (any value of n) returns
            // Indeterminate. The retry must therefore succeed and
            // the algorithm should still discover the cap.
            if call_idx == 0 {
                return ProbeOutcome::Indeterminate;
            }
            if max_tokens <= self.accept_up_to {
                ProbeOutcome::Accepted
            } else {
                ProbeOutcome::Rejected
            }
        }
    }

    /// M2: a single Indeterminate mid-Phase-1 should NOT collapse the
    /// discovered ceiling. The retry recovers the boundary.
    #[tokio::test]
    async fn m2_indeterminate_in_phase1_is_retried() {
        // accept_up_to = 524288; the very first call returns
        // Indeterminate (5xx blip). The retry recovers and the
        // algorithm should still discover ~524288.
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(FlakyTransport {
            accept_up_to: 524_288,
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert!(
            (524_000..=524_288).contains(&got),
            "retry should recover cap, got {got}"
        );
    }

    /// C2: a 4xx that does NOT carry the "max_tokens" signature
    /// (e.g. 401 auth, 404 model-not-found) must classify as
    /// Indeterminate, not Rejected — otherwise a transient auth
    /// failure would collapse the discovered ceiling to that
    /// exact probe value.
    #[tokio::test]
    async fn c2_generic_4xx_is_indeterminate_not_rejected() {
        // We test the body_carries_max_tokens_rejection helper
        // directly because the classify-by-status path runs inside
        // the trait method (not the algorithm). The transport
        // returns a Response whose text we control.
        let r = crate::llm::wire::Response {
            text: r#"{"type":"error","error":{"message":"invalid api key"}}"#.into(),
            finish_reason: None,
            truncated: false,
            usage: crate::llm::wire::Usage::default(),
        };
        assert!(!body_carries_max_tokens_rejection(&r.text));
        // Boundary signature still classifies as Rejected-eligible.
        let r2 = crate::llm::wire::Response {
            text: r#"{"type":"error","error":{"message":"max_tokens > 524288"}}"#.into(),
            finish_reason: None,
            truncated: false,
            usage: crate::llm::wire::Usage::default(),
        };
        assert!(body_carries_max_tokens_rejection(&r2.text));
    }

    /// M9: parallel_probe_with_cancel must return Indeterminate for
    /// every point whose task was cancelled before completing.
    #[tokio::test]
    async fn m9_parallel_probe_respects_cancellation_token() {
        use tokio_util::sync::CancellationToken;
        struct NeverTransport;
        #[async_trait]
        impl ProbeTransport for NeverTransport {
            async fn probe_send(&self, _max_tokens: u32) -> ProbeOutcome {
                // Sleep long enough that the cancel beats the probe.
                tokio::time::sleep(Duration::from_secs(60)).await;
                ProbeOutcome::Accepted
            }
        }
        let t: Arc<dyn ProbeTransport> = Arc::new(NeverTransport);
        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();
        // Cancel from another task after a tiny delay.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_child.cancel();
        });
        let pts: Vec<u32> = (1..=10).collect();
        let outcomes = parallel_probe_with_cancel(t, &pts, Some(cancel)).await;
        // Every probe should have come back Indeterminate because the
        // token fired before the sleep finished.
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, ProbeOutcome::Indeterminate)),
            "expected every probe to be Indeterminate, got {outcomes:?}"
        );
    }

    /// M7 smoke: probe_tasks_started increments per task spawn, not
    /// per HTTP round-trip. Pinned by table test (see probe_table.rs).
    #[test]
    fn m7_probe_tasks_started_field_exists() {
        // Compile-time check: the field name must be `probe_tasks_started`,
        // not the legacy `probes_attempted`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        let t =
            crate::llm::probe_table::MaxTokensTable::from_path(&path, MIN_AUTOPROBE_FLOOR, false)
                .unwrap();
        assert_eq!(t.probe_tasks_started(), 0);
    }

    // -----------------------------------------------------------------
    // PR-473 regression pin: per-provider probe ceiling.
    //
    // The exponential phase used to walk `2^1..2^30` regardless of
    // the provider; on a fresh CI runner with no cached probe
    // result, DeepSeek-direct was probed with `2^30 = 1_073_741_824`
    // and rejected with HTTP 400 `invalid_request_error`. The fix
    // adds a `ceiling` parameter to `detect_max_tokens` so the
    // exponential phase short-circuits at the first `2^k` past
    // the per-provider hard cap. Tests below pin the contract.
    // -----------------------------------------------------------------

    /// Transport that records every probe value ever sent and
    /// rejects anything strictly above `accept_up_to`. Used by the
    /// ceiling tests below to assert the algorithm never probes
    /// above the per-provider bound. Phase 0 fires once with
    /// `max_tokens = u32::MAX`; that probe is the only call allowed
    /// to exceed the per-provider ceiling.
    #[derive(Clone)]
    struct Phase0AwareRecordingTransport {
        accept_up_to: u32,
        calls: Arc<AtomicU32>,
        max_sent: Arc<AtomicU32>,
        phase0_seen: Arc<std::sync::atomic::AtomicBool>,
        max_non_phase0_sent: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ProbeTransport for Phase0AwareRecordingTransport {
        async fn probe_send(&self, n: u32) -> ProbeOutcome {
            self.probe_send_with_body(n).await.outcome
        }

        async fn probe_send_with_body(&self, n: u32) -> ProbeResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Update max_sent only if this value is strictly larger.
            let prev = self.max_sent.load(Ordering::SeqCst);
            if n > prev {
                self.max_sent.store(n, Ordering::SeqCst);
            }
            if n == u32::MAX {
                self.phase0_seen.store(true, Ordering::SeqCst);
                // Empty body so Phase 0 falls through to Phase 1.
                let accepted = self.accept_up_to == u32::MAX;
                ProbeResult {
                    outcome: if accepted {
                        ProbeOutcome::Accepted
                    } else {
                        ProbeOutcome::Rejected
                    },
                    body: String::new(),
                }
            } else {
                let prev = self.max_non_phase0_sent.load(Ordering::SeqCst);
                if n > prev {
                    self.max_non_phase0_sent.store(n, Ordering::SeqCst);
                }
                if n <= self.accept_up_to {
                    ProbeResult {
                        outcome: ProbeOutcome::Accepted,
                        body: String::new(),
                    }
                } else {
                    ProbeResult {
                        outcome: ProbeOutcome::Rejected,
                        body: String::new(),
                    }
                }
            }
        }
    }

    /// PR-473 regression pin: with the per-provider ceiling set to
    /// `DEEPSEEK_MAX_TOKENS_CAP = 393_216`, the exponential phase
    /// short-circuits at `2^19 = 524_288` and the algorithm never
    /// sends a value strictly above `DEEPSEEK_MAX_TOKENS_CAP` on
    /// the wire **after Phase 0**. Discovered value still lands
    /// near the upstream bound (the transport accepts everything
    /// up to 524_288, so Phase 1 ends with `lo = 524_288` past the
    /// `n > ceiling` break — but the probe never tries the value
    /// 524_288 either; the break happens before the probe fires).
    ///
    /// Phase 0 deliberately fires one probe at `max_tokens =
    /// u32::MAX` to elicit the upstream's reported cap. The Phase 0
    /// probe is the only call allowed to exceed the per-provider
    /// ceiling; every subsequent walk respects it. The
    /// `max_non_phase0_sent` assertion below pins the contract:
    /// after Phase 0 fires, the algorithm's walk stays within
    /// `DEEPSEEK_MAX_TOKENS_CAP`.
    #[tokio::test]
    async fn pr473_probe_never_sends_value_above_deepseek_ceiling() {
        use crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP;
        let calls = Arc::new(AtomicU32::new(0));
        let max_sent = Arc::new(AtomicU32::new(0));
        let phase0_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let max_non_phase0_sent = Arc::new(AtomicU32::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(Phase0AwareRecordingTransport {
            accept_up_to: 1_048_576,
            calls: calls.clone(),
            max_sent: max_sent.clone(),
            phase0_seen: phase0_seen.clone(),
            max_non_phase0_sent: max_non_phase0_sent.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, DEEPSEEK_MAX_TOKENS_CAP)
            .await
            .unwrap();
        // Phase 0 fired (transport was permissive enough to
        // accept everything; body was empty → no parseable cap →
        // Phase 0 returns None → algorithm falls through to Phase 1).
        assert!(
            phase0_seen.load(std::sync::atomic::Ordering::SeqCst),
            "Phase 0 must fire at least once"
        );
        // After Phase 0, the largest probe value sent on the wire
        // stays at or below DEEPSEEK_MAX_TOKENS_CAP. With
        // exponential phase going 2^1..2^18 (262_144), the largest
        // non-Phase-0 value probed is 262_144 — well below the
        // ceiling.
        let max_after = max_non_phase0_sent.load(Ordering::SeqCst);
        assert!(
            max_after <= DEEPSEEK_MAX_TOKENS_CAP,
            "Phase 1+ sent a value ({max_after}) above DEEPSEEK_MAX_TOKENS_CAP ({DEEPSEEK_MAX_TOKENS_CAP}) — short-circuit failed"
        );
        assert!(
            got <= DEEPSEEK_MAX_TOKENS_CAP,
            "discovered value ({got}) exceeded DEEPSEEK_MAX_TOKENS_CAP"
        );
        // Sanity: the probe short-circuits at k=19 (the first
        // `2^k > 393_216`). Without the ceiling the algorithm
        // would run 19 more sequential probes for k=19..=30
        // (each accepted by this permissive transport), plus a
        // full 32-round Phase 2; with the ceiling, Phase 1 ends
        // at k=18. Phase 2 still fires (it tightens Phase 1's
        // `lo`), so we check that the total budget stays under
        // the legacy 30 + 32 * 20 + Phase 0 / 0.5 (≤ 3 calls).
        let total_calls = calls.load(Ordering::SeqCst);
        assert!(
            total_calls <= 30 + 32 * 20 + 3,
            "probe ran more than the algorithm budget + Phase 0/0.5, got {total_calls}"
        );
    }

    /// PR-473 regression pin: the ceiling clamp contract applies
    /// even when the transport would accept the value. A
    /// hypothetical transport that accepts 1_000_000 (above the
    /// DeepSeek cap) must not let `max_tokens = 1_000_000` leak
    /// into a real DeepSeek probe **after Phase 0**. The ceiling
    /// is the safety net, not the transport.
    #[tokio::test]
    async fn pr473_probe_clamp_applies_even_when_transport_accepts() {
        use crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP;
        let calls = Arc::new(AtomicU32::new(0));
        let max_sent = Arc::new(AtomicU32::new(0));
        let phase0_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let max_non_phase0_sent = Arc::new(AtomicU32::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(Phase0AwareRecordingTransport {
            // Always accept; the ceiling is what we trust.
            accept_up_to: u32::MAX,
            calls: calls.clone(),
            max_sent: max_sent.clone(),
            phase0_seen: phase0_seen.clone(),
            max_non_phase0_sent: max_non_phase0_sent.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, DEEPSEEK_MAX_TOKENS_CAP)
            .await
            .unwrap();
        // Returned value is bounded by ceiling, not by what the
        // transport would accept.
        assert!(got <= DEEPSEEK_MAX_TOKENS_CAP, "got {got}");
        let max_after = max_non_phase0_sent.load(Ordering::SeqCst);
        assert!(
            max_after <= DEEPSEEK_MAX_TOKENS_CAP,
            "Phase 1+ sent {max_after} on the wire — ceiling clamp failed for the algorithm walk"
        );
    }

    /// The default ceiling (MAX_AUTOPROBE_CEILING) preserves the
    /// legacy unbounded behaviour for callers that do not opt in
    /// to a per-provider ceiling. Existing unit + integration
    /// tests pass MAX_AUTOPROBE_CEILING for this reason; the
    /// discovered value lands at the upstream boundary regardless
    /// of the ceiling (as long as ceiling >= boundary).
    #[tokio::test]
    async fn ceiling_at_or_above_boundary_does_not_constrain_discovery() {
        // Provider accepts everything up to 8_192. With the
        // default ceiling (1.07G), the algorithm must still
        // discover 8_192.
        let got = detect_max_tokens(cap(8192), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(
            got, 8192,
            "ceiling above the boundary must not constrain discovery"
        );
    }

    /// PR-x23 regression pin: when the upstream rejects the smallest
    /// probe value with the same HTTP 400 + `max_tokens` body it
    /// uses for the upper bound (some relays require
    /// `max_tokens >= N` and surface that requirement with the
    /// standard rejection signature), Phase 1 must keep walking
    /// upward until it finds an Accepted value. Without the fix
    /// (`if lo > 0 { break }`), the algorithm collapses to
    /// `lo = 0, hi = 2` and surfaces the misleading "rejected every
    /// probe" error even though the upstream's true ceiling is
    /// well above `MIN_AUTOPROBE_FLOOR`.
    #[derive(Clone)]
    struct MinFloorTransport {
        min_accepted: u32,
        max_accepted: u32,
    }

    #[async_trait]
    impl ProbeTransport for MinFloorTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            if max_tokens < self.min_accepted || max_tokens > self.max_accepted {
                ProbeOutcome::Rejected
            } else {
                ProbeOutcome::Accepted
            }
        }
    }

    #[tokio::test]
    async fn phase1_first_probe_rejected_advances_upward() {
        // Upstream requires max_tokens >= 1024 and rejects anything
        // > 4096. Without the Phase 1 robustness fix the algorithm
        // would conclude `lo = 0` after the first probe at n=2 is
        // rejected and fail with "got 0". With the fix, Phase 1
        // walks n=2..512 (all rejected), then accepts n=1024,
        // then breaks at n=2048 (rejected), and Phase 2 narrows
        // between [1024, 2048].
        let t: Arc<dyn ProbeTransport> = Arc::new(MinFloorTransport {
            min_accepted: 1024,
            max_accepted: 4096,
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .expect("algorithm must converge when upstream has a min floor");
        assert!(
            (1024..=4096).contains(&got),
            "discovered value ({got}) must land inside the upstream valid range [1024, 4096]"
        );
    }

    // -----------------------------------------------------------------
    // `quote_provider_model_keys` — cosmetic normalisation of the
    // sidecar so every `[providers.*.*]` header uses double-quoted
    // keys regardless of whether the underlying name contains a
    // bare-key-disqualifying character (e.g. `.` inside
    // `mimo-v2.5`). Pinning the shape here keeps the operator's
    // expectation stable across `toml` crate upgrades. Mirrors the
    // same set of tests in
    // [`crate::llm::temperature_probe::tests`] so both sidecars
    // share the same quoting contract.
    // -----------------------------------------------------------------
    #[test]
    fn quote_provider_model_keys_quotes_bare_keys() {
        let input = "\
schema_version = 1\n\
[providers.kimi-k3.kimi-k3]\n\
max_tokens = 524288\n\
";
        let out = quote_provider_model_keys(input);
        assert!(
            out.contains(r#"[providers."kimi-k3"."kimi-k3"]"#),
            "bare keys must be quoted; got:\n{out}"
        );
    }

    #[test]
    fn quote_provider_model_keys_idempotent_on_already_quoted_keys() {
        let input = r#"[providers."glm-5.1"."glm-5.1"]
max_tokens = 524288
"#;
        let out = quote_provider_model_keys(input);
        assert_eq!(out, input, "already-quoted keys must be left untouched");
    }

    #[test]
    fn quote_provider_model_keys_ignores_non_provider_headers() {
        // The regex only matches `[providers.X.Y]`. Other tables
        // (e.g. `[operator_caps]`, `[some_other_table]`) are
        // untouched even if their keys are bare.
        let input = "\
[operator_caps.minimax]\n\
min = 524288\n\
[providers.kimi-k3.kimi-k3]\n\
max_tokens = 524288\n\
";
        let out = quote_provider_model_keys(input);
        assert!(
            out.contains(r#"[providers."kimi-k3"."kimi-k3"]"#),
            "providers header must be quoted; got:\n{out}"
        );
        assert!(
            out.contains("[operator_caps.minimax]"),
            "non-provider headers must be untouched; got:\n{out}"
        );
    }

    // -----------------------------------------------------------------
    // `parse_cap_from_error_body` — Phase 0 single-request cap parser
    // used to short-circuit the algorithm in three round-trips for
    // upstreams (MiniMax-M3, qwen3.8-max, longcat-2.0) whose error
    // body carries the boundary value verbatim. The helper is
    // intentionally conservative: any uncertainty falls through to
    // Phase 1 with `None`.
    // -----------------------------------------------------------------

    /// Anthropic-compat body (`model[MiniMax-M2.5] does not support
    /// max tokens > 196608 (2013)`) parses to the boundary value.
    /// The `(2013)` suffix is MiniMax's request-id pattern; the
    /// helper must accept it.
    #[test]
    fn parse_cap_anthropic_compat_body_basic() {
        let body = r#"{"type":"error","error":{"message":"model[MiniMax-M2.5] does not support max tokens > 196608 (2013)"}}"#;
        assert_eq!(parse_cap_from_error_body(body), Some(196_608));
    }

    /// Anthropic-compat with a hyphenated model name (qwen3.8-max,
    /// qwen3.7-max) parses to the boundary value. The model-name
    /// brackets `[...]` are deliberately excluded from the regex
    /// because the actual numeric boundary is what we need.
    #[test]
    fn parse_cap_anthropic_compat_with_hyphenated_model() {
        let body = r#"{"type":"error","error":{"message":"model[qwen3.8-max] does not support max tokens > 524288"}}"#;
        assert_eq!(parse_cap_from_error_body(body), Some(524_288));
    }

    /// Anthropic-compat with a dotted model name (mimo-v2.5) parses
    /// to the boundary value.
    #[test]
    fn parse_cap_anthropic_compat_with_dots() {
        let body = r#"{"type":"error","error":{"message":"model[mimo-v2.5] does not support max tokens > 131072"}}"#;
        assert_eq!(parse_cap_from_error_body(body), Some(131_072));
    }

    /// OpenAI-compat body (`max_tokens is too large: M. This model
    /// supports at most N completion tokens, whereas you provided
    /// M.`) parses to `N`, ignoring the rejected value `M`.
    #[test]
    fn parse_cap_openai_compat_basic() {
        let body = r#"{"error":{"param":"max_tokens is too large: 200000. This model supports at most 131072 completion tokens, whereas you provided 200000.","type":"server_error","message":"..."}}"#;
        assert_eq!(parse_cap_from_error_body(body), Some(131_072));
    }

    /// Bodies that do not match either pattern must return `None`
    /// so Phase 1 takes over. Generic 4xx errors (auth, model-not-
    /// found, rate-limit) and empty strings must all be rejected
    /// without false positives.
    #[test]
    fn parse_cap_returns_none_for_unrelated_body() {
        assert_eq!(parse_cap_from_error_body("model not found"), None);
        assert_eq!(parse_cap_from_error_body("auth error"), None);
        assert_eq!(parse_cap_from_error_body(""), None);
        assert_eq!(parse_cap_from_error_body("internal server error"), None);
        assert_eq!(
            parse_cap_from_error_body(
                r#"{"error":{"message":"unrelated error","type":"server_error"}}"#
            ),
            None
        );
    }

    /// A 200-OK body that happens to contain the cap substring must
    /// NOT short-circuit Phase 0 — `parse_cap_from_error_body` is
    /// pure text parsing and the algorithm only calls it after a
    /// `Rejected` outcome, but the helper itself must not assume
    /// status. The body text is the only input we test here.
    /// Returning `None` is the safe answer because Phase 0 would
    /// have classified the 200 as `Accepted` and never called the
    /// parser in production; the test pins the no-side-effects
    /// contract.
    #[test]
    fn parse_cap_returns_none_for_non_4xx_simulation() {
        // The parser is called regardless of HTTP status. The
        // body happens to carry the cap substring; the parser
        // returns the parsed value. Phase 0 only reaches the
        // parser when the upstream rejected (4xx carrying
        // `max_tokens`); the test below pins that the parser
        // does not refuse well-formed bodies outright.
        let body = r#"{"content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1}}"#;
        assert_eq!(parse_cap_from_error_body(body), None);
        // When the body actually carries the cap, the parser
        // returns the value (caller decides whether to use it).
        let body_with_cap =
            r#"{"error":{"message":"model[X] does not support max tokens > 100000"}}"#;
        assert_eq!(parse_cap_from_error_body(body_with_cap), Some(100_000));
    }

    /// Pin the parser against the real boundaries observed on the
    /// 2026-08-04 model roster: 131_072 (qwen3.8-max, longcat-2.0),
    /// 196_608 (MiniMax-M2.5), 524_288 (MiniMax-M3, qwen3.7-max).
    #[test]
    fn parse_cap_handles_large_values() {
        assert_eq!(
            parse_cap_from_error_body(
                r#"{"error":{"message":"model[qwen3.8-max] does not support max tokens > 131072"}}"#
            ),
            Some(131_072)
        );
        assert_eq!(
            parse_cap_from_error_body(
                r#"{"error":{"message":"model[MiniMax-M2.5] does not support max tokens > 196608"}}"#
            ),
            Some(196_608)
        );
        assert_eq!(
            parse_cap_from_error_body(
                r#"{"error":{"message":"model[MiniMax-M3] does not support max tokens > 524288"}}"#
            ),
            Some(524_288)
        );
    }

    /// A body that reports `> u32::MAX` is a sentinel for "no real
    /// cap" and must be rejected. Phase 1 with the per-provider
    /// ceiling will discover the boundary by exponential search in
    /// that case; Phase 0 cannot.
    #[test]
    fn parse_cap_rejects_u32_max_sentinel() {
        // 4294967295 == u32::MAX. Reports the literal u32 boundary,
        // which we treat as "no real cap" rather than a usable one.
        let body = r#"{"error":{"message":"model[X] does not support max tokens > 4294967295"}}"#;
        assert_eq!(parse_cap_from_error_body(body), None);
        // Same via the OpenAI-compat shape.
        let body2 = r#"{"error":{"param":"max_tokens is too large: 5000000000. This model supports at most 4294967295 completion tokens, whereas you provided 5000000000."}}"#;
        assert_eq!(parse_cap_from_error_body(body2), None);
    }

    /// A body that reports a value below the algorithm floor
    /// (< 1024) is rejected as "not a useful cap". This protects
    /// against pathological upstream responses (some relays
    /// mistakenly report 1 or 100 as the boundary when they mean
    /// "minimum required"); we let Phase 1 walk the full algorithm
    /// instead of trusting a degenerate value.
    #[test]
    fn parse_cap_rejects_zero_or_small() {
        let body1 = r#"{"error":{"message":"model[X] does not support max tokens > 100"}}"#;
        assert_eq!(parse_cap_from_error_body(body1), None);
        let body2 = r#"{"error":{"message":"model[X] does not support max tokens > 0"}}"#;
        assert_eq!(parse_cap_from_error_body(body2), None);
        let body3 = r#"{"error":{"param":"max_tokens is too large: 200. This model supports at most 50 completion tokens, whereas you provided 200."}}"#;
        assert_eq!(parse_cap_from_error_body(body3), None);
    }

    /// The Anthropic-compat upstream (MiniMax) emits `>` as the
    /// JSON escape `\u003e` inside `error.message`. The helper must
    /// unescape the JSON so the regex matches; without this, Phase
    /// 0 falls through to Phase 1 unnecessarily.
    #[test]
    fn parse_cap_anthropic_compat_json_escaped_greater_than() {
        // Real MiniMax production body: `invalid params, model[MiniMax-M2.5]
        // does not support max tokens \u003e 196608 (2013)`. The
        // escape needs to be decoded before the regex match.
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"invalid params, model[MiniMax-M2.5] does not support max tokens \u003e 196608 (2013)"},"request_id":"req-123"}"#;
        assert_eq!(parse_cap_from_error_body(body), Some(196_608));
    }

    /// Body without an `error.message` JSON envelope falls back to
    /// matching the raw text (OpenAI-compat shape: the body uses
    /// ASCII punctuation directly).
    #[test]
    fn parse_cap_falls_back_to_raw_text_when_no_message_field() {
        // No `error.message` field, but the body itself contains
        // the cap substring (e.g. a non-JSON upstream error). The
        // helper falls back to matching the raw body.
        let body = "supports at most 524288 completion tokens";
        assert_eq!(parse_cap_from_error_body(body), Some(524_288));
    }

    /// OpenCode Go relay wraps the upstream JSON-schema validation
    /// error in `Error from provider (Console Go): Upstream request
    /// failed: [invalid_parameter] 参数校验失败: \n/max_tokens:
    /// 4294967295 is not less or equal to 131072\n`. The cap
    /// follows the literal `is not less or equal to` substring
    /// (note: no `than`; it's a translation of the Chinese
    /// `参数校验失败`).
    #[test]
    fn parse_cap_opencode_go_is_not_less_or_equal() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"Error from provider (Console Go): Upstream request failed: [invalid_parameter] 参数校验失败: \n/max_tokens: 4294967295 is not less or equal to 131072\n"}}"#;
        assert_eq!(parse_cap_from_error_body(body), Some(131_072));
    }

    // -----------------------------------------------------------------
    // Phase 0 / Phase 0.5: single-request cap probe with parallel
    // validation. Tested with a controllable `BodyMockTransport`
    // primed to return the canonical Anthropic-compat error bodies
    // and the upstream-confirming 200 OK / 4xx rejection pairs.
    // -----------------------------------------------------------------

    /// Phase 0 short-circuits when the initial probe returns a
    /// parseable cap. Total wire calls: 1 (Phase 0) + 2 (Phase
    /// 0.5 A and B). The discovered value lands at the cap.
    #[tokio::test]
    async fn phase_0_short_circuits_when_body_has_cap() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            accept_up_to: u32::MAX, // accept anything (Phase 0.5 A)
            body_for_cap: Some(CAP_BODY_MINIMAX_M3.to_owned()),
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 524_288, "Phase 0 must discover the parsed cap");
        assert!(
            calls.load(Ordering::SeqCst) <= 3,
            "Phase 0 + Phase 0.5 must fire at most 3 probes, got {}",
            calls.load(Ordering::SeqCst)
        );
    }

    /// Phase 0.5 `(A OK, B Rejected)` returns the cap. The B
    /// rejection is the textbook "upstream confirms the boundary"
    /// signal: `N` is the largest value the upstream accepts.
    #[tokio::test]
    async fn phase_0_5_validation_succeeds_when_a_ok_b_rejected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            accept_up_to: 131_072, // cap = 131_072
            body_for_cap: Some(CAP_BODY_QWEN.to_owned()),
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 131_072);
        // 1 (Phase 0) + 2 (A and B) = 3 calls.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Phase 0.5 `(A OK, B OK)` returns the cap as a known floor.
    /// The upstream reported `N` as its supported cap in the body
    /// but actually accepts more; we still use `N` because it is
    /// a safe lower-bound value the upstream definitely accepts.
    #[tokio::test]
    async fn phase_0_5_validation_succeeds_when_both_ok() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            accept_up_to: u32::MAX, // accept everything; cap is just a floor
            body_for_cap: Some(CAP_BODY_QWEN.to_owned()),
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 131_072);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Phase 0 falls back to Phase 1 when the body is not
    /// parseable (generic 4xx without the cap signature).
    /// The algorithm walks the full exponential + bisect path
    /// and discovers the upstream boundary by binary search.
    #[tokio::test]
    async fn phase_0_falls_back_to_phase1_when_body_unparseable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            accept_up_to: 8_192,
            body_for_cap: None, // first call returns generic 400
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        // Binary search converges near 8_192.
        assert!((8_000..=8_192).contains(&got), "got {got}");
        // Phase 0 fired one probe (unparseable); Phase 1 + Phase 2
        // then ran the full algorithm.
        let n = calls.load(Ordering::SeqCst);
        assert!(
            n > 3,
            "Phase 0 + 0.5 must fall through to Phase 1 (n > 3), got {n}"
        );
    }

    /// Phase 0 falls back to Phase 1 when the candidate cap is
    /// rejected by the upstream (the parse lied). The algorithm
    /// walks Phase 1 + Phase 2 from scratch.
    #[tokio::test]
    async fn phase_0_falls_back_to_phase1_when_a_rejected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            // Body reports 131_072 as the cap, but the upstream
            // only accepts up to 4_096. A_rejected triggers the
            // fallback to Phase 1.
            accept_up_to: 4_096,
            body_for_cap: Some(CAP_BODY_QWEN.to_owned()),
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert!((4_000..=4_096).contains(&got), "got {got}");
        let n = calls.load(Ordering::SeqCst);
        assert!(
            n > 3,
            "Phase 0 + 0.5 must fall through to Phase 1 (n > 3), got {n}"
        );
    }

    /// The Phase 0 callback fires once when Phase 0 / 0.5
    /// succeeds, and never when Phase 1 takes over. Pin the
    /// contract so the caller can rely on the callback as the
    /// signal for "ceiling was parsed from the body".
    #[tokio::test]
    async fn phase_0_callback_fires_on_short_circuit() {
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            accept_up_to: u32::MAX,
            body_for_cap: Some(CAP_BODY_QWEN.to_owned()),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let captured: std::sync::Arc<std::sync::Mutex<Option<u32>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_cb = std::sync::Arc::clone(&captured);
        let _ = detect_max_tokens_with_phase0_cap_callback(
            t,
            MIN_AUTOPROBE_FLOOR,
            MAX_AUTOPROBE_CEILING,
            move |cap| {
                if let Ok(mut g) = captured_for_cb.lock() {
                    *g = Some(cap);
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(*captured.lock().unwrap(), Some(131_072));
    }

    /// The Phase 0 callback stays `None` when Phase 1 takes over
    /// (the body was unparseable). This pins the negative case so
    /// `probe_and_store` never persists a `ceiling` value the
    /// upstream never confirmed.
    #[tokio::test]
    async fn phase_0_callback_does_not_fire_when_falling_back() {
        let t: Arc<dyn ProbeTransport> = Arc::new(BodyMockTransport {
            accept_up_to: 8_192,
            body_for_cap: None, // generic 400 → no parseable cap
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let captured: std::sync::Arc<std::sync::Mutex<Option<u32>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_cb = std::sync::Arc::clone(&captured);
        let _ = detect_max_tokens_with_phase0_cap_callback(
            t,
            MIN_AUTOPROBE_FLOOR,
            MAX_AUTOPROBE_CEILING,
            move |cap| {
                if let Ok(mut g) = captured_for_cb.lock() {
                    *g = Some(cap);
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(*captured.lock().unwrap(), None);
    }

    /// Phase 0.5 retries `B` once when `B` is `Indeterminate`.
    /// The retry is gated on the outcome (not a timer) so a clean
    /// `Rejected`/`Accepted` after the first probe does not pay
    /// the second round-trip. On the second `Indeterminate` the
    /// helper returns the candidate as best-effort.
    #[tokio::test]
    async fn phase_0_5_retries_b_on_indeterminate() {
        // Custom transport: Phase 0 returns the parseable body,
        // A accepts, B returns Indeterminate twice (retry path).
        struct FlakyPhase05Transport {
            b_calls: AtomicUsize,
        }
        #[async_trait]
        impl ProbeTransport for FlakyPhase05Transport {
            async fn probe_send(&self, n: u32) -> ProbeOutcome {
                self.probe_send_with_body(n).await.outcome
            }

            async fn probe_send_with_body(&self, n: u32) -> ProbeResult {
                if n == u32::MAX {
                    ProbeResult {
                        outcome: ProbeOutcome::Rejected,
                        body: CAP_BODY_QWEN.to_owned(),
                    }
                } else if n == 131_072 {
                    // A: accept.
                    ProbeResult {
                        outcome: ProbeOutcome::Accepted,
                        body: String::new(),
                    }
                } else if n == 131_073 {
                    // B: Indeterminate twice (retry path).
                    let count = self.b_calls.fetch_add(1, Ordering::SeqCst);
                    let _ = count;
                    ProbeResult {
                        outcome: ProbeOutcome::Indeterminate,
                        body: String::new(),
                    }
                } else {
                    ProbeResult {
                        outcome: ProbeOutcome::Accepted,
                        body: String::new(),
                    }
                }
            }
        }
        let t: Arc<dyn ProbeTransport> = Arc::new(FlakyPhase05Transport {
            b_calls: AtomicUsize::new(0),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        // Best-effort: 131_072 is the discovered value even when
        // the B retry stays Indeterminate.
        assert_eq!(got, 131_072);
    }

    // -----------------------------------------------------------------
    // Phase 0 / Phase 0.5 test fixtures. Consts and a mock
    // transport live INSIDE `mod tests` so the `#[cfg(test)]`
    // gate excludes them from non-test builds (no dead-code
    // warnings in `cargo build --release`).
    // -----------------------------------------------------------------

    /// Canonical Anthropic-compat error body for MiniMax-M3 (cap =
    /// 524_288). Shared across the Phase 0 tests so the bodies
    /// match the upstream responses captured in production logs.
    const CAP_BODY_MINIMAX_M3: &str = r#"{"type":"error","error":{"message":"model[MiniMax-M3] does not support max tokens > 524288 (2013)"}}"#;

    /// Canonical Anthropic-compat error body for qwen3.8-max /
    /// longcat (cap = 131_072). Shared across the Phase 0 tests.
    const CAP_BODY_QWEN: &str = r#"{"type":"error","error":{"message":"model[qwen3.8-max] does not support max tokens > 131072"}}"#;

    /// Transport that records every call and returns a controllable
    /// body / outcome pair. Used by the Phase 0 tests above.
    #[derive(Clone)]
    struct BodyMockTransport {
        /// Cap above which `probe_send` returns `Rejected` (with
        /// the canonical max-tokens rejection body). `u32::MAX`
        /// means every value is accepted.
        accept_up_to: u32,
        /// Body returned by the FIRST probe only (regardless of
        /// `max_tokens`). `None` returns a generic 4xx without
        /// the cap signature (so Phase 0 falls back to Phase 1).
        body_for_cap: Option<String>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProbeTransport for BodyMockTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            self.probe_send_with_body(max_tokens).await.outcome
        }

        async fn probe_send_with_body(&self, max_tokens: u32) -> ProbeResult {
            let call_idx = self.calls.fetch_add(1, Ordering::SeqCst);
            // The first call (Phase 0 with max_tokens = u32::MAX)
            // returns the configured body regardless of value,
            // when one is set. Otherwise it returns a generic
            // 400 that does not parse.
            if call_idx == 0 {
                return match &self.body_for_cap {
                    Some(body) => ProbeResult {
                        outcome: ProbeOutcome::Rejected,
                        body: body.clone(),
                    },
                    None => ProbeResult {
                        outcome: ProbeOutcome::Rejected,
                        body: r#"{"error":{"message":"internal error"}}"#.to_owned(),
                    },
                };
            }
            // Subsequent calls follow the cap.
            if max_tokens <= self.accept_up_to {
                ProbeResult {
                    outcome: ProbeOutcome::Accepted,
                    body: String::new(),
                }
            } else {
                ProbeResult {
                    outcome: ProbeOutcome::Rejected,
                    body: r#"{"type":"error","error":{"message":"max_tokens > cap"}}"#.to_owned(),
                }
            }
        }
    }

    // `Phase0AwareRecordingTransport` lives near the top of this
    // `mod tests` block so the PR-473 tests and the helper live
    // close together; see the definition above.
}
