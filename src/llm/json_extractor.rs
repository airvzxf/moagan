//! Tolerant JSON extractor for LLM responses that don't honor
//! `response_format: json_object` (Path B in
//! `docs/research-json-structured-output.md`).
//!
//! Multi-pass: skip JS comments in place, skip prose prefix/suffix,
//! brace-balance, then return the substring that parses as JSON.
//! Indices returned are byte offsets into the original input so the
//! caller can slice `&input[start..end]` without remapping.

use serde::de::DeserializeOwned;
use thiserror::Error;

use super::control_tokens;
use super::json_strategy::JsonRecoveryStrategy;
use super::wire::Request;

/// Errors returned by [`extract_tolerant_json`] and [`extract_and_parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractError {
    /// No `{` or `[` delimiter was found in the input (after skipping
    /// JS comments).
    NoJsonFound,
    /// A JSON-looking delimiter was found but its matching close brace
    /// never appeared within the input (truncated or malformed input).
    UnbalancedBraces,
    /// The extracted substring did not parse as JSON. Carries the
    /// `serde_json` error message.
    ParseFailed(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoJsonFound => write!(f, "no JSON delimiter found in input"),
            Self::UnbalancedBraces => write!(f, "unbalanced braces in candidate JSON"),
            Self::ParseFailed(m) => write!(f, "JSON parse failed: {m}"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Returns the byte range `[start, end)` of the first JSON value in
/// `input`. The returned range can be sliced directly against `input`
/// without remapping because the scan walks the original bytes and
/// only advances the cursor past JS comments / prose.
///
/// # Expected pre-processing
///
/// This function only *selects* a substring; it never rewrites the
/// input, precisely so the returned offsets stay valid against the
/// caller's own string. Two sanitising passes therefore run in the
/// caller, ahead of this one — see
/// [`crate::phases::util::parse_model_json_traced`]:
///
/// 1. [`crate::llm::control_tokens::strip_chat_template_tokens`] —
///    chat-template markers (`<|im_start|>`, `<system>`, `[BOS]`,
///    …). Without it, a leading `[BOS]` is indistinguishable from a
///    JSON array delimiter: the scan below locks onto the `[`, finds
///    the bare word `BOS` where a value should be, and reports
///    [`ExtractError::UnbalancedBraces`].
/// 2. [`crate::llm::control_tokens::strip`] — ASCII control bytes
///    and DEL.
///
/// The passes this function still performs itself, in order: strip a
/// leading UTF-8 BOM, skip JS comments and prose prefix, then
/// brace-balance to the matching close delimiter.
pub fn extract_tolerant_json(input: &str) -> Result<(usize, usize), ExtractError> {
    let bytes = input.as_bytes();
    // Strip an optional leading UTF-8 BOM if present.
    let offset = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let (start, open_char, after_open) =
        find_first_json_delim(bytes, offset).ok_or(ExtractError::NoJsonFound)?;
    if !looks_like_json_after_open(bytes, after_open, open_char) {
        return Err(ExtractError::UnbalancedBraces);
    }
    let abs_end =
        find_balanced_end(bytes, after_open, open_char).ok_or(ExtractError::UnbalancedBraces)?;
    Ok((start, abs_end))
}

/// Convenience: extract and parse into T.
pub fn extract_and_parse<T: DeserializeOwned>(input: &str) -> Result<T, ExtractError> {
    let (start, end) = extract_tolerant_json(input)?;
    let slice = &input[start..end];
    serde_json::from_str(slice).map_err(|e| ExtractError::ParseFailed(e.to_string()))
}

/// Advance `i` past any JS comment that starts at position `i` (or
/// any position immediately after the initial `/`). Updates `in_string`
/// if a string boundary is crossed. Returns `true` when a comment was
/// consumed and `i` was advanced, `false` when `i` did not point at a
/// comment opener.
fn skip_js_comment(bytes: &[u8], i: &mut usize, in_string: &mut bool) -> bool {
    if *in_string || *i + 1 >= bytes.len() || bytes[*i] != b'/' {
        return false;
    }
    match bytes[*i + 1] {
        b'/' => {
            let mut j = *i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            *i = j;
            true
        }
        b'*' => {
            let mut j = *i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            *i = (j + 2).min(bytes.len());
            true
        }
        _ => false,
    }
}

/// Find the first JSON delimiter (`{` or `[`) in `input`. Returns
/// `(start_byte, open_char, byte_after_open)` if one is found.
/// The delimiter must be at position 0 or preceded by whitespace
/// (a defensive guard against false positives like `use {curly}`).
fn find_first_json_delim(bytes: &[u8], start: usize) -> Option<(usize, char, usize)> {
    let mut i = start;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        let ch = b as char;
        if skip_js_comment(bytes, &mut i, &mut in_string) {
            continue;
        }
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }
        if b == b'{' || b == b'[' {
            let delim_char = b as char;
            let delim_len = delim_char.len_utf8();
            // Require the delim to be at position 0 or preceded by whitespace,
            // so prose like "use {curly} braces" does not match. Also
            // accept a delim that sits exactly at the scan start (e.g.
            // right after a stripped UTF-8 BOM).
            let preceded_by_whitespace = if i <= start {
                true
            } else {
                let prev = bytes[i - 1] as char;
                prev.is_whitespace()
            };
            if preceded_by_whitespace {
                return Some((i, delim_char, i + delim_len));
            }
        }
        i += 1;
    }
    None
}

/// Look at the first non-whitespace byte after the open delim and
/// decide whether it could plausibly start a JSON value. This is a
/// cheap false-positive filter so that prose such as `{curly}` (where
/// the body is a bare word) is rejected before we attempt brace
/// balancing.
fn looks_like_json_after_open(bytes: &[u8], start: usize, open: char) -> bool {
    let mut i = start;
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return false;
    }
    let b = bytes[i];
    match open {
        '{' => b == b'"' || b == b'{' || b == b'}',
        '[' => {
            matches!(
                b,
                b'"' | b'{' | b'[' | b']' | b'-' | b'+' | b'.' | b't' | b'f' | b'n'
            ) || b.is_ascii_digit()
        }
        _ => false,
    }
}

/// Brace-balance the input starting from position `start` (which sits
/// just after the opening delimiter). `open` is the opening char
/// (`{` or `[`). Returns the byte index just past the matching close
/// delimiter, or `None` if the input is unbalanced.
fn find_balanced_end(bytes: &[u8], start: usize, open: char) -> Option<usize> {
    let close = match open {
        '{' => '}',
        '[' => ']',
        _ => return None,
    };
    let mut depth: i32 = 1;
    let mut i = start;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        if skip_js_comment(bytes, &mut i, &mut in_string) {
            continue;
        }
        let b = bytes[i];
        let ch = b as char;
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + ch.len_utf8());
            }
        }
        i += 1;
    }
    None
}

/// Strategy-aware parse error. Returned by [`parse_with_strategy`]
/// when every recovery pass that the chosen strategy enables has
/// been exhausted. The variant encodes which strategy was active
/// so the post-execution review can tell at a glance whether a
/// failure is "model produced malformed JSON on a path that has no
/// recovery" (`Strict`) or "every recovery pass the strategy
/// enables failed" (`Lenient`).
///
/// This is intentionally distinct from
/// [`crate::phases::util::ParseError`]: the latter is the
/// legacy "all strategies in the recovery chain failed" enum
/// used by [`crate::phases::util::parse_json_with_recovery`].
/// Adding the strategy here keeps the recovery decision
/// self-contained in the [`parse_with_strategy`] wrapper and
/// avoids a `phases → llm` circular dependency.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// The strict strategy's direct parse failed. No tolerant
    /// extraction, no m3 repair, no retry — the wrapper hands
    /// back the original `serde_json` error verbatim so the
    /// caller can surface it as `Error::SchemaViolation`.
    #[error("strict parse failed: {0}")]
    Strict(String),
    /// The lenient / continuation / prompt-prefill strategy's
    /// full recovery chain failed. The recovery chain runs
    /// [`extract_tolerant_json`] (Path B) followed by the m3
    /// repair pipeline; this variant fires when every pass
    /// failed. The original error message is preserved for the
    /// caller's diagnostic.
    #[error("lenient recovery failed: {0}")]
    Lenient(String),
}

impl ParseError {
    /// Borrow the underlying error message. Used by the phase
    /// layer's retry loop to enrich the warning payload without
    /// having to match on the variant first.
    pub fn message(&self) -> &str {
        match self {
            Self::Strict(s) | Self::Lenient(s) => s,
        }
    }
}

/// Strategic wrapper over the existing tolerant-extraction and
/// m3-repair chain. The wrapper decides which subset of the
/// recovery pipeline runs based on `strategy`:
///
/// - [`Strict`](JsonRecoveryStrategy::Strict) — direct parse
///   only. If the response is not valid JSON, return
///   [`ParseError::Strict`] immediately. The other helpers
///   ([`extract_tolerant_json`], the m3 repair pipeline) are
///   NOT consulted; use this for models that already honour
///   `response_format: json_object` strictly.
/// - [`Lenient`](JsonRecoveryStrategy::Lenient) — the full
///   recovery chain: code-fence strip → direct parse → control-
///   token strip → tolerant extraction (PR-C3 iterative
///   brackets + PR-C4 chat-template strip) → m3 repair on the
///   extracted candidate → m3 repair on the full input. If
///   every pass fails, return [`ParseError::Lenient`].
/// - [`Continuation`](JsonRecoveryStrategy::Continuation) —
///   same as `Lenient` for the single parse attempt; the
///   continuation re-call itself lives in
///   [`crate::phases::phase::RunContext::call_with_retry_parse`]
///   and is driven by
///   [`crate::llm::json_strategy::max_continuation_attempts`].
/// - [`PromptPrefill`](JsonRecoveryStrategy::PromptPrefill) —
///   same as `Lenient` for the single parse attempt; the
///   prefill retry lives in the dispatcher (also in
///   `call_with_retry_parse`) and is driven by
///   [`crate::llm::json_strategy::needs_assistant_prefill`].
///
/// The wrapper is `async` so the dispatcher can call it without
/// a separate sync-vs-async boundary; the body has no `.await`
/// calls because the recovery chain is purely synchronous.
/// `model` and `request` are kept on the signature for future
/// diagnostic use (e.g. logging which model / role produced a
/// specific failure); today they are unused.
pub async fn parse_with_strategy<T: DeserializeOwned>(
    strategy: JsonRecoveryStrategy,
    model: &str,
    request: &Request,
    raw: &str,
) -> Result<T, ParseError> {
    let _ = model;
    let _ = request;
    match strategy {
        JsonRecoveryStrategy::Strict => parse_strict(raw),
        JsonRecoveryStrategy::Lenient
        | JsonRecoveryStrategy::Continuation
        | JsonRecoveryStrategy::PromptPrefill => parse_lenient(raw),
    }
}

/// Direct parse only. Returns [`ParseError::Strict`] with the
/// underlying `serde_json` error verbatim if the payload is not
/// valid JSON.
fn parse_strict<T: DeserializeOwned>(raw: &str) -> Result<T, ParseError> {
    match serde_json::from_str::<T>(raw) {
        Ok(v) => Ok(v),
        Err(e) => Err(ParseError::Strict(e.to_string())),
    }
}

/// Full recovery chain. Mirrors the order
/// [`crate::phases::util::parse_model_json_traced`] runs, but
/// without the per-pass `FnMut(RepairEvent)` sink — the wrapper
/// emits a single `ParseError::Lenient` on the failure path so
/// the caller's retry loop does not have to drive a sink. The
/// dispatcher still emits its own `model.json_repair_applied`
/// warnings via the existing
/// [`crate::phases::phase::RunContext::parse_model_json`] helper
/// when the user-facing parse path is used; this wrapper is for
/// the strategy-aware retry loop, which does not need the per-
/// pass warning stream because the chain runs at most once per
/// retry attempt.
///
/// Note: `phases::util::parse_model_json_traced` already imports
/// the tolerant extractor and m3 repair helpers from
/// `llm::control_tokens` / `llm::json_extractor`. Reusing it here
/// is a deliberate cycle at the module level — both modules
/// expose functions to each other — but the call graph never
/// recurses: `parse_lenient` calls `parse_model_json_traced`,
/// which calls `extract_tolerant_json` / m3 repair, none of
/// which call back into `parse_lenient`.
fn parse_lenient<T: DeserializeOwned>(raw: &str) -> Result<T, ParseError> {
    let _ = control_tokens::strip_response_text;
    let _ = extract_tolerant_json;
    // Reuse the existing traced chain with a no-op sink. The
    // dispatcher does not need per-pass repair warnings on this
    // path because the wrapper fires at most once per retry
    // attempt — emitting one warning per recovery pass would
    // multiply warnings by the retry count.
    crate::phases::util::parse_model_json_traced::<T, _>(raw, |_event| {})
        .map_err(|e| ParseError::Lenient(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::control_tokens;
    use serde::Deserialize;

    #[test]
    fn extract_tolerant_json_finds_object_in_prose_prefix() {
        let input = "Sure! Here is the JSON you requested:\n{\"answer\": 42}";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "{\"answer\": 42}");
    }

    /// A ChatML-wrapped payload run through the response-path strip
    /// and then the extractor. The `<|im_start|>` / `<system>`
    /// markers become spaces, the prose between them is dropped as
    /// an ordinary prose prefix, and the inner object comes back
    /// intact.
    ///
    /// The markers in *this* payload are ones the extractor already
    /// tolerated before the strip existed (they contain no `[`, so
    /// nothing hijacks the delimiter scan) — the case that genuinely
    /// needed the strip is the bracket family, covered by
    /// `extract_tolerant_json_needs_strip_for_bracket_tokens` below.
    #[test]
    fn extract_tolerant_json_handles_chat_template_wrapped_response() {
        let raw = "<|im_start|>\n<system>some prose</system>\n{\"x\":\"v\"}<|im_end|>";
        let cleaned = control_tokens::strip_response_text(raw);
        let (start, end) = extract_tolerant_json(&cleaned).unwrap();
        let value: serde_json::Value = serde_json::from_str(&cleaned[start..end]).unwrap();
        assert_eq!(value, serde_json::json!({ "x": "v" }));
    }

    /// Regression lock on the case the strip exists for: `[BOS]`
    /// reads as a JSON array delimiter, so without the strip the
    /// extractor locks onto it and fails. Both spellings — canonical
    /// and the `[BO S]` typo — are covered.
    #[test]
    fn extract_tolerant_json_needs_strip_for_bracket_tokens() {
        for raw in ["[BOS]{\"x\":\"v\"}[EOS]", "[BO S]{\"x\":\"v\"}[EOS ]"] {
            // Without the strip the delimiter scan is hijacked.
            assert_eq!(
                extract_tolerant_json(raw).unwrap_err(),
                ExtractError::UnbalancedBraces
            );
            // With it, the inner object is recovered.
            let cleaned = control_tokens::strip_response_text(raw);
            let (start, end) = extract_tolerant_json(&cleaned).unwrap();
            let value: serde_json::Value = serde_json::from_str(&cleaned[start..end]).unwrap();
            assert_eq!(value, serde_json::json!({ "x": "v" }));
        }
    }

    #[test]
    fn extract_tolerant_json_finds_object_in_prose_suffix() {
        let input = "{\"answer\": 42}\nHope that helps!";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "{\"answer\": 42}");
    }

    #[test]
    fn extract_tolerant_json_strips_js_line_comment() {
        let input = "// header comment\n{\"answer\": 42}";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "{\"answer\": 42}");
    }

    #[test]
    fn extract_tolerant_json_strips_js_block_comment() {
        let input = "/* block */\n{\"answer\": 42}";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "{\"answer\": 42}");
    }

    #[test]
    fn extract_tolerant_json_handles_truncated_brace() {
        let input = "{\"answer\": 42";
        let err = extract_tolerant_json(input).unwrap_err();
        assert_eq!(err, ExtractError::UnbalancedBraces);
    }

    #[test]
    fn extract_tolerant_json_handles_empty_string() {
        let err = extract_tolerant_json("").unwrap_err();
        assert_eq!(err, ExtractError::NoJsonFound);
    }

    #[test]
    fn extract_tolerant_json_handles_nested_objects() {
        let input = "prefix {\"outer\": {\"inner\": {\"deep\": 1}}} suffix";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(
            &input[start..end],
            "{\"outer\": {\"inner\": {\"deep\": 1}}}"
        );
    }

    #[test]
    fn extract_tolerant_json_handles_array_of_objects() {
        let input = "Here is the list:\n[{\"a\": 1}, {\"a\": 2}]";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "[{\"a\": 1}, {\"a\": 2}]");
    }

    #[test]
    fn extract_tolerant_json_preserves_escaped_quotes_in_strings() {
        let input = "{\"msg\": \"He said \\\"hi\\\" to me\"}";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(
            &input[start..end],
            "{\"msg\": \"He said \\\"hi\\\" to me\"}"
        );
    }

    #[test]
    fn extract_tolerant_json_handles_unicode() {
        let input = "{\"greeting\": \"こんにちは\"}";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "{\"greeting\": \"こんにちは\"}");
    }

    #[test]
    fn extract_tolerant_json_rejects_brace_in_prose_sentence() {
        let input = "Use {curly} braces in Rust.";
        let err = extract_tolerant_json(input).unwrap_err();
        assert_eq!(err, ExtractError::UnbalancedBraces);
    }

    #[test]
    fn extract_and_parse_deserializes_valid_json() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct Out {
            answer: i32,
        }
        let input = "noise {\"answer\": 42} tail";
        let parsed: Out = extract_and_parse(input).unwrap();
        assert_eq!(parsed, Out { answer: 42 });
    }

    #[test]
    fn extract_and_parse_returns_parse_error_on_invalid_json() {
        let input = "{\"answer\": \"not a number\"}";
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        struct Out {
            answer: i32,
        }
        let err = extract_and_parse::<Out>(input).unwrap_err();
        assert!(matches!(err, ExtractError::ParseFailed(_)));
    }

    #[test]
    fn extract_tolerant_json_handles_bom() {
        let input = "\u{feff}{\"answer\": 42}";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "{\"answer\": 42}");
    }

    #[test]
    fn extract_tolerant_json_handles_array_root() {
        let input = "answer list: [1, 2, 3]";
        let (start, end) = extract_tolerant_json(input).unwrap();
        assert_eq!(&input[start..end], "[1, 2, 3]");
    }

    // --- parse_with_strategy wrapper tests -------------------------

    use super::{ParseError, parse_with_strategy};
    use crate::llm::role::Role;
    use crate::llm::wire::{Message, Request};

    fn stub_request(model: &str) -> Request {
        Request {
            role: Role::Sketch,
            model: model.to_owned(),
            system: String::new(),
            user: String::new(),
            max_tokens: 0,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
        }
    }

    #[tokio::test]
    async fn strict_strategy_passes_well_formed_json() {
        let raw = r#"{"answer": 42}"#;
        let parsed: serde_json::Value = parse_with_strategy(
            JsonRecoveryStrategy::Strict,
            "gpt-5.6-luna",
            &stub_request("gpt-5.6-luna"),
            raw,
        )
        .await
        .unwrap();
        assert_eq!(parsed, serde_json::json!({"answer": 42}));
    }

    #[tokio::test]
    async fn strict_strategy_rejects_prose_prefix_directly() {
        // Strict skips tolerant extraction. Prose-prefixed JSON
        // would normally be recovered by the Lenient pipeline;
        // Strict surfaces the parse error verbatim so the
        // post-execution review can see when a strict model
        // genuinely broke the contract.
        let raw = "Sure, here you go: {\"answer\": 42}";
        let err = parse_with_strategy::<serde_json::Value>(
            JsonRecoveryStrategy::Strict,
            "gpt-5.6-luna",
            &stub_request("gpt-5.6-luna"),
            raw,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ParseError::Strict(_)),
            "Strict must return ParseError::Strict on direct parse failure, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn lenient_strategy_recovers_prose_prefixed_json() {
        // Lenient runs the full recovery chain — direct parse
        // fails, tolerant extraction succeeds.
        let raw = "Sure, here you go: {\"answer\": 42}";
        let parsed: serde_json::Value = parse_with_strategy(
            JsonRecoveryStrategy::Lenient,
            "kimi-k3",
            &stub_request("kimi-k3"),
            raw,
        )
        .await
        .unwrap();
        assert_eq!(parsed, serde_json::json!({"answer": 42}));
    }

    #[tokio::test]
    async fn lenient_strategy_recovers_chat_template_wrapped_payload() {
        // PR-C4: chat-template markers (`[BOS]`, `[EOS]`, …)
        // confuse the tolerant extractor until the control-token
        // strip runs first. Lenient's chain applies the strip
        // before the tolerant extraction.
        let raw = "\n[BOS]{\"answer\": 42}[EOS]\n";
        let parsed: serde_json::Value = parse_with_strategy(
            JsonRecoveryStrategy::Lenient,
            "kimi-k2.7-code",
            &stub_request("kimi-k2.7-code"),
            raw,
        )
        .await
        .unwrap();
        assert_eq!(parsed, serde_json::json!({"answer": 42}));
    }

    #[tokio::test]
    async fn lenient_strategy_returns_parse_error_when_chain_exhausted() {
        // Genuinely-broken JSON: the recovery chain cannot turn
        // the input into valid JSON, so Lenient returns
        // ParseError::Lenient.
        let raw = "this is not json at all";
        let err = parse_with_strategy::<serde_json::Value>(
            JsonRecoveryStrategy::Lenient,
            "kimi-k3",
            &stub_request("kimi-k3"),
            raw,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ParseError::Lenient(_)),
            "Lenient must return ParseError::Lenient on chain exhaustion, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn continuation_strategy_runs_lenient_chain_for_single_parse() {
        // The Continuation strategy runs Lenient for the single
        // parse attempt; the continuation re-call lives in
        // `phases::phase::RunContext::call_with_retry_parse`.
        // A well-formed payload must parse identically to Lenient.
        let raw = r#"{"answer": 42}"#;
        let parsed: serde_json::Value = parse_with_strategy(
            JsonRecoveryStrategy::Continuation,
            "minimax-m3",
            &stub_request("minimax-m3"),
            raw,
        )
        .await
        .unwrap();
        assert_eq!(parsed, serde_json::json!({"answer": 42}));
    }

    #[tokio::test]
    async fn prompt_prefill_strategy_runs_lenient_chain_for_single_parse() {
        // The PromptPrefill strategy runs Lenient for the single
        // parse attempt; the prefill re-call lives in
        // `phases::phase::RunContext::call_with_retry_parse`.
        let raw = r#"{"answer": 42}"#;
        let parsed: serde_json::Value = parse_with_strategy(
            JsonRecoveryStrategy::PromptPrefill,
            "deepseek-v4-pro",
            &stub_request("deepseek-v4-pro"),
            raw,
        )
        .await
        .unwrap();
        assert_eq!(parsed, serde_json::json!({"answer": 42}));
    }

    #[tokio::test]
    async fn parse_error_message_helper_returns_underlying_error() {
        // The dispatcher converts ParseError → Error::SchemaViolation
        // via the `message()` helper. Pin the contract so the
        // helper stays total across both variants.
        let strict = ParseError::Strict("oops".to_owned());
        assert_eq!(strict.message(), "oops");
        let lenient = ParseError::Lenient("also oops".to_owned());
        assert_eq!(lenient.message(), "also oops");
    }

    #[tokio::test]
    async fn request_extra_messages_field_is_accepted_but_unused() {
        // The wrapper keeps `request` as a future-proofing
        // parameter; today the parse chain does not inspect it.
        // A request that carries an `extra_messages` payload
        // must NOT change the parse result.
        let raw = r#"{"answer": 42}"#;
        let mut req = stub_request("deepseek-v4-flash");
        req.extra_messages = vec![Message {
            role: "assistant".into(),
            content: "{".into(),
        }];
        let parsed: serde_json::Value =
            parse_with_strategy(JsonRecoveryStrategy::Strict, "deepseek-v4-flash", &req, raw)
                .await
                .unwrap();
        assert_eq!(parsed, serde_json::json!({"answer": 42}));
    }
}
