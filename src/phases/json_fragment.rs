//! The fragment check of the model-JSON recovery chain
//! ([`crate::phases::util::parse_model_json_traced`]).
//!
//! The open-brace repair prepends `{` to an answer that starts with a
//! bare `"key":`. When the model's own text never closes that object
//! either, both braces would be invented and a lone field
//! (`"problem": "…",`) would parse as an object whose other fields are
//! all empty. [`closes_object`] tells the two cases apart, so a
//! fragment becomes a parse failure and the caller's retry runs.

/// `true` when the object that opens at byte `open` of `s` is closed
/// by a matching closer later in `s`. Strings (with `\` escapes) are
/// skipped; `{`/`[` open a level and `}`/`]` close one. `false` when
/// `s[open]` is not `{`.
pub fn closes_object(s: &str, open: usize) -> bool {
    let bytes = s.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return false;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for &b in &bytes[open..] {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests;
