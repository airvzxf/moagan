//! Centralised control-token sanitiser for LLM inputs and outputs
//! (catalog 10-integrada-v0 §D.7.2, roadmap PR-27).
//!
//! Most providers never emit raw ASCII control bytes (`\u{0000}`
//! – `\u{001F}`) or the C1 DEL byte (`\u{007F}`) in their
//! responses — but a small number of upstream quirks do leak them
//! through:
//!
//! - Terminal-escape sequences (`\u{001B}[...m`) pasted by
//!   operators into a brief.
//! - Stray NULs from CSV exports or DB dumps.
//! - DEL bytes from copy-paste of fixed-width text.
//! - C1 control bytes (`\u{0080}`–`\u{009F}`) emitted by some
//!   non-UTF-8 legacy encodings.
//!
//! Before any of this text reaches `serde_json::from_str`, the
//! JSON-tolerant extractor, or the SSE parser, we strip the
//! offending bytes so downstream code never has to defend against
//! them again. The strip preserves `\n`, `\r`, and `\t` because
//! those three are legitimate whitespace in natural-language
//! prompts and in JSON string values.
//!
//! The function returns [`Cow<'_, str>`] so the no-op path (no
//! control bytes present) borrows from the input without
//! allocating. That keeps the cost negligible on the hot path.

use std::borrow::Cow;

/// Strip ASCII control bytes and the C1 DEL byte from `s`,
/// preserving `\n`, `\r`, and `\t`.
///
/// Removed:
///
/// - `\u{0000}` – `\u{001F}` except `\n` (`\u{000A}`), `\r`
///   (`\u{000D}`), and `\t` (`\u{0009}`).
/// - `\u{007F}` (DEL).
///
/// Preserved verbatim:
///
/// - All other Unicode codepoints (including the BOM
///   `\u{FEFF}` and the zero-width spaces `\u{200B}`–`\u{200D}`
///   — those live far outside the ASCII control range and are
///   out of scope for this helper).
/// - C1 control bytes (`\u{0080}`–`\u{009F}`) — also out of
///   scope; the input is expected to be valid UTF-8.
///
/// When nothing was removed, the function returns
/// [`Cow::Borrowed`] pointing at the original slice so the call
/// site pays no allocation cost. When something was removed it
/// returns [`Cow::Owned`] holding the cleaned-up string.
pub fn strip(s: &str) -> Cow<'_, str> {
    if !needs_strip(s) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if should_keep(c) {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Cheap predicate: does `s` contain at least one byte that the
/// strip pass would remove? Used to skip the allocation on the
/// hot path.
fn needs_strip(s: &str) -> bool {
    s.chars().any(|c| !should_keep(c))
}

/// Single source of truth for "is this char allowed through the
/// strip?" Pulled out so the predicate is shared between the
/// fast-path scan and the allocation path; if either ever drifts
/// the strip would silently lose or leak characters.
fn should_keep(c: char) -> bool {
    if c == '\n' || c == '\r' || c == '\t' {
        return true;
    }
    let code = c as u32;
    code >= 0x20 && code != 0x7F
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure-clean input: no control bytes anywhere. The helper
    /// must hand back a `Cow::Borrowed` so callers on the hot
    /// path pay zero allocation.
    #[test]
    fn strip_returns_borrowed_when_clean() {
        let input = "the quick brown fox jumps over the lazy dog";
        let out = strip(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    /// Pure-clean input that contains BOM and zero-width
    /// characters: those are deliberately out of scope (they
    /// live in the BMP far from the ASCII control range), so
    /// the strip must preserve them verbatim and still return a
    /// `Cow::Borrowed`.
    #[test]
    fn strip_preserves_bom_and_zero_width() {
        let mut input = String::from("hello");
        input.insert(0, '\u{FEFF}'); // BOM
        input.push('\u{200B}'); // zero-width space
        input.push('\u{200C}'); // zero-width non-joiner
        input.push('\u{200D}'); // zero-width joiner
        input.push_str("world");
        let out = strip(&input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    /// Control bytes (`\u{0000}`–`\u{001F}`) other than
    /// `\n`/`\r`/`\t` are dropped. The output must be a
    /// `Cow::Owned` and must equal the input with those bytes
    /// removed.
    #[test]
    fn strip_removes_ascii_control_bytes() {
        let mut input = String::from("a");
        for c in [
            '\0', '\u{01}', '\u{02}', '\u{03}', '\u{04}', '\u{05}', '\u{06}', '\u{07}',
            '\u{08}', // \b
            '\u{0B}', // VT
            '\u{0C}', // FF
            '\u{0E}', '\u{0F}', '\u{10}', '\u{11}', '\u{12}', '\u{13}', '\u{14}', '\u{15}',
            '\u{16}', '\u{17}', '\u{18}', '\u{19}',
            '\u{1A}', // SUB (kept here as a generic control)
            '\u{1C}', '\u{1D}', '\u{1E}', '\u{1F}',
        ] {
            input.push(c);
        }
        input.push('b');
        let out = strip(&input);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(out.as_ref(), "ab");
    }

    /// DEL byte (`\u{007F}`) is dropped alongside the ASCII
    /// control range. DEL is one of the few non-range-0x20
    /// bytes the spec explicitly calls out.
    #[test]
    fn strip_removes_del_byte() {
        let input = "left\u{007F}right";
        let out = strip(input);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(out.as_ref(), "leftright");
    }

    /// Whitespace (`\n`, `\r`, `\t`) is preserved verbatim. The
    /// strip is a control-byte filter, not a whitespace
    /// trimmer.
    #[test]
    fn strip_preserves_newline_cr_tab() {
        let input = "line1\nline2\rline3\tcol3";
        let out = strip(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    /// Mixed payload: a natural-language string with embedded
    /// NULs, ESC sequences, and DEL bytes (the kind of garbage
    /// an operator might paste from a terminal). The cleaned
    /// output must drop the raw control bytes (the ESC, the
    /// NUL, the DEL) while preserving the printable ASCII
    /// around them (the bracket-delimited parameters of the
    /// escape sequence). Only the bytes in the control range
    /// are touched — the helper does not understand terminal
    /// escape syntax.
    #[test]
    fn strip_cleans_terminal_paste_payload() {
        let mut input = String::from("hello ");
        input.push_str("\u{001B}[1;31m");
        input.push_str("world");
        input.push('\0');
        input.push_str("\u{007F}!");
        let out = strip(&input);
        // ESC, NUL, and DEL gone; the rest (including the
        // `[1;31m` parameters of the escape sequence) is left
        // intact because those are printable ASCII.
        assert_eq!(out.as_ref(), "hello [1;31mworld!");
    }

    /// Unicode codepoints above `\u{007F}` (Latin-1 supplement,
    /// BMP, SMP) must pass through untouched. The filter only
    /// touches the ASCII control range and DEL.
    #[test]
    fn strip_passes_through_non_ascii_unicode() {
        let input = "café — こんにちは — 🦀 — ñ";
        let out = strip(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    /// Empty input is a no-op. The helper returns `Cow::Borrowed`
    /// of the empty slice — no allocation, no panic.
    #[test]
    fn strip_on_empty_string_is_borrowed() {
        let out = strip("");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), "");
    }

    /// String consisting entirely of control bytes becomes an
    /// empty owned string. The output is still safe to feed to
    /// downstream parsers (which would otherwise reject it).
    #[test]
    fn strip_on_all_controls_yields_empty() {
        let mut input = String::new();
        for c in ['\0', '\u{07}', '\u{1F}', '\u{007F}'] {
            input.push(c);
        }
        let out = strip(&input);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(out.as_ref(), "");
    }

    /// Boundary check on the inclusive ends of the control
    /// range. `\u{001F}` is the last removed byte; `\u{0020}` is
    /// the first preserved byte; `\u{007E}` is the last ASCII
    /// printable; `\u{007F}` is DEL (removed).
    #[test]
    fn strip_respects_range_boundaries() {
        let input = "\u{001F}a\u{0020}b\u{007E}c\u{007F}d";
        let out = strip(input);
        assert_eq!(out.as_ref(), "a b~cd");
    }
}
