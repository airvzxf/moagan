//! Last-resort repairs of the model-JSON recovery chain
//! ([`crate::phases::util::parse_model_json_traced`]).
//!
//! The per-cell curator (spike S2) sometimes writes one closer too many
//! — `[[1,2],[3,4]]]`, `}]}],"assign"` — or a string value without its
//! opening quote (`[3, 5, 3 keeps the price."]`). [`repair_slips`]
//! fixes those two shapes. The chain runs it only after every older
//! repair failed, so an answer the chain already recovered never
//! changes. [`continues_after`] spots the stray closer that ends the
//! outer value early, which the tolerant extractor would otherwise
//! accept as a complete (but shortened) answer.

use crate::phases::util::{RepairKind, RepairTrace};

/// `true` when the text after a complete JSON value goes on as JSON:
/// its first non-whitespace byte is `,`, `]` or `}`. Prose after the
/// value (or nothing) is `false`.
pub fn continues_after(rest: &str) -> bool {
    matches!(
        rest.bytes().find(|b| !b.is_ascii_whitespace()),
        Some(b',' | b']' | b'}')
    )
}

/// Drop every closer (`]` or `}`, outside strings) that does not match
/// the innermost open container, and every closer that would close the
/// outermost one while [`continues_after`] holds for the rest of the
/// text (the model keeps writing the same value). `None` when nothing
/// was dropped.
pub fn drop_stray_closers(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut dropped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            out.push(b);
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                out.push(b);
            }
            b'{' | b'[' => {
                stack.push(b);
                out.push(b);
            }
            b'}' | b']' => {
                let opener = if b == b'}' { b'{' } else { b'[' };
                let closes_outermost = stack.len() == 1 && continues_after(&s[i + 1..]);
                if stack.last() == Some(&opener) && !closes_outermost {
                    stack.pop();
                    out.push(b);
                } else {
                    dropped = true;
                }
            }
            _ => out.push(b),
        }
    }
    if dropped {
        String::from_utf8(out).ok()
    } else {
        None
    }
}

/// Insert the opening quote of a string value written without one.
/// A value position (after `[`, after `,` inside an array, after `:`)
/// that starts with something other than `"`, `{`, `[`, a closer, or a
/// JSON literal (`true`, `false`, `null`, a number starting with a
/// digit or `-`) followed by `,`, `]`, `}` or the end, is quoted when
/// the same line holds a `"` followed by `,`, `]`, `}` or the end:
/// `[3, 5, 3 keeps the price."]` → `[3, 5, "3 keeps the price."]`.
/// `None` when nothing changed.
pub fn quote_bare_strings(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 8);
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut expect_value = false;
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            out.push(b);
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            out.push(b);
            i += 1;
            continue;
        }
        if expect_value && !matches!(b, b'"' | b'{' | b'[' | b'}' | b']') {
            expect_value = false;
            let literal_end = literal_end(bytes, i);
            if (!is_literal(&s[i..literal_end]) || !ends_value(bytes, literal_end))
                && let Some(quote) = closing_quote(bytes, i)
            {
                out.push(b'"');
                out.extend_from_slice(&bytes[i..=quote]);
                changed = true;
                i = quote + 1;
                continue;
            }
            out.extend_from_slice(&bytes[i..literal_end.max(i + 1)]);
            i = literal_end.max(i + 1);
            continue;
        }
        expect_value = false;
        match b {
            b'"' => in_string = true,
            b'{' => stack.push(b),
            b'[' => {
                stack.push(b);
                expect_value = true;
            }
            b'}' | b']' => {
                stack.pop();
            }
            b':' => expect_value = stack.last() == Some(&b'{'),
            b',' => expect_value = stack.last() == Some(&b'['),
            _ => {}
        }
        out.push(b);
        i += 1;
    }
    if changed {
        String::from_utf8(out).ok()
    } else {
        None
    }
}

/// End of the run of literal-like bytes (ASCII letters, digits, `+`,
/// `-`, `.`) that starts at `start`.
fn literal_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
        .map_or(bytes.len(), |n| start + n)
}

/// `true`, `false`, `null`, or a number that starts with a digit or `-`.
fn is_literal(token: &str) -> bool {
    matches!(token, "true" | "false" | "null")
        || (token.starts_with(|c: char| c.is_ascii_digit() || c == '-')
            && token.parse::<f64>().is_ok())
}

/// `true` when the first non-whitespace byte at or after `at` ends a
/// value: `,`, `]`, `}` or the end of the text.
fn ends_value(bytes: &[u8], at: usize) -> bool {
    matches!(
        bytes[at..].iter().find(|b| !b.is_ascii_whitespace()),
        None | Some(b',' | b']' | b'}')
    )
}

/// Index of the first `"` on the line that starts at `start`, when the
/// value it closes ends there ([`ends_value`] after it).
fn closing_quote(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start..]
        .iter()
        .take_while(|&&b| b != b'\n')
        .position(|&b| b == b'"')
        .map(|n| start + n)?;
    ends_value(bytes, quote + 1).then_some(quote)
}

/// Run [`drop_stray_closers`], then [`quote_bare_strings`], on `s`.
/// Returns the repaired text (`None` when neither changed anything)
/// and one trace per repair that changed it: [`RepairKind::Bracket`]
/// for dropped closers, [`RepairKind::Quote`] for inserted quotes.
pub fn repair_slips(s: &str) -> (Option<String>, Vec<RepairTrace>) {
    let mut events = Vec::new();
    let mut current: Option<String> = None;
    if let Some(patched) = drop_stray_closers(s) {
        events.push(RepairTrace {
            kind: RepairKind::Bracket,
            bytes_before: s.len(),
            bytes_after: patched.len(),
        });
        current = Some(patched);
    }
    let base = current.as_deref().unwrap_or(s);
    if let Some(patched) = quote_bare_strings(base) {
        events.push(RepairTrace {
            kind: RepairKind::Quote,
            bytes_before: base.len(),
            bytes_after: patched.len(),
        });
        current = Some(patched);
    }
    (current, events)
}

#[cfg(test)]
mod tests;
