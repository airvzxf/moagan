//! Centralized size caps for LLM payloads (D.29.2).
//!
//! The caps live in one module so a refactor that tightens or
//! loosens one budget only touches a single constant. Callers go
//! through [`check_size`] so the failure mode (`Error::PayloadTooLarge`
//! with a `{label}: {bytes} > {cap}` payload) is uniform across
//! the prompt, response, and attachment paths.
//!
//! Three caps ship today:
//!
//! | Constant | Default | Use |
//! |---|---|---|
//! | [`MAX_PROMPT_BYTES`] | 250 KiB | Cap on the normalised raw prompt in the intake phase. Briefs larger than this are truncated, not errored. |
//! | [`MAX_RESPONSE_BYTES`] | 10 MiB | Cap on a decoded response body. Larger bodies surface as `Error::PayloadTooLarge`. |
//! | [`MAX_ATTACHMENT_BYTES`] | 50 MiB | Cap on a single `--attach`ed file. Surfaces as `Error::PayloadTooLarge`. |
//!
//! The intake phase clamps at [`MAX_PROMPT_BYTES`] (truncate, not
//! error) so a 250 KiB paste does not abort the whole run; the
//! response and attachment paths hard-fail because the operator
//! explicitly opted into receiving that body. The asymmetry is
//! deliberate: prompts are operator-typed, responses are
//! provider-typed.

use crate::error::{Error, Result};

/// D.29.2: hard cap on the normalised raw prompt (250 KiB).
/// Briefs larger than this are truncated by
/// [`crate::phases::intake::normalize_raw_prompt`] so the LLM
/// context window cannot be exhausted by a single paste. Sized
/// at 250 KiB rather than the legacy 256 KiB so a refactor that
/// moves the cap does not silently re-widen the budget.
pub const MAX_PROMPT_BYTES: usize = 250 * 1024;

/// D.29.2: hard cap on a decoded response body (10 MiB).
/// A response larger than this fails the call with
/// [`Error::PayloadTooLarge`] so the dispatcher never has to
/// hold a 100 MB string in memory. Sized at 10 MiB so the
/// longest plausible structured-output (a fully-populated
/// `Intake` JSON, the synthesiser's report, etc.) fits with
/// plenty of headroom.
pub const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// D.29.2: hard cap on a single `--attach`ed file (50 MiB).
/// The cap is enforced on the way in so a stray `dd` of a 100
/// GiB dump cannot saturate the disk. Future callers
/// (`moagan run --attach <path>`) call
/// [`check_size`] with this cap.
pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

/// D.29.2: helper that returns `Err(Error::PayloadTooLarge)`
/// when `bytes > cap`.
///
/// `label` flows into the error payload as
/// `"{label}: {bytes} > {cap}"` so the post-mortem log can
/// pinpoint which budget blew (`"prompt"`, `"response"`, or
/// `"attachment"`). Callers pass the actual byte length of the
/// payload (e.g. `req.user.len()`); the helper does not try to
/// guess. Equal-to-cap is allowed (`bytes == cap` is `Ok`); the
/// cap is a strict upper bound but not a strict-less-than
/// bound.
///
/// ```rust,ignore
/// use crate::llm::size_limits::{check_size, MAX_PROMPT_BYTES};
/// check_size("prompt", raw.len(), MAX_PROMPT_BYTES)?;
/// ```
pub fn check_size(label: &str, bytes: usize, cap: usize) -> Result<()> {
    tracing::trace!(label, bytes, cap, "check_size: evaluating payload cap");
    if bytes > cap {
        tracing::warn!(
            label,
            bytes,
            cap,
            "check_size: payload exceeds cap (PayloadTooLarge)"
        );
        Err(Error::PayloadTooLarge(format!("{label}: {bytes} > {cap}")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D.29.2: `check_size` returns `Ok(())` when the payload is
    /// within the cap. The boundary `bytes == cap` must also
    /// pass — the cap is an upper bound, not a strict-less-than.
    #[test]
    fn check_size_allows_payloads_within_cap() {
        assert!(check_size("prompt", 0, MAX_PROMPT_BYTES).is_ok());
        assert!(check_size("prompt", 1, MAX_PROMPT_BYTES).is_ok());
        assert!(check_size("prompt", MAX_PROMPT_BYTES, MAX_PROMPT_BYTES).is_ok());
        assert!(check_size("response", 0, MAX_RESPONSE_BYTES).is_ok());
        assert!(
            check_size("response", MAX_RESPONSE_BYTES, MAX_RESPONSE_BYTES).is_ok(),
            "bytes == cap must pass"
        );
        assert!(check_size("attachment", 0, MAX_ATTACHMENT_BYTES).is_ok());
        assert!(check_size("attachment", 1024 * 1024, MAX_ATTACHMENT_BYTES).is_ok());
    }

    /// D.29.2: a payload one byte over the cap returns
    /// `Err(Error::PayloadTooLarge(...))` whose `Display`
    /// includes the label, the byte count, and the cap. Pin the
    /// wire form so a future refactor that rewords the message
    /// trips the test.
    #[test]
    fn check_size_rejects_payloads_above_cap() {
        let err = check_size("response", MAX_RESPONSE_BYTES + 1, MAX_RESPONSE_BYTES)
            .expect_err("over-cap must fail");
        match err {
            Error::PayloadTooLarge(msg) => {
                assert!(
                    msg.contains("response"),
                    "label must propagate, got {msg:?}"
                );
                assert!(
                    msg.contains(&(MAX_RESPONSE_BYTES + 1).to_string()),
                    "byte count must propagate, got {msg:?}"
                );
                assert!(
                    msg.contains(&MAX_RESPONSE_BYTES.to_string()),
                    "cap must propagate, got {msg:?}"
                );
            }
            other => panic!("expected Error::PayloadTooLarge, got {other:?}"),
        }
        // Prompt path.
        assert!(matches!(
            check_size("prompt", MAX_PROMPT_BYTES + 1, MAX_PROMPT_BYTES),
            Err(Error::PayloadTooLarge(_))
        ));
        // Attachment path.
        assert!(matches!(
            check_size("attachment", MAX_ATTACHMENT_BYTES + 1, MAX_ATTACHMENT_BYTES),
            Err(Error::PayloadTooLarge(_))
        ));
    }

    /// D.29.2: the constants themselves must be exactly the
    /// documented values. A future refactor that bumps the cap
    /// (intentionally or not) trips the test, surfacing the
    /// change as a deliberate diff in this file.
    #[test]
    fn caps_have_documented_values() {
        assert_eq!(MAX_PROMPT_BYTES, 250 * 1024, "prompt cap must stay 250 KiB");
        assert_eq!(
            MAX_RESPONSE_BYTES,
            10 * 1024 * 1024,
            "response cap must stay 10 MiB"
        );
        assert_eq!(
            MAX_ATTACHMENT_BYTES,
            50 * 1024 * 1024,
            "attachment cap must stay 50 MiB"
        );
    }

    /// D.29.2: `MAX_RESPONSE_BYTES > MAX_PROMPT_BYTES` and
    /// `MAX_ATTACHMENT_BYTES > MAX_RESPONSE_BYTES`. The
    /// hierarchy reflects the use case: prompts are operator-
    /// typed (small), responses are model-generated (medium),
    /// attachments are file dumps (large). Pin the ordering so
    /// a future refactor that swaps the constants trips the
    /// test.
    #[test]
    fn caps_hold_expected_ordering() {
        const {
            assert!(
                MAX_PROMPT_BYTES < MAX_RESPONSE_BYTES,
                "prompt cap must be smaller than response cap"
            );
            assert!(
                MAX_RESPONSE_BYTES < MAX_ATTACHMENT_BYTES,
                "response cap must be smaller than attachment cap"
            );
        }
    }
}
