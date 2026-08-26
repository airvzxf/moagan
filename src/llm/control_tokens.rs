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
//!
//! # Chat-template markers
//!
//! [`strip_chat_template_tokens`] covers a second, unrelated class
//! of noise: the template markers that chat-tuned open-source models
//! (ChatML, Llama-3, Mistral, Qwen) sometimes leak into their own
//! output — `<|im_start|>`, `<system>`, `[BOS]` and friends. Those
//! are ordinary printable text, so the ASCII strip above cannot see
//! them, yet they break the JSON extractor just as effectively.
//!
//! [`strip_response_text`] chains both passes for call sites on the
//! model-response path that want the whole sanitising treatment in
//! one line.

use std::borrow::Cow;

use once_cell::sync::Lazy;
use regex::Regex;

/// Every chat-template marker the response path strips, as a single
/// alternation. One compiled regex means one pass over the input and
/// one `replace_all` allocation at most.
///
/// Families, in the order they appear in the alternation:
///
/// - **Bracket tokens** (`[BOS]`, `[EOS]`) — matched
///   case-insensitively via the scoped `(?i:…)` group, with an
///   optional single space inside the brackets so the observed
///   `[BO S]` / `[EOS ]` typos are caught too. These come first
///   because they are the only family that can otherwise be
///   mistaken for a JSON array delimiter.
/// - **ChatML pair tokens** — `<|im_start|>`, `<|im_end|>` and the
///   sibling end-of-turn markers emitted by Llama-3 / Qwen /
///   Gemma chat templates.
/// - **Section markers** — `<system>`, `<assistant>`, `<user>` and
///   their closing forms, matched case-sensitively because
///   lowercase is what the templates emit and a looser match would
///   start eating legitimate prose.
static CHAT_TEMPLATE_MARKERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?i:\[BO ?S\]|\[EOS ?\])",
        r"|<\|im_start\|>|<\|im_end\|>|<\|eot_id\|>|<\|end_of_turn\|>|<\|endoftext\|>",
        r"|</?system>|</?assistant>|</?user>",
    ))
    .expect("invalid chat-template marker regex")
});

/// Strip chat-template control markers from `input`, replacing each
/// with a single space.
///
/// Open-source chat-template models (ChatML, Llama-3, Mistral, Qwen)
/// occasionally leak their own template markers into the response
/// body, particularly when JSON mode nudges the model into opening
/// with a section header. Those markers derail
/// [`crate::llm::json_extractor::extract_tolerant_json`]: `[BOS]` in
/// particular reads as a JSON array delimiter, so the extractor
/// locks onto it, finds the bare word `BOS` where a JSON value
/// should be, and reports `UnbalancedBraces` on an otherwise-valid
/// payload.
///
/// The replacement is a single space rather than the empty string so
/// that word boundaries survive: `{"a":1}<|im_end|>{"b":2}` must not
/// collapse into two adjacent values with nothing between them.
///
/// # Scope
///
/// This is a blunt textual pass — it has no notion of JSON string
/// boundaries, so a marker sitting *inside* a string value (say
/// `{"note":"<system>"}`) would be replaced too. That is why the
/// response path only reaches this helper *after* a direct
/// `serde_json` parse of the untouched payload has already failed:
/// well-formed JSON is returned before the strip ever runs. See
/// [`crate::phases::util::parse_model_json_traced`].
///
/// Returns [`Cow::Borrowed`] when no marker was present, so the
/// common clean-response path allocates nothing.
pub fn strip_chat_template_tokens(input: &str) -> Cow<'_, str> {
    let out = CHAT_TEMPLATE_MARKERS.replace_all(input, " ");
    if matches!(out, Cow::Owned(_)) {
        tracing::debug!(
            input_len = input.len(),
            "control_tokens: chat-template markers stripped"
        );
    }
    out
}

/// Response-path entry point: run both sanitising passes over `input`.
///
/// Applies [`strip_chat_template_tokens`] (chat-template markers)
/// followed by [`strip`] (ASCII control bytes and DEL), so a call
/// site only needs one line to get a fully sanitised payload. The
/// chat-template pass runs first because its replacement introduces
/// only spaces, which the control-byte pass is happy to leave alone.
///
/// Returns [`Cow::Borrowed`] when neither pass changed anything.
pub fn strip_response_text(input: &str) -> Cow<'_, str> {
    match strip_chat_template_tokens(input) {
        // Nothing stripped: the borrow still points at `input`, so
        // the control-byte pass can keep borrowing from it.
        Cow::Borrowed(borrowed) => strip(borrowed),
        // Markers were stripped: `owned` is a local, so the second
        // pass has to hand back an owned string of its own.
        Cow::Owned(owned) => Cow::Owned(strip(&owned).into_owned()),
    }
}

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
    tracing::debug!(
        input_len = s.len(),
        output_len = out.len(),
        "control_tokens: ASCII control bytes stripped"
    );
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

    /// ChatML pair tokens wrapping a JSON payload are removed. The
    /// role tag (`assistant`) is deliberately *not* stripped — it
    /// is ordinary prose, and dropping the prose prefix is the JSON
    /// extractor's job, not this helper's. What matters here is
    /// that the markers are gone and the JSON body survives
    /// byte-for-byte.
    #[test]
    fn strip_chat_template_tokens_chatml_markers() {
        let input = "<|im_start|>assistant\n{\"a\":1}<|im_end|>";
        let out = strip_chat_template_tokens(input);
        assert!(!out.contains("<|im_start|>"));
        assert!(!out.contains("<|im_end|>"));
        assert!(out.contains("{\"a\":1}"));
        assert_eq!(out.trim(), "assistant\n{\"a\":1}");
    }

    /// Section markers are removed and the text they wrapped is
    /// left behind.
    #[test]
    fn strip_chat_template_tokens_section_markers() {
        let input = "<system>foo</system>";
        let out = strip_chat_template_tokens(input);
        assert_eq!(out.trim(), "foo");
    }

    /// Bracket tokens are matched case-insensitively and tolerate
    /// the single-space typo variants (`[BO S]`, `[EOS ]`) that
    /// show up in real model output.
    #[test]
    fn strip_chat_template_tokens_brackets_with_typo() {
        let out = strip_chat_template_tokens("[BO S]hello[EOS ]");
        assert_eq!(out.trim(), "hello");
        // Canonical spellings and lowercase both match.
        assert_eq!(strip_chat_template_tokens("[BOS]hi[EOS]").trim(), "hi");
        assert_eq!(strip_chat_template_tokens("[bos]hi[eos]").trim(), "hi");
    }

    /// Marker-free input must borrow: the clean-response path is
    /// the common case and it should not allocate.
    #[test]
    fn strip_chat_template_tokens_returns_borrowed_on_no_op() {
        let input = "{\"x\":1}";
        let out = strip_chat_template_tokens(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }

    /// Families mixed in one payload. The section marker sitting
    /// between the opening brace and the first key is eaten, while
    /// the JSON braces either side of it are preserved — the
    /// replacement is a space, so the value stays parseable.
    #[test]
    fn strip_chat_template_tokens_combined_pass() {
        let input = "<|im_start|><system>{</system>\"a\":1}<|im_end|>";
        let out = strip_chat_template_tokens(input);
        assert_eq!(out.trim(), "{ \"a\":1}");
    }

    /// The combined response-path helper runs both passes: the
    /// chat-template markers *and* the ASCII control bytes that
    /// [`strip`] handles.
    #[test]
    fn strip_response_text_applies_both_passes() {
        let input = "<|im_start|>\u{0000}{\"a\":1}\u{007F}<|im_end|>";
        let out = strip_response_text(input);
        assert_eq!(out.trim(), "{\"a\":1}");
    }

    /// Input clean under both passes borrows end-to-end — the
    /// chained helper must not allocate just because it composes
    /// two `Cow`-returning functions.
    #[test]
    fn strip_response_text_returns_borrowed_on_no_op() {
        let input = "{\"x\":1}";
        let out = strip_response_text(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), input);
    }
}
