//! Per-mode retry budget matrix (D.21.6).
//!
//! The retry loop in `phase.rs::call_with_retry_parse` currently
//! uses a hard-coded `n_attempts = 5`. This module exposes the
//! per-`(mode, reason)` budget the spec wants, so the next
//! integration step can replace the hard-coded value without
//! changing call sites.
//!
//! Compliance: proposal-03 §D.21.6 (T16-06 §2.5).
//!
//! Conventions:
//! - `max_attempts` is the number of HTTP attempts the loop is
//!   allowed to issue (1 = no retry, 2 = one retry, 3 = two
//!   retries, etc.).
//! - `use_json_repair` is `true` only when the failure reason is
//!   a parse / schema mismatch that the local JSON repair pass
//!   can plausibly fix without re-issuing the call.

use crate::cli::Mode;

/// Why the retry loop is being consulted. Mirrors the failure
/// classification used by `phase.rs::call_with_retry_parse` and
/// `src/llm/retry.rs::Retry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// Transport-level failure (DNS, TCP, TLS, body read).
    Transport,
    /// Provider-side rate limit (`HTTP 429`).
    RateLimit,
    /// Output did not parse as the expected shape.
    Parse,
    /// Output parsed but failed the contract / schema check.
    Schema,
    /// The call exceeded its wall-clock deadline.
    Timeout,
    /// The model stopped early because of `finish_reason:
    /// truncated`.
    Truncated,
}

/// One row of the per-mode retry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBudget {
    /// Number of attempts the loop may issue.
    pub max_attempts: u32,
    /// `true` when the local JSON repair pass should be invoked
    /// before deciding the parse / schema failure is final.
    pub use_json_repair: bool,
}

/// Look up the retry budget for `(mode, reason)`. The matrix
/// matches proposal-03 §D.21.6 verbatim — keep it in sync when
/// tweaking either input.
///
/// Behavioural rules:
///
/// - `Fast`, `Explore`, `Batch`: no retries regardless of reason.
///   These modes are designed to be CI-friendly and predictable,
///   so a 5xx / 429 surfaces immediately.
/// - `Standard`: one extra attempt for transport / rate-limit /
///   timeout / truncated failures; parse / schema failures also
///   get one attempt but with the local JSON repair pass
///   enabled.
/// - `Deep`: same as `Standard` for transport / timeout /
///   truncated; parse / schema failures get two attempts; rate
///   limits get three (the heavy path tolerates a transient
///   throttle and is the most expensive to restart).
pub fn budget_for(mode: Mode, reason: RetryReason) -> RetryBudget {
    use Mode::*;
    use RetryReason::*;
    match (mode, reason) {
        (Fast, _) => RetryBudget {
            max_attempts: 1,
            use_json_repair: matches!(reason, Parse | Schema),
        },
        (Standard, Parse | Schema) => RetryBudget {
            max_attempts: 1,
            use_json_repair: true,
        },
        (Standard, _) => RetryBudget {
            max_attempts: 2,
            use_json_repair: false,
        },
        (Deep, Parse | Schema) => RetryBudget {
            max_attempts: 2,
            use_json_repair: true,
        },
        (Deep, RateLimit) => RetryBudget {
            max_attempts: 3,
            use_json_repair: false,
        },
        (Deep, _) => RetryBudget {
            max_attempts: 2,
            use_json_repair: false,
        },
        (Explore, _) => RetryBudget {
            max_attempts: 1,
            use_json_repair: matches!(reason, Parse | Schema),
        },
        (Batch, _) => RetryBudget {
            max_attempts: 1,
            use_json_repair: matches!(reason, Parse | Schema),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Fast` does not retry. Period.
    #[test]
    fn budget_for_fast_any_reason_is_one_attempt() {
        for reason in [
            RetryReason::Transport,
            RetryReason::RateLimit,
            RetryReason::Parse,
            RetryReason::Schema,
            RetryReason::Timeout,
            RetryReason::Truncated,
        ] {
            let b = budget_for(Mode::Fast, reason);
            assert_eq!(b.max_attempts, 1, "reason={reason:?}");
        }
    }

    /// `Standard` allows one retry for transport (the common
    /// transient 5xx).
    #[test]
    fn budget_for_standard_transport_is_two_attempts() {
        let b = budget_for(Mode::Standard, RetryReason::Transport);
        assert_eq!(b.max_attempts, 2);
        assert!(!b.use_json_repair);
    }

    /// `Standard` parse failures use the local repair pass; the
    /// retry budget stays at 1 attempt because repair happens
    /// inline before the next LLM call is considered.
    #[test]
    fn budget_for_standard_parse_uses_json_repair() {
        let b = budget_for(Mode::Standard, RetryReason::Parse);
        assert_eq!(b.max_attempts, 1);
        assert!(b.use_json_repair);
    }

    /// `Deep` rate-limit failures are the only entry in the
    /// matrix that allows three attempts — the heavy path can
    /// absorb the latency hit.
    #[test]
    fn budget_for_deep_rate_limit_is_three_attempts() {
        let b = budget_for(Mode::Deep, RetryReason::RateLimit);
        assert_eq!(b.max_attempts, 3);
        assert!(!b.use_json_repair);
    }

    /// `Deep` parse failures get two attempts with repair on.
    #[test]
    fn budget_for_deep_parse_uses_json_repair() {
        let b = budget_for(Mode::Deep, RetryReason::Parse);
        assert_eq!(b.max_attempts, 2);
        assert!(b.use_json_repair);
    }

    /// `Explore` is single-shot across the board (mirrors `Fast`).
    #[test]
    fn budget_for_explore_any_reason_is_one_attempt() {
        for reason in [
            RetryReason::Transport,
            RetryReason::RateLimit,
            RetryReason::Parse,
            RetryReason::Timeout,
        ] {
            let b = budget_for(Mode::Explore, reason);
            assert_eq!(b.max_attempts, 1, "reason={reason:?}");
        }
    }

    /// `Batch` is single-shot across the board (CI-friendly).
    #[test]
    fn budget_for_batch_any_reason_is_one_attempt() {
        for reason in [
            RetryReason::Transport,
            RetryReason::RateLimit,
            RetryReason::Schema,
            RetryReason::Truncated,
        ] {
            let b = budget_for(Mode::Batch, reason);
            assert_eq!(b.max_attempts, 1, "reason={reason:?}");
        }
    }

    /// `Deep` transport failures retry but do NOT use the JSON
    /// repair pass (the parse layer is irrelevant here).
    #[test]
    fn budget_for_deep_transport_is_two_attempts_no_repair() {
        let b = budget_for(Mode::Deep, RetryReason::Transport);
        assert_eq!(b.max_attempts, 2);
        assert!(!b.use_json_repair);
    }

    /// `Deep` schema failures are equivalent to parse failures
    /// from the retry-budget perspective: two attempts with
    /// repair.
    #[test]
    fn budget_for_deep_schema_uses_json_repair() {
        let b = budget_for(Mode::Deep, RetryReason::Schema);
        assert_eq!(b.max_attempts, 2);
        assert!(b.use_json_repair);
    }

    /// `Standard` timeouts allow one retry (the deadline may
    /// have been hit because of an upstream blip).
    #[test]
    fn budget_for_standard_timeout_is_two_attempts_no_repair() {
        let b = budget_for(Mode::Standard, RetryReason::Timeout);
        assert_eq!(b.max_attempts, 2);
        assert!(!b.use_json_repair);
    }
}
