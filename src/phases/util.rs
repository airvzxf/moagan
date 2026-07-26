//! Shared helpers for phases that need to read JSON from disk, write
//! JSON to disk atomically, and parse LLM responses.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::atomic::writer::AtomicWriter;
use crate::error::Result;

/// Read a JSON file and deserialize it.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(crate::Error::from)
}

/// Write `value` as JSON to `path` atomically.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(crate::Error::from)?;
    AtomicWriter::new().write(path, &bytes)?;
    Ok(())
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
    let tail_start = trimmed.len().saturating_sub(500);
    let tail = &trimmed[tail_start..];
    Err(crate::Error::SchemaViolation(format!(
        "model output is not valid JSON: {e}; len={} bytes; tail={:?}; full raw follows:\n{}",
        trimmed.len(),
        tail,
        trimmed
    )))
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
    // Three chained repair passes. The order matters:
    //  1. colon-repair: insert `:` between a complete string and the
    //     next `[` or `{` (the `"key"[item]` pathology).
    //  2. separator-repair: insert `,` between two adjacent values
    //     inside an array or object (the `["a" "b"]` pathology).
    //  3. bracket-repair: append missing closers and rebalance
    //     mismatched `}` / `]` cascades.
    // Each pass is a no-op when its target pathology is absent, so the
    // composition is safe to run on already-valid inputs.
    let colon = repair_missing_colon(s);
    let after_colon = colon.as_deref().unwrap_or(s);
    let seps = repair_missing_separators(after_colon);
    let after_seps = seps.as_deref().unwrap_or(after_colon);
    repair_missing_brackets(after_seps)
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
}
