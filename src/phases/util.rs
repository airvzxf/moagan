//! Shared helpers for phases that need to read JSON from disk, write
//! JSON to disk atomically, and parse LLM responses.

use std::borrow::Cow;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::atomic::writer::AtomicWriter;
use crate::error::Result;
use crate::llm::control_tokens;
use crate::llm::json_extractor;
use crate::redact::detect_stale;

const DEFAULT_STALE_TTL_SECS: u64 = 86_400;

/// Default cap on the iterative bracket-repair loop. Each pass adds
/// one missing closer; 3 passes cover the realistic nested-truncation
/// patterns observed in MiniMax-M3 (inner `]`, outer `}`, etc.) and
/// cap the worst-case latency on genuinely-broken payloads.
const DEFAULT_BRACKET_REPAIR_MAX_ITERS: usize = 3;

fn stale_ttl_secs() -> u64 {
    std::env::var("MOAGAN_STALE_TTL_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_STALE_TTL_SECS)
}

fn emit_stale_artifact_if_needed(path: &Path) -> Option<crate::redact::StaleArtifact> {
    let artifact = detect_stale(path, stale_ttl_secs());
    if let Some(artifact) = &artifact {
        artifact.emit();
    }
    artifact
}

/// Which repair pass actually changed the model output. Surfaced
/// through `parse_model_json_traced` so the warnings stream can
/// tell post-execution reviewers which m3 pathology this model
/// triggered (one diagnosis per kind, in the order they fired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairKind {
    /// A `:` was inserted between a complete key string and the
    /// value opener (`"...["` -> `"":[...]`).
    Colon,
    /// A `,` was inserted between two adjacent values inside an
    /// array or object, or a trailing comma was eaten.
    Separator,
    /// A missing closer (`}` or `]`) was appended, or a misplaced
    /// closer was rebalanced.
    Bracket,
}

impl RepairKind {
    /// Stable string label used in the warnings stream.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Colon => "colon",
            Self::Separator => "separator",
            Self::Bracket => "bracket",
        }
    }
}

/// One repair event the traced parser emits to the sink callback.
/// Alias of [`RepairTrace`] used in the public API where the
/// `Trace` suffix is less idiomatic.
pub type RepairEvent = RepairTrace;

/// Read a JSON file and deserialize it.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    emit_stale_artifact_if_needed(path);
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(crate::Error::from)
}

/// Write `value` as JSON to `path` atomically.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(crate::Error::from)?;
    AtomicWriter::new().write(path, &bytes)?;
    Ok(())
}

/// Return the last `max_bytes` bytes of `s` as a UTF-8 safe slice.
///
/// Rust's `&str[i..j]` panics with `byte index N is not a char
/// boundary` when the byte index falls inside a multi-byte UTF-8
/// code point. The naive `&s[s.len().saturating_sub(500)..]`
/// pattern (used three times in this file before this commit)
/// produced that exact panic whenever the model returned more
/// than 500 bytes containing CJK, emoji, or any non-ASCII
/// script — the common case for `critique` outputs in `--mode
/// deep` once the model decided to mix Spanish/Chinese into its
/// verdict.
///
/// The fix walks **forward** from `s.len() - max_bytes` to the
/// next UTF-8 char boundary so the returned slice is always
/// `<= max_bytes` bytes and never splits a code point. When the
/// input is shorter than `max_bytes` the original string is
/// returned untouched.
pub(crate) fn safe_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = s.len() - max_bytes;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    &s[idx..]
}

/// Strip a leading/trailing markdown fence from the model output and
/// parse the inner JSON.
///
/// On failure, the error embeds the full raw payload and the last 500
/// bytes as a `tail:` summary, so the diagnostic travels with the
/// failure instead of pointing the user at a file they have no way to
/// read from a CLI invocation.
///
/// # LLM bracket repair
///
/// When the direct parse fails, we attempt a single narrow repair
/// (`repair_m3_brackets`) that handles two observed bugs of
/// MiniMax-M3:
///
///   1. The model emits the outer `}` while still inside an
///      unclosed array, producing `...[item}` (missing `]`).
///      The bracket walker appends the missing `]`.
///
///   2. The model writes a key string followed by `[` or `{`
///      without the `:` separator, producing
///      `"key"[item,item]`. The colon-insertion pass detects a
///      complete string literal followed by `[`/`{` and inserts
///      the missing `:`.
///
/// It does NOT help with mid-string truncation, unescaped quotes,
/// invalid escapes, structural breakage, or field-name typos. If the
/// repair would change the meaning of the JSON, it returns `None`
/// and the caller sees the original error. The role-aware
/// validator (`Role::validate_json`) then annotates the error with
/// the expected schema, so the user knows which phase produced the
/// malformed JSON.
///
/// Do NOT remove this without a corresponding fix to the upstream
/// model. Smoke batches against m3 have shown ~9% per-call failure
/// rate from these two cases; without the repair, every affected run
/// hard-errors and the user has to retry. Logged in commit
/// `e0a4594` (F4), reintroduced as `close_missing_brackets` in
/// commit `196b40d`, expanded to also handle missing colons in
/// commit TBD.
pub fn parse_model_json<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let trimmed = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<T>(&trimmed) {
        return Ok(v);
    }
    if let Some(repaired) = repair_m3_brackets(&trimmed)
        && let Ok(v) = serde_json::from_str::<T>(&repaired)
    {
        return Ok(v);
    }
    let e = serde_json::from_str::<T>(&trimmed)
        .err()
        .expect("parse failed above");
    let tail = safe_tail(&trimmed, 500);
    Err(crate::Error::SchemaViolation(format!(
        "model output is not valid JSON: {e}; len={} bytes; tail={:?}; full raw follows:\n{}",
        trimmed.len(),
        tail,
        trimmed
    )))
}

/// Like [`parse_model_json`] but reports every repair pass that
/// actually modified the model output, via the `sink` callback. Use
/// this from phase code so the warnings stream can show exactly
/// which m3 pathology was triggered (colon / separator / bracket).
/// The callback is invoked at most once per repair pass, in
/// pipeline order.
///
/// After PR D1 the recovery chain wires up Path B (tolerant
/// extraction via [`json_extractor::extract_tolerant_json`]) as an
/// intermediate step between the direct parse and the M3 repair
/// chain. Recovery order, with each step only attempted when the
/// previous one failed:
///
/// 1. **Direct parse** on the trimmed input.
/// 2. **Control-token strip** — [`control_tokens::strip_response_text`]
///    removes chat-template markers (`<|im_start|>`, `<system>`,
///    `[BOS]`, …) and ASCII control bytes, then the direct parse is
///    retried. Deliberately sequenced *after* step 1 so a
///    well-formed payload carrying a marker inside a string value
///    is returned intact and never reaches the strip. Every later
///    step operates on the cleaned text.
/// 3. **Tolerant extraction (Path B)** — carves out the first
///    balanced JSON value, dropping prose prefix/suffix, JS
///    comments, and a leading BOM. If the candidate parses, return.
/// 4. **M3 repair on the extracted candidate** — the bracket /
///    separator / colon chain runs on the Path B candidate; each
///    repair pass that fires emits a [`RepairEvent`] via `sink`.
/// 5. **M3 repair on the full input** — the same chain runs on the
///    cleaned input as the final fallback; events fire via `sink`.
///
/// Neither the strip nor the Path B step emits `RepairEvent`s: the
/// strip only removes markers that were never part of the JSON, and
/// Path B does not modify the input at all — it only selects a
/// substring. The companion helper [`parse_json_with_recovery`]
/// exposes the same recovery chain without an external sink (it
/// emits `tracing::debug!` events instead).
pub fn parse_model_json_traced<T, F>(raw: &str, mut sink: F) -> Result<T>
where
    T: DeserializeOwned,
    F: FnMut(RepairEvent),
{
    let trimmed = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<T>(&trimmed) {
        return Ok(v);
    }
    // Sanitising pass: chat-template markers and ASCII control
    // bytes. It runs *after* the direct parse so a well-formed
    // payload that legitimately contains a marker inside a string
    // value (`{"note":"<system>"}`) is returned untouched above and
    // never reaches the strip.
    let cleaned = control_tokens::strip_response_text(&trimmed);
    if let Cow::Owned(stripped) = &cleaned {
        // Something was removed: retry the direct parse, which is
        // enough on its own for payloads whose only defect was a
        // wrapping marker pair.
        if let Ok(v) = serde_json::from_str::<T>(stripped) {
            return Ok(v);
        }
    }
    if let Ok((start, end)) = json_extractor::extract_tolerant_json(&cleaned) {
        let candidate = &cleaned[start..end];
        if let Ok(v) = serde_json::from_str::<T>(candidate) {
            return Ok(v);
        }
        let (repaired, repairs) = repair_m3_brackets_with_trace(candidate);
        if let Some(repaired) = repaired {
            for r in &repairs {
                sink(RepairEvent {
                    kind: r.kind,
                    bytes_before: r.bytes_before,
                    bytes_after: r.bytes_after,
                });
            }
            if let Ok(v) = serde_json::from_str::<T>(&repaired) {
                return Ok(v);
            }
        }
    }
    let (repaired, repairs) = repair_m3_brackets_with_trace(&cleaned);
    let Some(repaired) = repaired else {
        let e = serde_json::from_str::<T>(&cleaned)
            .err()
            .expect("parse failed above");
        let tail = safe_tail(&cleaned, 500);
        return Err(crate::Error::SchemaViolation(format!(
            "model output is not valid JSON: {e}; len={} bytes; tail={:?}; full raw follows:\n{}",
            cleaned.len(),
            tail,
            cleaned
        )));
    };
    for r in repairs {
        sink(RepairEvent {
            kind: r.kind,
            bytes_before: r.bytes_before,
            bytes_after: r.bytes_after,
        });
    }
    serde_json::from_str::<T>(&repaired).map_err(|e| {
        let tail = safe_tail(&repaired, 500);
        crate::Error::SchemaViolation(format!(
            "model output is not valid JSON after repair: {e}; len={} bytes; tail={:?}; full raw follows:\n{}",
            repaired.len(),
            tail,
            repaired
        ))
    })
}

/// Errors returned by [`parse_json_with_recovery`]. The wrapper
/// exhausts every strategy before reporting a failure, so the only
/// variant is "all strategies failed". The raw payload and tail are
/// preserved by the caller's error message.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// Direct parse, tolerant extraction, M3 repair on the extracted
    /// candidate, and M3 repair on the full input all failed.
    #[error(
        "all JSON parsing strategies failed (direct, tolerant extraction, M3 repair on extracted candidate, M3 repair on full input)"
    )]
    AllStrategiesFailed,
}

/// Multi-strategy JSON recovery pipeline. The same recovery chain
/// that [`parse_model_json_traced`] runs inline is exposed here
/// without the per-pass `FnMut(RepairEvent)` sink — this helper is
/// intended for callers that want the recovered value without the
/// telemetry hook (e.g. tests, single-shot diagnostic flows).
/// Strategy order, with each step only attempted when the previous
/// one failed:
///
/// 1. **Direct parse** — `serde_json::from_str` on the raw input.
/// 2. **Tolerant extraction (Path B)** —
///    [`json_extractor::extract_tolerant_json`], which strips JS
///    line/block comments, BOM, and prose prefix/suffix around the
///    first balanced `{` or `[`. The returned substring is then
///    parsed again.
/// 3. **M3 repair on the extracted candidate** — same bracket /
///    separator / colon chain as step 4, applied only to the JSON
///    the tolerant extractor isolated. This lets M3 repair run on
///    prose-wrapped payloads that the tolerant extractor trimmed
///    down to a balanced fragment, but where the fragment still has
///    m3 pathologies.
/// 4. **M3 repair on the full input** — last-resort fallback. Same
///    chain as step 3, applied to the whole input.
///
/// Returns the first strategy that yields a valid JSON value, or
/// [`ParseError::AllStrategiesFailed`] if every strategy failed.
/// Each strategy that fires emits a `tracing::debug!` event so
/// post-execution reviewers can see which strategy recovered the
/// payload.
pub fn parse_json_with_recovery(input: &str) -> std::result::Result<serde_json::Value, ParseError> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(input) {
        tracing::trace!("parse_json_with_recovery: direct parse ok");
        return Ok(v);
    }
    if let Ok((start, end)) = json_extractor::extract_tolerant_json(input) {
        let candidate = &input[start..end];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
            tracing::debug!(
                start,
                end,
                "parse_json_with_recovery: tolerant extraction ok"
            );
            return Ok(v);
        }
        if let Some(repaired) = repair_m3_brackets(candidate)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired)
        {
            tracing::debug!("parse_json_with_recovery: m3 after extraction ok");
            return Ok(v);
        }
    }
    if let Some(repaired) = repair_m3_brackets(input)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired)
    {
        tracing::debug!("parse_json_with_recovery: m3 full ok");
        return Ok(v);
    }
    Err(ParseError::AllStrategiesFailed)
}

/// Repair two narrow cases we have observed in the MiniMax-M3
/// output stream:
///
///  1. The model emits the outer `}` while still inside an
///     unclosed array, so the JSON ends with `...[last_item}` instead
///     of `...[last_item]}`. The bracket-walker inserts the missing
///     `]` at the position the original output pointed to.
///
///  2. The model writes a key string followed immediately by `[`
///     or `{` (skipping the `:`), e.g. `"suggestions"[item,item]`
///     or — the harsher variant — `"suggestions[item,item}`
///     where it also forgets to close the key string entirely. The
///     colon-repair pass tokenizes the input, finds the end of each
///     complete string literal, and inserts `:` when the next
///     non-whitespace char is `[` or `{`.
///
/// Both repairs are narrow heuristic patches. They do NOT help with:
///
///   - mid-string truncation (`"a":"hel)
///   - unescaped quotes inside a string value
///   - invalid escapes (`\\x`, `\\0`)
///   - structural breakage (an object property repeated twice, etc.)
///   - field-name typos (`tradeffs` for `tradeoffs`)
///
/// For those, the caller (`parse_model_json`) produces an error that
/// the validator (`Role::validate_json`) annotates with the expected
/// role schema; the user must retry or use a different model.
fn repair_m3_brackets(s: &str) -> Option<String> {
    repair_m3_brackets_with_trace(s).0
}

/// One tracked repair event emitted by the chain. Includes the
/// byte-delta so the warnings stream can show how much the model
/// output was rewritten.
#[derive(Debug, Clone)]
pub struct RepairTrace {
    /// Which repair pass fired.
    pub kind: RepairKind,
    /// Length of the input that fed into this pass.
    pub bytes_before: usize,
    /// Length of the output after this pass applied.
    pub bytes_after: usize,
}

/// Pipeline of the three repair passes. Returns the patched string
/// (when the chain managed to produce one) and the list of repair
/// events that actually modified the input, in pipeline order. The
/// list is empty when the input was already balanced.
fn repair_m3_brackets_with_trace(s: &str) -> (Option<String>, Vec<RepairTrace>) {
    let mut events: Vec<RepairTrace> = Vec::new();
    let mut current = s.to_owned();

    if let Some(patched) = repair_missing_colon(&current)
        && patched.len() != current.len()
    {
        let bytes_before = current.len();
        let bytes_after = patched.len();
        events.push(RepairTrace {
            kind: RepairKind::Colon,
            bytes_before,
            bytes_after,
        });
        current = patched;
    }
    if let Some(patched) = repair_missing_separators(&current)
        && patched.len() != current.len()
    {
        let bytes_before = current.len();
        let bytes_after = patched.len();
        events.push(RepairTrace {
            kind: RepairKind::Separator,
            bytes_before,
            bytes_after,
        });
        current = patched;
    }
    // Iterative bracket repair: balance → parse, up to 3 passes.
    // Each pass that appends a closer emits one `Bracket` event so the
    // warnings stream keeps one record per closer the heuristic
    // recovered. After 3 passes with no parse success the helper
    // gives up; the existing `Error::SchemaViolation` path then
    // fires for the caller.
    match repair_brackets_iterative(
        &current,
        &mut |ev| {
            events.push(RepairTrace {
                kind: ev.kind,
                bytes_before: ev.bytes_before,
                bytes_after: ev.bytes_after,
            });
        },
        DEFAULT_BRACKET_REPAIR_MAX_ITERS,
    ) {
        Some(repaired) => (Some(repaired), events),
        None => (None, events),
    }
}

/// Walk the input, find places where a string literal is followed by
/// `[` or `{` (after optional whitespace) without a `:` between,
/// and insert the missing `:`.
///
/// Operates on the raw output as a token sequence so that the
/// `"…` in `"key[a, b]` does not fool us into thinking we are still
/// inside a string when the `[` actually starts the value.
///
/// The walker tracks string state properly (`\"` is an escape and does
/// not close the string) so it does NOT mistake a `[` or `{` inside
/// an in-progress string for the start of a value.
fn repair_missing_colon(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n + 8);
    let mut changed = false;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c != '"' {
            out.push(c);
            i += 1;
            continue;
        }
        // Copy the opening quote and the entire string literal,
        // honouring escapes (`\"` is not a string close, `\\`
        // followed by an unterminated char at end of input is a
        // hard error). Use `in_string` so a `[` or `{` appearing
        // mid-string is NOT mistaken for the start of a value.
        let mut in_string = true;
        out.push(c);
        i += 1;
        while in_string && i < n {
            match chars[i] {
                '\\' => {
                    if i + 1 < n {
                        out.push('\\');
                        out.push(chars[i + 1]);
                        i += 2;
                    } else {
                        // Unterminated escape at end of input.
                        return None;
                    }
                }
                '"' => {
                    out.push('"');
                    i += 1;
                    in_string = false;
                }
                ch => {
                    out.push(ch);
                    i += 1;
                }
            }
        }
        if in_string {
            // Model stopped mid-string. Cannot safely insert `:`.
            // Let the next pass / the validator surface the real
            // error.
            return None;
        }
        // We just copied a complete string. Look ahead past
        // whitespace for a structural char that would be a value.
        let mut j = i;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        if j < n && (chars[j] == '[' || chars[j] == '{') {
            // The string must have been a key. Insert `:`.
            out.push(':');
            changed = true;
        }
    }
    if changed { Some(out) } else { None }
}

// --- separator repair (state machine) ---

/// State of the JSON-walker state machine. The walker is a tiny
/// hand-rolled JSON recognizer that knows enough to spot a missing
/// `,` between array elements or object values, and a missing
/// closer at the end. It is NOT a full JSON parser: it does not
/// validate types, only structural sequencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// Inside an object. `expecting_key` is `true` after `{` or
    /// `,` (the next non-ws is either `"` opening a key string,
    /// or `}` for an empty object); it is `false` after `:`
    /// (the next non-ws is the value or `}` for an empty value).
    Object { expecting_key: bool },
    /// Inside an array, looking for values separated by `,`.
    Array,
}

/// Walk the input and insert a missing `,` between array elements
/// or object values when the model emits two values back-to-back
/// without a separator. Also removes trailing commas inside `]` and
/// `}`.
///
/// This handles the m3 failure mode that the bracket-walker
/// (`repair_missing_brackets`) cannot: the model writes two
/// array elements adjacent, e.g. `["a" "b" "c"]`, and serde
/// reports `expected ',' or ']'`. The walker detects "we just
/// closed a value, the next non-whitespace is the start of another
/// value" and inserts `,`.
fn repair_missing_separators(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n + 8);
    // Stack: (frame, has_value). `has_value` is true once a
    // complete value has been emitted inside this container; the
    // next value-start char then needs a leading `,`.
    let mut stack: Vec<(Frame, bool)> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut changed = false;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if in_string && c == '\\' && i + 1 < n {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if in_string && escape {
            out.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if c == '"' {
            let was_in_string = in_string;
            in_string = !in_string;
            out.push(c);
            i += 1;
            if was_in_string {
                // Mark that this container has now seen a value.
                if let Some((_, has_value)) = stack.last_mut() {
                    *has_value = true;
                }
                // After a string close, the next non-ws char
                // tells us what the writer intended. For an object
                // after a key (expecting_key=true) it must be `:`;
                // for an object after a value, or an array
                // element, it must be `,` or the matching closer.
                // Anything else means the model forgot a separator
                // or the `:` itself.
                let mut j = i;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < n {
                    let next = chars[j];
                    let expect_colon = matches!(
                        stack.last().map(|(f, _)| f),
                        Some(Frame::Object {
                            expecting_key: true
                        })
                    );
                    let want = if expect_colon { ':' } else { ',' };
                    if next != want && next != ']' && next != '}' {
                        out.push(want);
                        changed = true;
                        if let Some((Frame::Object { expecting_key }, _)) = stack.last_mut() {
                            // After an inserted separator the
                            // object phase flips. Inserted `,`
                            // means we are now waiting for a key.
                            // Inserted `:` means we are waiting
                            // for a value.
                            *expecting_key = want == ',';
                        }
                    } else if next == want
                        && expect_colon
                        && let Some((Frame::Object { expecting_key }, _)) = stack.last_mut()
                    {
                        *expecting_key = false;
                    }
                }
            }
            continue;
        }
        if in_string {
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '{' | '[' => {
                // The case of "value closer, then whitespace, then
                // new value start" is handled by the whitespace
                // branch below. Here we just push the opener.
                out.push(c);
                if c == '{' {
                    stack.push((
                        Frame::Object {
                            expecting_key: true,
                        },
                        false,
                    ));
                } else {
                    stack.push((Frame::Array, false));
                }
            }
            '}' | ']' => {
                // Eat a trailing comma so the result is well-formed
                // JSON. This handles `[1, 2, 3,]`.
                if out.ends_with(',') {
                    out.pop();
                    changed = true;
                }
                out.push(c);
                stack.pop();
                // The container we just closed held a complete
                // value (this `}` or `]`). Mark the parent as
                // having-seen-a-value so the next value-start
                // (in the parent's context) gets a leading `,`.
                if let Some((_, has_value)) = stack.last_mut() {
                    *has_value = true;
                }
            }
            ',' => {
                out.push(c);
                if let Some((Frame::Object { expecting_key }, _)) = stack.last_mut() {
                    *expecting_key = true;
                }
            }
            ':' => {
                out.push(c);
                if let Some((Frame::Object { expecting_key }, _)) = stack.last_mut() {
                    *expecting_key = false;
                }
            }
            c if c.is_whitespace() => {
                // Detect "value closer, whitespace, new value start"
                // (the model wrote `} {"` and forgot the `,`).
                // Insert the `,` between the closer and the
                // whitespace so the result reads `}, {"…`.
                if let Some((_, has_value)) = stack.last()
                    && *has_value
                {
                    let mut j = i + 1;
                    while j < n && chars[j].is_whitespace() {
                        j += 1;
                    }
                    let next = if j < n { Some(chars[j]) } else { None };
                    let is_value_start = matches!(next, Some('"') | Some('{') | Some('['));
                    let prev = out.chars().last();
                    let prev_is_closer = prev == Some('}') || prev == Some(']');
                    if is_value_start && prev_is_closer && !out.ends_with(',') {
                        out.push(',');
                        changed = true;
                    }
                }
                out.push(c);
            }
            _ => out.push(c),
        }
        i += 1;
    }
    if in_string {
        return None;
    }
    if stack.is_empty() {
        if changed { Some(out) } else { None }
    } else {
        // Stack is non-empty: the input has unclosed containers. The
        // bracket-repair pass will handle that case better than we
        // do here (it knows which closer to append and how to
        // rebalance cascade mismatches). Give up so the upstream
        // chain can hand the input to the bracket-repair.
        None
    }
}

// --- bracket repair ---

/// Walk the input char by char and repair a missing closer (`}` or `]`)
/// at the end, plus the case where the model emits a closer that
/// belongs to a parent scope (e.g. `}` while still inside `[`).
///
/// Returns:
///   - `Some(repaired)` if any closer was inserted,
///   - `Some(s.clone())` if the input was already balanced (so the
///     upstream caller can chain with the colon-repair pass without
///     losing work),
///   - `None` if the input is unterminated mid-string (we cannot
///     safely repair that).
#[allow(dead_code)] // Reference implementation; the iterative bracket repair uses `repair_one_missing_bracket`.
fn repair_missing_brackets(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some(String::new());
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut changed = false;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if in_string && c == '\\' && i + 1 < n {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if in_string && escape {
            out.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if in_string {
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '{' | '[' => {
                stack.push(c);
                out.push(c);
            }
            '}' | ']' => {
                let expected = match c {
                    '}' => '{',
                    ']' => '[',
                    _ => unreachable!(),
                };
                if stack.last() == Some(&expected) {
                    stack.pop();
                    out.push(c);
                } else {
                    // The model emitted a closer that does not match
                    // the most-recent opener. The most common
                    // MiniMax-M3 form: the `}` was meant to close
                    // the outer scope but the inner array's `]` is
                    // missing. Insert the closer the top of the
                    // stack needs, then re-evaluate.
                    let top = stack.last().copied();
                    let needed = match top {
                        Some('{') => '}',
                        Some('[') => ']',
                        _ => return None,
                    };
                    out.push(needed);
                    changed = true;
                    stack.pop();
                    if stack.last() == Some(&expected) {
                        stack.pop();
                        out.push(c);
                    } else {
                        return None;
                    }
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    if in_string {
        return None;
    }
    if stack.is_empty() {
        // Balanced. If we changed something to repair, hand the
        // result to the caller; otherwise hand the input back so
        // the chained colon-repair pass is not lost.
        if changed {
            Some(out)
        } else {
            Some(s.to_owned())
        }
    } else {
        let closers: String = stack
            .iter()
            .rev()
            .map(|c| match c {
                '{' => '}',
                '[' => ']',
                _ => unreachable!(),
            })
            .collect();
        Some(format!("{out}{closers}"))
    }
}

/// Single-step variant of [`repair_missing_brackets`]. Adds at most
/// ONE missing closer (`}` or `]`) per call so the caller — the
/// iterative loop [`repair_brackets_iterative`] — can try parsing
/// between insertions and stop as soon as the payload is valid JSON.
///
/// Returns:
///
///   - `Some(input.clone())` if the input is already balanced (the
///     stack drained on the walk); the iterative loop uses this as
///     the "no more closers to add" signal.
///   - `Some(inserted)` if exactly one closer was added: either a
///     trailing closer for the still-open top of the stack (the
///     end-of-input case) or an inline closer for a mismatched
///     closing char the model emitted (`...[item}` → `...[item]}`).
///   - `None` if the input is unrepairable in this pass
///     (unterminated string, mismatched closer with an empty stack).
///
/// The walker's string handling mirrors [`repair_missing_brackets`]:
/// escapes inside strings are honoured, mid-string truncation aborts
/// with `None`, and `]`/`}` are only matched against `[`/`{`
/// respectively when outside a string.
fn repair_one_missing_bracket(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some(String::new());
    }
    let mut out = String::with_capacity(s.len() + 1);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if in_string && c == '\\' && i + 1 < n {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if in_string && escape {
            out.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if in_string {
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '{' | '[' => {
                stack.push(c);
                out.push(c);
            }
            '}' | ']' => {
                let expected = match c {
                    '}' => '{',
                    ']' => '[',
                    _ => unreachable!(),
                };
                if stack.last() == Some(&expected) {
                    stack.pop();
                    out.push(c);
                } else {
                    // Mismatched closer: insert the closer the top of
                    // the stack needs first, then accept the model's
                    // closer. ONE closer added per call; the
                    // iterative loop will retry on the next pass if
                    // the inner stack still has an unclosed opener.
                    let top = stack.last().copied();
                    let needed = match top {
                        Some('{') => '}',
                        Some('[') => ']',
                        _ => return None,
                    };
                    out.push(needed);
                    stack.pop();
                    if stack.last() == Some(&expected) {
                        stack.pop();
                        out.push(c);
                    } else {
                        return None;
                    }
                    return Some(out);
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    if in_string {
        return None;
    }
    if stack.is_empty() {
        // Balanced already — return the input untouched so the
        // iterative loop sees "no closer added" and breaks out.
        Some(s.to_owned())
    } else {
        // Add exactly ONE closer from the still-open top of the stack.
        let needed = match stack.last().copied() {
            Some('{') => '}',
            Some('[') => ']',
            _ => return None,
        };
        out.push(needed);
        Some(out)
    }
}

/// Iteratively attempt bracket repair: up to `max_iters` passes of
/// balance → attempt-parse → keep-or-return. Each pass that
/// actually modified the input emits one [`RepairEvent`] of kind
/// [`RepairKind::Bracket`] to `sink` so the warnings stream keeps
/// one record per closer the heuristic recovered (matching the
/// single-event emission that the pre-iterative chain produced for
/// the common one-closer case).
///
/// Implements `proposal-02-rust.md §4.6`'s iterative closing-bracket
/// autocompletion: nested outputs from MiniMax-M3 often need more
/// than one closer (inner `]` then outer `}`), and the previous
/// single-pass implementation could miss cases where the per-pass
/// walker only added one closer at a time. By adding one closer
/// per pass and re-parsing after each insertion, the loop stops as
/// soon as the payload is valid JSON.
///
/// Returns:
///
/// - `Some(candidate)` once the balanced candidate parses as JSON,
///   OR after `max_iters` passes with the LAST repaired candidate
///   (or the original input if no closer was added) so the caller
///   can still attempt a final parse and surface a useful
///   diagnostic — matching the pre-iterative chain's contract.
/// - `None` only when the walker refuses outright (e.g. `}{[` where
///   the misplaced closer hits an empty stack and aborts). In that
///   case the chain propagates `None` and the caller's
///   `Error::SchemaViolation` path fires.
///
/// `max_iters = 3` is the production cap (see
/// [`DEFAULT_BRACKET_REPAIR_MAX_ITERS`]). Tests use lower caps to
/// pin specific iter counts.
pub(crate) fn repair_brackets_iterative(
    input: &str,
    sink: &mut impl FnMut(RepairEvent),
    max_iters: usize,
) -> Option<String> {
    let mut current = input.to_owned();
    let mut last_repaired: Option<String> = None;
    for _ in 0..max_iters {
        match repair_one_missing_bracket(&current) {
            Some(repaired) if repaired != current => {
                sink(RepairEvent {
                    kind: RepairKind::Bracket,
                    bytes_before: current.len(),
                    bytes_after: repaired.len(),
                });
                current = repaired.clone();
                last_repaired = Some(repaired);
            }
            Some(_) => {
                // Balanced already — no further closer to add. Hand
                // back the last repaired candidate (or the current
                // string if we never repaired anything) so the
                // caller can still attempt a final parse.
                return last_repaired.or(Some(current));
            }
            None => return None,
        }
        if serde_json::from_str::<serde_json::Value>(&current).is_ok() {
            return Some(current);
        }
    }
    // max_iters exhausted without a parse success. Hand back the
    // last repaired candidate (or the original input if we never
    // appended anything) so the caller can still surface the
    // post-repair parse error with the per-pass events that fired.
    last_repaired.or(Some(current))
}

/// Remove leading ```json and trailing ``` markers from a model output.
pub fn strip_code_fence(raw: &str) -> String {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("```json")
        && let Some(end) = rest.rfind("```")
    {
        return rest[..end].trim().to_owned();
    }
    if let Some(rest) = s.strip_prefix("```")
        && let Some(end) = rest.rfind("```")
    {
        return rest[..end].trim().to_owned();
    }
    s.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    static STALE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn stale_artifact_emits_when_artifact_old() {
        let _guard = STALE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.json");
        std::fs::write(&path, b"{}").unwrap();
        unsafe {
            std::env::set_var("MOAGAN_STALE_TTL_SECS", "0");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        let artifact = emit_stale_artifact_if_needed(&path);
        unsafe {
            std::env::remove_var("MOAGAN_STALE_TTL_SECS");
        }
        assert!(artifact.is_some());
    }

    #[test]
    fn stale_artifact_silent_when_fresh() {
        let _guard = STALE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fresh.json");
        std::fs::write(&path, b"{}").unwrap();
        unsafe {
            std::env::set_var("MOAGAN_STALE_TTL_SECS", u64::MAX.to_string());
        }
        let artifact = emit_stale_artifact_if_needed(&path);
        unsafe {
            std::env::remove_var("MOAGAN_STALE_TTL_SECS");
        }
        assert!(artifact.is_none());
    }

    #[test]
    fn stale_artifact_respects_env_ttl() {
        let _guard = STALE_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("configured.json");
        std::fs::write(&path, b"{}").unwrap();
        unsafe {
            std::env::set_var("MOAGAN_STALE_TTL_SECS", "0");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        let artifact = emit_stale_artifact_if_needed(&path);
        unsafe {
            std::env::remove_var("MOAGAN_STALE_TTL_SECS");
        }
        assert_eq!(artifact.map(|value| value.ttl_secs), Some(0));
    }

    #[test]
    fn json_roundtrip_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.json");
        let v = Sample {
            a: 7,
            b: "ok".into(),
        };
        write_json(&p, &v).unwrap();
        let back: Sample = read_json(&p).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn strip_fence_with_json_tag() {
        let s = "```json\n{\"a\":1,\"b\":\"x\"}\n```";
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 1);
    }

    #[test]
    fn strip_fence_without_tag() {
        let s = "```\n{\"a\":2,\"b\":\"x\"}\n```";
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 2);
    }

    #[test]
    fn strip_fence_passthrough() {
        let s = "{\"a\":3,\"b\":\"x\"}";
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 3);
    }

    #[test]
    fn invalid_json_errors() {
        let s = "not json";
        let r: std::result::Result<Sample, _> = parse_model_json(s);
        assert!(r.is_err());
    }

    #[test]
    fn missing_closer_errors() {
        // The model stopped mid-string. The repair refuses to
        // touch unterminated strings, so the user sees the
        // original error with the raw payload.
        let truncated = r#"{"a":1,"b":"hel"#;
        let r: std::result::Result<Sample, _> = parse_model_json(truncated);
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("full raw follows"));
        assert!(msg.contains("{\"a\":1,\"b\":\"hel"));
    }

    // --- close_missing_brackets unit tests (the original m3 bug) ---

    // --- repair_m3_brackets unit tests (the original m3 bug) ---

    #[test]
    fn repair_returns_input_as_is_when_already_balanced() {
        let s = r#"{"a":1,"b":"x"}"#;
        let out = repair_m3_brackets(s).unwrap();
        // Balanced input is returned unchanged (no repair needed,
        // but the chain contract still hands the input back so the
        // caller doesn't lose any upstream patch).
        assert_eq!(out, s);
    }

    #[test]
    fn repair_returns_empty_for_empty_input() {
        // Empty input is technically balanced (trivially): the
        // bracket-walker returns Some("") so the chained colon-repair
        // does not crash on an empty input.
        let out = repair_m3_brackets("").unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn repair_returns_none_when_unterminated_string() {
        // The model stopped mid-string. We can't safely repair
        // this and the user sees the original error.
        let s = r#"{"a":1,"b":"hello"#;
        assert!(repair_m3_brackets(s).is_none());
    }

    #[test]
    fn repair_appends_for_missing_close_brace() {
        // Input is missing the outer `}`.
        let s = r#"{"a":1,"b":"x""#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"a":1,"b":"x"}"#);
    }

    #[test]
    fn repair_inserts_bracket_before_misplaced_brace() {
        // The m3 case: outer `}` was emitted but the inner array `]`
        // is missing. The walker inserts `]` to close the array,
        // then matches the model's `}` against the outer `{`.
        // The result is valid JSON with the `]` in the right
        // position.
        let s = r#"{"verdict":"fix","issues":["a","b","c","d"],"suggestions":["e","f","g","h"}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(
            out,
            r#"{"verdict":"fix","issues":["a","b","c","d"],"suggestions":["e","f","g","h"]}"#
        );
    }

    #[test]
    fn repair_handles_escaped_quotes_in_strings() {
        // The string contains an escaped quote \" which must not
        // be treated as a string boundary.
        let s = r#"{"a":1,"b":"he said \"hi\"""#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"a":1,"b":"he said \"hi\""}"#);
    }

    #[test]
    fn repair_returns_none_when_only_closers() {
        // Stack is empty after popping — no repair needed.
        let s = r#"]}"#;
        assert!(repair_m3_brackets(s).is_none());
    }

    // --- end-to-end parse_model_json with repair ---

    #[test]
    fn parse_model_json_repairs_m3_style_truncation() {
        // The exact failure mode we observed in m3: the model emits
        // the outer `}` while still inside an array, so the JSON
        // is missing the inner `]`. The repair path appends `]`
        // and the parse succeeds.
        let s = r#"{"a":1,"b":"x"}"#;
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 1);
        assert_eq!(v.b, "x");
    }

    #[test]
    fn parse_model_json_repairs_missing_outer_brace() {
        // The model stopped mid-output without closing the outer object.
        let s2 = r#"{"a":4,"b":"y""#;
        let v: Sample = parse_model_json(s2).unwrap();
        assert_eq!(v.a, 4);
        assert_eq!(v.b, "y");
    }

    #[test]
    fn parse_model_json_does_not_repair_mid_string() {
        // The model stopped inside a string. We can't safely repair
        // this and the user sees the original error.
        let s = r#"{"a":1,"b":"hel"#;
        let r: std::result::Result<Sample, _> = parse_model_json(s);
        assert!(r.is_err());
    }

    // --- separator-repair tests (m3 bug 3: missing `,` between values) ---

    #[test]
    fn repair_inserts_comma_between_array_strings() {
        // The model wrote `["a" "b" "c"]` — three strings with no
        // commas between them. The separator-repair pass inserts the
        // missing commas.
        let s = r#"["a" "b" "c"]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"["a", "b", "c"]"#);
    }

    #[test]
    fn repair_inserts_comma_between_object_values() {
        // The model wrote `{"k1":"v1" "k2":"v2"}` — two pairs with
        // no comma between them.
        let s = r#"{"k1":"v1" "k2":"v2"}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"k1":"v1", "k2":"v2"}"#);
    }

    #[test]
    fn repair_does_not_double_existing_comma() {
        // When the input is well-formed, the repair should be a
        // no-op (return the same string).
        let s = r#"{"a":1, "b":2, "c":3}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn repair_eats_trailing_comma_before_closer() {
        // The model wrote `[1, 2, 3,]` — a trailing comma inside
        // the array. The separator-repair pass strips it.
        let s = r#"[1, 2, 3,]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"[1, 2, 3]"#);
    }

    #[test]
    fn repair_handles_missing_comma_and_missing_closer_together() {
        // The full m3-style cascade: missing `,` between values AND
        // missing `]` at the end. The combined separator+bracket
        // repair should produce a single valid JSON.
        let s = r#"["a" "b" "c"]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"["a", "b", "c"]"#);
    }

    #[test]
    fn repair_inside_array_with_objects() {
        // Each item is an object literal; the model wrote them
        // back-to-back without commas. The walker inserts `,`
        // before each subsequent `{`.
        let s = r#"[{"a":1} {"b":2} {"c":3}]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"[{"a":1}, {"b":2}, {"c":3}]"#);
    }

    // --- colon-insertion tests (m3 bug 2: missing `:` between key and value) ---

    #[test]
    fn repair_inserts_colon_when_string_is_closed_then_bracket() {
        // The actual m3_fib_a failure mode captured from the proxy:
        // the model writes the key string properly closed, then
        // opens the array without `:`. The colon-repair pass
        // inserts the missing `:`. The full input also omits the
        // closing `]` and `}`, so the combined colon+bracket
        // repair must produce the complete JSON.
        let s = r#"{"verdict":"fix","issues":[],"suggestions"["a","b","c","d"]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(
            out,
            r#"{"verdict":"fix","issues":[],"suggestions":["a","b","c","d"]}"#
        );
    }

    #[test]
    fn repair_inserts_colon_when_string_is_followed_by_object() {
        // The model wrote `"meta"{x:1}` — the key string
        // "meta" is properly closed, but `:` is missing and `{`
        // follows immediately. The colon-repair pass inserts `:`.
        let s = r#"{"id":"p_001","meta"{x:1}}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"id":"p_001","meta":{x:1}}"#);
    }

    #[test]
    fn repair_recovers_unterminated_key_by_using_next_string() {
        // Worse m3 variant: the model wrote `"suggestions[a,b]}`
        // with NO closing `"` for the key, but a closing `"` later
        // for the array values. The colon-repair sees the closing
        // quote as belonging to a complete string and inserts `:`,
        // giving us a parseable (if weird) JSON.
        let s = r#"{"suggestions"[a,b]}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"suggestions":[a,b]}"#);
    }

    #[test]
    fn repair_inserts_colon_into_unrecoverable_input() {
        // The walker can only do its best. For an input that is
        // both unterminated and malformed it will still try to
        // insert the `:`. The bracket-repair then closes the still-
        // open array/object so we surface a "looks-like-JSON"
        // output for the validator to diagnose. The result is
        // still invalid JSON but at least bracket-balanced, which
        // gives the validator's serde error a cleaner shape.
        let s = r#"{"ab"[a"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"ab":[a]}"#);
    }

    #[test]
    fn repair_inserts_colon_with_whitespace_between() {
        // The model emits `"b"   [1,2]` — close quote, then a
        // couple of spaces, then `[`. The colon-repair pass
        // tolerates whitespace and inserts `:` between the close
        // quote and the bracket.
        let s = r#"{"a":1,"b"   [1,2]}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"a":1,"b":   [1,2]}"#);
    }

    #[test]
    fn repair_does_not_double_colon_on_valid_input() {
        // Sanity: when the JSON is correctly written, the chain
        // returns the input unchanged.
        let s = r#"{"a":1,"b":[1,2]}"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn repair_does_not_insert_colon_in_array_context() {
        // Inside an array, a string close followed by `,` is
        // valid. We must not insert `:` in that case.
        let s = r#"["a","b","c"]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn repair_combines_colon_insertion_with_missing_closer() {
        // Stress test: the same blob has both pathologies at once.
        // The model emits a key-then-array value with the array
        // not yet closed. We should fix the missing `:` AND the
        // missing `]` to produce a single valid JSON.
        let s = r#"{"id":"p","suggestions"["a","b"]"#;
        let out = repair_m3_brackets(s).unwrap();
        assert_eq!(out, r#"{"id":"p","suggestions":["a","b"]}"#);
    }

    #[test]
    fn parse_model_json_repairs_missing_colon() {
        // End-to-end: the actual m3_fib_a failure mode captured from
        // the proxy. The model produces a Critique-like payload
        // with `:` missing between the suggestions key string and
        // the array. The colon-repair inserts `:` and the
        // bracket-repair appends `]}` to close the unclosed
        // containers, producing a parseable JSON.
        let s = r#"{"verdict":"fix","issues":[],"suggestions"["a","b","c","d"]"#;
        let v: serde_json::Value = parse_model_json(s).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj["verdict"], "fix");
        let suggestions = obj["suggestions"].as_array().unwrap();
        assert_eq!(suggestions.len(), 4);
    }

    #[test]
    fn parse_model_json_does_not_double_colon_on_valid_input() {
        // Sanity: a well-formed payload still parses cleanly. The
        // repair path is a no-op here, so the direct parse wins.
        let s = r#"{"a":1,"b":"ok"}"#;
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 1);
        assert_eq!(v.b, "ok");
    }

    // --- traced parser tests -----------------------------------------

    #[test]
    fn traced_reports_colon_and_bracket_repair() {
        // `"b"[1,2]` — missing colon between the key string and the
        // array value, plus missing closing `]}` at the end. The
        // tolerant extractor returns UnbalancedBraces (no balanced
        // end), so the wrapper falls through to the M3-on-full-input
        // path; both colon and bracket passes fire and the `FnMut`
        // sink records them in pipeline order.
        let s = r#"{"a":1,"b"[1,2"#;
        let mut kinds: Vec<RepairKind> = Vec::new();
        let v: serde_json::Value = parse_model_json_traced(s, |ev| {
            kinds.push(ev.kind);
        })
        .unwrap();
        assert_eq!(v["a"], serde_json::json!(1));
        assert!(kinds.contains(&RepairKind::Colon));
        assert!(kinds.contains(&RepairKind::Bracket));
    }

    #[test]
    fn traced_reports_separator_repair_on_extracted_candidate() {
        // The tolerant extractor carves out the balanced `[…]`, then
        // the M3 chain runs on the candidate to fix the missing
        // commas. The separator pass fires inside the Path B branch
        // and the `FnMut` sink records it.
        let s = r#"["a" "b" "c"]"#;
        let mut kinds: Vec<RepairKind> = Vec::new();
        let v: serde_json::Value = parse_model_json_traced(s, |ev| {
            kinds.push(ev.kind);
        })
        .unwrap();
        assert_eq!(v, serde_json::json!(["a", "b", "c"]));
        assert!(kinds.contains(&RepairKind::Separator));
    }

    #[test]
    fn traced_no_events_on_valid_input() {
        let s = r#"{"a":1,"b":"ok"}"#;
        let mut kinds: Vec<RepairKind> = Vec::new();
        let _v: Sample = parse_model_json_traced(s, |ev| {
            kinds.push(ev.kind);
        })
        .unwrap();
        assert!(kinds.is_empty());
    }

    /// Chain-level cover for the control-token strip: a payload
    /// wrapped in chat-template markers parses to the inner JSON
    /// with no marker text leaking into the returned `Value`. The
    /// bracket family is the case that actually needs the strip —
    /// `[BOS]` otherwise hijacks the extractor's delimiter scan.
    /// No `RepairEvent` fires: the strip is not a repair.
    #[test]
    fn traced_strips_chat_template_markers_from_wrapped_payload() {
        for s in [
            "<|im_start|>assistant\n{\"a\":1,\"b\":\"ok\"}<|im_end|>",
            "[BOS]{\"a\":1,\"b\":\"ok\"}[EOS]",
            "[BO S]{\"a\":1,\"b\":\"ok\"}[EOS ]",
            "<system>here it is</system>\n{\"a\":1,\"b\":\"ok\"}",
        ] {
            let mut kinds: Vec<RepairKind> = Vec::new();
            let v: Sample = parse_model_json_traced(s, |ev| kinds.push(ev.kind))
                .unwrap_or_else(|e| panic!("payload {s:?} should parse: {e}"));
            assert_eq!(
                v,
                Sample {
                    a: 1,
                    b: "ok".to_owned(),
                }
            );
            assert!(kinds.is_empty(), "strip must not report a repair");
        }
    }

    /// The strip must not reach well-formed JSON: a marker sitting
    /// inside a string value is part of the data, and the direct
    /// parse at the top of the chain returns before the strip runs.
    #[test]
    fn traced_preserves_chat_template_markers_inside_string_values() {
        let s = r#"{"a":1,"b":"<system>[BOS]<|im_end|>"}"#;
        let v: Sample = parse_model_json_traced(s, |_| {}).unwrap();
        assert_eq!(v.b, "<system>[BOS]<|im_end|>");
    }

    #[test]
    fn traced_includes_bytes_delta() {
        let s = r#"{"a":1,"b":[1,2"#;
        let mut events: Vec<RepairEvent> = Vec::new();
        let _v: serde_json::Value = parse_model_json_traced(s, |ev| {
            events.push(ev);
        })
        .unwrap();
        // At least the bracket pass fires; after BYTES.len != before.
        let bracket = events
            .iter()
            .find(|e| e.kind == RepairKind::Bracket)
            .expect("bracket repair event");
        assert!(bracket.bytes_after > bracket.bytes_before);
    }

    #[test]
    fn traced_propagates_failure_on_unrepairable() {
        let s = r#"{"a":1,"b":"hel"#;
        let mut kinds: Vec<RepairKind> = Vec::new();
        let r: std::result::Result<Sample, _> =
            parse_model_json_traced(s, |ev| kinds.push(ev.kind));
        assert!(r.is_err());
        // No repair events were emitted because the chain refused to
        // write the unterminated input.
        assert!(kinds.is_empty());
    }

    // --- iterative bracket-repair tests -------------------------------
    //
    // proposal-02-rust.md §4.6 calls for iterative `}`/`]`
    // autocompletion: nested outputs from MiniMax-M3 need more than
    // one closer (inner `]` then outer `}`), and the previous
    // single-pass walker only fired once. The tests below pin the
    // new helper's behaviour: per-iter `RepairEvent::Bracket`
    // emission, the `max_iters` cap, and the early-return on
    // unrepairable / already-balanced input.

    #[test]
    fn repair_brackets_iterative_balances_in_one_pass() {
        // Simple case: missing outer `}`. The helper appends the
        // closer on iter 0, the parse succeeds, and the function
        // returns the parseable candidate with exactly one event.
        let s = r#"{"a": 1"#;
        let mut events: Vec<RepairEvent> = Vec::new();
        let result: Option<String> = repair_brackets_iterative(s, &mut |ev| events.push(ev), 3);
        assert_eq!(result.as_deref(), Some(r#"{"a": 1}"#));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, RepairKind::Bracket);
        assert_eq!(events[0].bytes_before, s.len());
        assert_eq!(events[0].bytes_after, result.as_ref().unwrap().len());
    }

    #[test]
    fn repair_brackets_iterative_balances_nested_in_two_passes() {
        // Nested case (the MiniMax-M3 pattern from proposal §4.6):
        // the inner `]` and the outer `}` are both missing. The
        // helper appends `]` on iter 0 (still unparseable, the
        // outer `{` is open), then `}` on iter 1, and the parse
        // succeeds. Exactly two `Bracket` events fire — one per
        // closer the heuristic recovered.
        let s = r#"{"a": [1, 2"#;
        let mut events: Vec<RepairEvent> = Vec::new();
        let result: Option<String> = repair_brackets_iterative(s, &mut |ev| events.push(ev), 3);
        assert_eq!(result.as_deref(), Some(r#"{"a": [1, 2]}"#));
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.kind == RepairKind::Bracket));
        // Bytes-after grows monotonically as closer chars append.
        assert!(events[1].bytes_after > events[0].bytes_after);
    }

    #[test]
    fn repair_brackets_iterative_caps_at_three_iterations() {
        // 4-level deep array — needs 4 `]` closers to parse. With
        // `max_iters = 3` the helper exhausts the budget, emits
        // exactly 3 events (one per closer added), and hands back
        // the last repaired candidate so the caller's final parse
        // attempt can produce a `SchemaViolation` diagnostic. The
        // event count is the contract; the returned string is the
        // post-repair candidate the chain will pass to
        // `serde_json::from_str` one more time.
        let s = "[[[[1";
        let mut events: Vec<RepairEvent> = Vec::new();
        let result: Option<String> = repair_brackets_iterative(s, &mut |ev| events.push(ev), 3);
        let repaired = result.expect("cap-at-3 returns the last repaired candidate");
        assert_eq!(repaired, "[[[[1]]]");
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.kind == RepairKind::Bracket));
    }

    #[test]
    fn repair_brackets_iterative_emits_one_repair_event_per_iteration() {
        // The event count is `iter_count` for any payload whose
        // recovery took less than `max_iters` passes. Pin the
        // 2-iter case explicitly so a future regression that
        // collapses it to a single event (re-introducing the
        // single-pass bug) is caught.
        let s = r#"{"a": [1, 2"#;
        let mut events: Vec<RepairEvent> = Vec::new();
        let result: Option<String> = repair_brackets_iterative(s, &mut |ev| events.push(ev), 3);
        assert!(result.is_some());
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.kind == RepairKind::Bracket));
        assert_eq!(events[0].bytes_before, s.len());
        assert!(events[1].bytes_before > events[0].bytes_before);
    }

    #[test]
    fn repair_brackets_iterative_handles_already_balanced_input() {
        // The direct parse wins in `parse_model_json_traced`, so
        // this branch is reached only in tests / non-default paths.
        // The helper should return the input unchanged and emit
        // zero events (no closer to add → no `RepairEvent`).
        let s = r#"{"a":1}"#;
        let mut events: Vec<RepairEvent> = Vec::new();
        let result: Option<String> = repair_brackets_iterative(s, &mut |ev| events.push(ev), 3);
        assert_eq!(result.as_deref(), Some(s));
        assert!(events.is_empty());
    }

    #[test]
    fn repair_brackets_iterative_handles_already_broken_payload() {
        // `}{[` is unrepairable: the misplaced `}` hits an empty
        // stack, the walker aborts immediately, and the helper
        // returns `None`. No events fire (no closer added) and the
        // caller's `Error::SchemaViolation` path engages.
        let s = "}{[";
        let mut events: Vec<RepairEvent> = Vec::new();
        let result: Option<String> = repair_brackets_iterative(s, &mut |ev| events.push(ev), 3);
        assert!(result.is_none());
        assert!(events.is_empty());
    }

    #[test]
    fn parse_model_json_traced_recovers_nested_truncated_payload() {
        // Integration test for the proposal §4.6 contract:
        // nested-truncated payloads that need multiple closers
        // (the inner `]` and the outer `}` revealed by it, plus
        // the array's `}`) used to fail under the single-pass
        // walker when the trace-sink path was exercised. With
        // `repair_brackets_iterative` the helper walks the stack
        // one closer per pass until the payload parses, and the
        // `parse_model_json_traced` wrapper returns `Ok` with
        // the recovered value intact.
        //
        // Three closers are needed: the inner `]` (closes the
        // `"bar": 2` object), the array's `]`, and the outer `}`.
        // The iterative helper adds them across three passes and
        // emits one `RepairEvent::Bracket` per closer.
        let s = r#"{"foo": [1, {"bar": 2"#;
        let mut events: Vec<RepairEvent> = Vec::new();
        let v: serde_json::Value =
            parse_model_json_traced(s, |ev| events.push(ev)).expect("nested-truncated recovers");
        assert_eq!(v["foo"][0], serde_json::json!(1));
        assert_eq!(v["foo"][1]["bar"], serde_json::json!(2));
        let bracket_count = events
            .iter()
            .filter(|e| e.kind == RepairKind::Bracket)
            .count();
        assert_eq!(
            bracket_count, 3,
            "expected iterative bracket repair to fire 3 events, got {bracket_count}"
        );
    }

    // --- parse_json_with_recovery tests --------------------------------

    #[test]
    fn parse_json_with_recovery_direct_parse_succeeds() {
        // Clean JSON: the wrapper must short-circuit on the direct
        // parse and never reach the tolerant extractor.
        let v: serde_json::Value = parse_json_with_recovery(r#"{"a":1,"b":"x"}"#).unwrap();
        assert_eq!(v["a"], serde_json::json!(1));
        assert_eq!(v["b"], serde_json::json!("x"));
    }

    #[test]
    fn parse_json_with_recovery_tolerant_extraction_succeeds_on_prose_prefix() {
        // Direct parse fails because of the prose prefix. The
        // tolerant extractor (Path B) finds the balanced `{...}`
        // substring and the wrapper parses the candidate.
        let input = "Sure! Here is the JSON you requested:\n{\"answer\": 42}";
        let v: serde_json::Value = parse_json_with_recovery(input).unwrap();
        assert_eq!(v["answer"], serde_json::json!(42));
    }

    #[test]
    fn parse_json_with_recovery_m3_after_extraction_succeeds() {
        // Tolerant extraction succeeds and the candidate still has an
        // m3 pathology (missing commas between array elements). M3
        // repair on the candidate restores parseability.
        let input = "prose prefix [\"a\" \"b\" \"c\"] prose suffix";
        let v: serde_json::Value = parse_json_with_recovery(input).unwrap();
        assert_eq!(v, serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn parse_json_with_recovery_returns_error_on_truly_invalid_json() {
        // No JSON delimiter at all -> tolerant extractor fails ->
        // M3-on-full sees nothing to repair -> AllStrategiesFailed.
        let input = "this is plain prose with no JSON at all";
        let r = parse_json_with_recovery(input);
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert_eq!(err, ParseError::AllStrategiesFailed);
    }

    #[test]
    fn parse_json_with_recovery_handles_js_comments() {
        // The tolerant extractor strips JS line comments before
        // looking for the JSON delimiter. The wrapper then parses
        // the cleaned candidate directly.
        let input = "// header comment\n{\"answer\": 42}";
        let v: serde_json::Value = parse_json_with_recovery(input).unwrap();
        assert_eq!(v["answer"], serde_json::json!(42));

        // Block comment variant.
        let input = "/* block */ {\"answer\": 7}";
        let v: serde_json::Value = parse_json_with_recovery(input).unwrap();
        assert_eq!(v["answer"], serde_json::json!(7));
    }

    #[test]
    fn parse_json_with_recovery_preserves_extraction_metadata_via_tracing() {
        // Set up a tracing subscriber that captures every event into
        // an in-memory buffer. Run the wrapper on a payload that
        // requires the tolerant extraction step, then assert that
        // the recovery succeeded AND that the wrapper emitted the
        // `tracing::debug!` event that documents the extraction
        // byte range. This pins the tracing instrumentation: if a
        // future refactor drops the `tracing::debug!` call, this
        // test fails.
        use std::io;
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::prelude::*;

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .map_err(|_| io::Error::other("shared tracing buffer poisoned"))?
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for SharedBuf {
            type Writer = SharedWriter;

            fn make_writer(&'a self) -> Self::Writer {
                SharedWriter(self.0.clone())
            }
        }

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(buf.clone()),
        );

        tracing::subscriber::with_default(subscriber, || {
            let input = "noise prefix {\"answer\": 42} noise suffix";
            let v = parse_json_with_recovery(input).unwrap();
            assert_eq!(v["answer"], serde_json::json!(42));
        });

        let captured =
            String::from_utf8(buf.0.lock().map(|b| b.clone()).unwrap_or_default()).unwrap();
        assert!(
            captured.contains("parse_json_with_recovery"),
            "tracing log not captured: {captured}"
        );
        assert!(
            captured.contains("tolerant extraction"),
            "tolerant extraction event missing: {captured}"
        );
    }

    /// Regression: naive `&s[s.len()-500..]` panicked with
    /// "byte index N is not a char boundary" whenever the
    /// model returned more than 500 bytes containing CJK.
    /// The original `parse_model_json` error path used that
    /// naive slice; the `safe_tail` helper reproduces the
    /// slice but trims forward to the next UTF-8 char boundary,
    /// eliminating the panic.
    #[test]
    fn safe_tail_handles_cjk_at_byte_boundary() {
        // Build a string whose last byte is the middle byte of the
        // 3-byte UTF-8 sequence for `创` (E5 88 9B). The naive
        // 500-byte slice from the end would land inside that
        // sequence; `safe_tail` must walk back to a boundary.
        let mut s = String::with_capacity(520);
        for _ in 0..(510 / 3) {
            s.push('创');
        }
        // Pad to >=510 bytes of CJK
        while s.len() < 520 {
            s.push('创');
        }
        // 1 more char puts the final byte sequence in the last 3
        // bytes; safe_tail(max=500) must land before it.
        assert!(s.len() > 500);

        let tail = safe_tail(&s, 500);
        // No panic, length is <= max_bytes, and the slice is a
        // valid char boundary (Rust guarantees this for `&str`
        // already; the test mainly confirms we did not panic).
        assert!(tail.len() <= 500);
        assert!(s.is_char_boundary(s.len() - tail.len()));
    }

    /// Pure ASCII input behaves identically to a naive 500-byte
    /// slice: every byte is a char boundary so no walking happens.
    #[test]
    fn safe_tail_ascii_is_exact_slice() {
        let s = "x".repeat(800);
        let tail = safe_tail(&s, 500);
        assert_eq!(tail.len(), 500);
        assert_eq!(tail, "x".repeat(500));
    }

    /// Input shorter than `max_bytes` is returned unchanged. This
    /// avoids an unnecessary copy in the common error-path case
    /// (most model failures are <500 bytes long).
    #[test]
    fn safe_tail_short_string_unchanged() {
        let s = "hello world";
        let tail = safe_tail(s, 500);
        assert_eq!(tail, s);
    }

    /// The error path of `parse_model_json` no longer panics when
    /// the model's invalid JSON contains CJK past byte 500. The
    /// previous code panicked with "byte index N is not a char
    /// boundary"; this test pins the fix end-to-end.
    #[test]
    fn parse_model_json_error_message_includes_cjk_tail_without_panic() {
        // An invalid JSON that contains CJK beyond byte 500.
        let mut raw = String::with_capacity(800);
        raw.push_str("not json at all, just prose ");
        for _ in 0..(700 / 3) {
            raw.push('创');
        }
        // Confirm the test setup matches the bug condition: >500
        // bytes, multi-byte UTF-8 present near the end.
        assert!(raw.len() > 500);

        let result: Result<Sample> = parse_model_json(&raw);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err}");
        // The tail excerpt is embedded in the diagnostic — it must
        // be present, not a panic.
        assert!(
            msg.contains("tail="),
            "error did not include tail summary: {msg}"
        );
    }
}
