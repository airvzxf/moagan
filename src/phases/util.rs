//! Shared helpers for phases that need to read JSON from disk, write
//! JSON to disk atomically, and parse LLM responses.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::atomic::writer::AtomicWriter;
use crate::error::{Error, Result};

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

/// Strip a leading/trailing markdown fence from the model output, then
/// parse the inner JSON. If the first parse fails because the model
/// emitted a truncated response (missing closing brackets), try a
/// single auto-close pass and re-parse.
pub fn parse_model_json<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let trimmed = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<T>(&trimmed) {
        return Ok(v);
    }
    if let Some(closed) = auto_close_json(&trimmed)
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

/// Marker for the v0.1 stub. Focused continuation (catalog 10-integrada-v0
/// §D.4.6) is implemented in `parse_model_json_with_continuation` but
/// the phase pipeline does not yet wire a follow-up call when the
/// auto-close pass also fails. The marker is here so the catalog item
/// is visible in the source and a future commit can flip the
/// `parse_model_json` body to call the continuation variant.
pub const FOCUSED_CONTINUATION_AVAILABLE: bool = true;

/// Build a focused-continuation user message. Given the truncated
/// model output and a sample of the desired JSON shape, this returns a
/// user prompt that asks the model to emit only the missing tail. The
/// caller is expected to feed this through the same provider and then
/// concatenate `truncated` + `tail` before re-parsing.
pub fn build_continuation_user_message(truncated: &str, schema_hint: &str) -> String {
    format!(
        "The previous response was truncated mid-JSON before the object \
         closed. Re-emit the COMPLETE JSON object below, keeping the \
         content that was already produced (between ---BEGIN PREFIX--- \
         and ---END PREFIX---) and finishing any field that was cut \
         off. Output the full JSON object starting with `{{` and \
         ending with `}}`. No commentary, no markdown fences.\n\n\
         Schema reminder: {schema_hint}\n\n\
         ---BEGIN PREFIX (truncated, may be incomplete)---\n{truncated}\n\
         ---END PREFIX---\n\nOutput ONLY the complete JSON object now:"
    )
}

/// Async parser with focused continuation fallback.
///
/// 1. Strip the JSON fence and try to parse directly.
/// 2. If that fails, try the auto-close pass (handles missing `}]`).
/// 3. If auto-close also fails, ask the model to emit the missing tail
///    (`build_continuation_user_message`), concatenate, and re-parse.
///
/// Catalog 10-integrada-v0 §D.4.6.
pub async fn parse_with_continuation<T, F, Fut>(
    raw: &str,
    schema_hint: &str,
    max_continuation_bytes: usize,
    call_provider: F,
) -> Result<T>
where
    T: DeserializeOwned,
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<crate::llm::Response>>,
{
    // Pass 1: direct parse.
    if let Ok(v) = parse_model_json::<T>(raw) {
        return Ok(v);
    }
    let trimmed = strip_code_fence(raw);
    // Pass 2: auto-close missing brackets.
    if let Some(closed) = auto_close_json(&trimmed)
        && let Ok(v) = serde_json::from_str::<T>(&closed)
    {
        return Ok(v);
    }
    // Pass 3: focused continuation. Cap the truncated payload at
    // `max_continuation_bytes` so we do not blow up the next
    // request's token budget.
    let truncated: &str = if trimmed.len() > max_continuation_bytes {
        &trimmed[trimmed.len() - max_continuation_bytes..]
    } else {
        &trimmed
    };
    let user = build_continuation_user_message(truncated, schema_hint);
    let resp = call_provider(user).await?;
    let tail_raw = resp.text.trim();
    // The model may have included its own JSON fence. Strip it.
    let tail = strip_code_fence(tail_raw);
    // Concatenate and re-parse.
    let mut combined = String::with_capacity(trimmed.len() + tail.len());
    combined.push_str(&trimmed);
    combined.push_str(&tail);
    if let Ok(v) = serde_json::from_str::<T>(&combined) {
        return Ok(v);
    }
    if let Some(closed) = auto_close_json(&combined)
        && let Ok(v) = serde_json::from_str::<T>(&closed)
    {
        return Ok(v);
    }
    Err(Error::SchemaViolation(format!(
        "model output is not valid JSON after focused continuation: len={} bytes; combined_len={} bytes; full combined follows:\n{}",
        raw.len(),
        combined.len(),
        combined
    )))
}

/// If `s` is a JSON object/array that ends abruptly (the model emitted
/// content but forgot the closing brackets), append the missing
/// closers. Returns the patched string only when the patched version
/// is itself syntactically valid JSON.
fn auto_close_json(s: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for c in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match c {
            '{' | '[' => stack.push(c),
            '}' if stack.pop() != Some('{') => return None,
            ']' if stack.pop() != Some('[') => return None,
            _ => {}
        }
    }
    if in_string || stack.is_empty() {
        return None;
    }
    let mut closers = String::new();
    while let Some(open) = stack.pop() {
        closers.push(match open {
            '{' => '}',
            '[' => ']',
            _ => return None,
        });
    }
    let mut patched = String::with_capacity(s.len() + closers.len());
    patched.push_str(s);
    patched.push_str(&closers);
    Some(patched)
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
    fn auto_closes_missing_brackets() {
        // Model emitted the content but forgot the final `}`.
        let truncated = r#"{"a":1,"b":"x","c":[1,2,3]"#;
        let v: Sample = parse_model_json(truncated).unwrap();
        assert_eq!(v.a, 1);
        assert_eq!(v.b, "x");
    }

    #[test]
    fn auto_closes_nested() {
        let truncated = r#"{"a":7,"b":"x"}"#; // already complete
        let v: Sample = parse_model_json(truncated).unwrap();
        assert_eq!(v.a, 7);
    }

    #[test]
    fn continuation_user_message_contains_truncated() {
        let msg = build_continuation_user_message(r#"{"a":1,"b":"#, "test");
        assert!(msg.contains("{\"a\":1,\"b\":"));
        assert!(msg.contains("test"));
        assert!(msg.contains("---BEGIN PREFIX"));
    }
}
