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
/// When the direct parse fails because the model forgot to close one
/// or more containers (the most common failure observed with
/// MiniMax-M3, which emits `stop_reason: end_turn` while still inside
/// an open array), we attempt a single `close_missing_brackets` pass
/// and re-parse. This is the ONLY repair we do: it appends the
/// missing `]` or `}` at the end without touching anything else. It
/// does NOT help with mid-string truncation, unescaped quotes,
/// invalid escapes, structural breakage, or field-name typos. If the
/// repair would change the meaning of the JSON, it returns `None`
/// and the caller sees the original error.
///
/// Do NOT remove this without a corresponding fix to the upstream
/// model. Smoke batches against m3 have shown ~9% per-call failure
/// rate from this exact case; without the repair, every affected run
/// hard-errors and the user has to retry. Logged in commit
/// `e0a4594` (F4) and reintroduced in the planned F8 below.
pub fn parse_model_json<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let trimmed = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<T>(&trimmed) {
        return Ok(v);
    }
    if let Some(closed) = close_missing_brackets(&trimmed)
        && let Ok(v) = serde_json::from_str::<T>(&closed)
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

/// Repair the narrow case where the model emitted JSON that is
/// structurally complete but forgot to close one or more containers
/// (`{` or `[`) at the end.
///
/// The walker ignores everything inside a string. When it sees a
/// closer (`}` or `]`) whose matching opener is on top of the stack,
/// it pops normally and copies the character to the output. When the
/// closer DOES NOT match the top, the model emitted the closer while
/// still inside a nested container: the model effectively meant to
/// close the outer scope but wrote the wrong character at the wrong
/// position. We insert the closer that the top of the stack actually
/// needs, then "consume" the model's nearer opener so the model's
/// closer can match the next thing down.
///
/// Example with MiniMax-M3 input `{"v":"f","a":[1,2],"b":[3,4}`:
/// the model emitted the outer `}` while still inside array `b`.
/// The walker inserts `]` to close `b`, then matches the model's
/// `}` against the outer `{`. Result: `{"v":"f","a":[1,2],"b":[3,4]}`.
/// Valid JSON.
///
/// Returns `None` in four cases:
///   1. An unterminated string (the model stopped mid-string).
///   2. The string is already balanced and no repair was needed —
///      the caller should try the direct parse instead.
///   3. The string is empty.
///   4. The mismatch cannot be resolved (e.g. the model's closer
///      doesn't match anything on the stack, or the stack is empty).
///
/// This is NOT a magic fix. It only handles "missing closer at the
/// end" and "mid-output close against the wrong scope". Mid-string
/// truncation, unescaped quotes, invalid escapes, structural
/// breakage, and field-name typos require parser changes upstream
/// or a different model.
fn close_missing_brackets(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(s.len() + 4);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut changed = false;
    for c in s.chars() {
        if escape {
            escape = false;
            out.push(c);
            continue;
        }
        if c == '\\' {
            escape = true;
            out.push(c);
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            out.push(c);
            continue;
        }
        if in_string {
            out.push(c);
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
    }
    if in_string {
        return None;
    }
    if stack.is_empty() {
        if changed {
            Some(out)
        } else {
            // No repair needed — direct parse will succeed.
            None
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

    // --- close_missing_brackets unit tests ---

    #[test]
    fn close_missing_returns_none_when_already_balanced() {
        let s = r#"{"a":1,"b":"x"}"#;
        assert!(close_missing_brackets(s).is_none());
    }

    #[test]
    fn close_missing_returns_none_for_empty_input() {
        assert!(close_missing_brackets("").is_none());
    }

    #[test]
    fn close_missing_returns_none_when_unterminated_string() {
        let s = r#"{"a":1,"b":"hello"#;
        assert!(close_missing_brackets(s).is_none());
    }

    #[test]
    fn close_missing_appends_for_missing_close_brace() {
        // Input is missing the outer `}`.
        let s = r#"{"a":1,"b":"x""#;
        let out = close_missing_brackets(s).unwrap();
        assert_eq!(out, r#"{"a":1,"b":"x"}"#);
    }

    #[test]
    fn close_missing_inserts_bracket_before_misplaced_brace() {
        // The m3 case: outer `}` was emitted but the inner array `]`
        // is missing. The walker inserts `]` to close the array,
        // then matches the model's `}` against the outer `{`.
        // The result is valid JSON with the `]` in the right
        // position.
        let s = r#"{"verdict":"fix","issues":["a","b","c","d"],"suggestions":["e","f","g","h"}"#;
        let out = close_missing_brackets(s).unwrap();
        assert_eq!(
            out,
            r#"{"verdict":"fix","issues":["a","b","c","d"],"suggestions":["e","f","g","h"]}"#
        );
    }

    #[test]
    fn close_missing_handles_escaped_quotes_in_strings() {
        // The string contains an escaped quote \" which must not
        // be treated as a string boundary.
        let s = r#"{"a":1,"b":"he said \"hi\"""#;
        let out = close_missing_brackets(s).unwrap();
        assert_eq!(out, r#"{"a":1,"b":"he said \"hi\""}"#);
    }

    #[test]
    fn close_missing_returns_none_when_only_closers() {
        // Stack is empty after popping — no repair needed.
        let s = r#"]}"#;
        assert!(close_missing_brackets(s).is_none());
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
}
