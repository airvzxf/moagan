//! Tolerant JSON extractor for LLM responses that don't honor
//! `response_format: json_object` (Path B in
//! `docs/research-json-structured-output.md`).
//!
//! Multi-pass: skip JS comments in place, skip prose prefix/suffix,
//! brace-balance, then return the substring that parses as JSON.
//! Indices returned are byte offsets into the original input so the
//! caller can slice `&input[start..end]` without remapping.

use serde::de::DeserializeOwned;

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
}
