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
use crate::error::Error;

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

/// Map a provider / call error to the closest `RetryReason`. The
/// retry loop in `phase.rs::call_with_retry_parse` consults this on
/// the failure path so the budget can be looked up per-attempt.
///
/// Mapping rationale (D.21.6, best-effort):
///
/// - `Timeout` -> `Timeout` (deadline-driven retry, 1 extra attempt
///   in Standard).
/// - `PlanExhausted` -> `RateLimit` (semantically the same: the
///   provider refused to keep serving this account / model until
///   the quota resets; the operator action is "wait and retry").
/// - `SchemaViolation` -> `Schema` (validated output failed the
///   contract; the JSON itself parsed).
/// - `Provider`, `Cache`, `Io` -> `Transport` (catch-all bucket
///   for network / disk / upstream blips; the retry loop treats
///   them identically and relies on the budget matrix for the
///   per-mode cap).
/// - `MockExhausted` -> `Truncated` (the mock ran out of canned
///   responses; a real retry would not help, so this is the
///   closest "non-actionable" bucket — `Truncated` is also
///   non-actionable in production).
/// - Everything else (`InvalidArgs`, `InvalidApiKey`,
///   `InvalidState`, `Cancelled`, `Cancel`) -> `Transport` (the
///   classification is moot because the retry loop bails on the
///   first error in these cases, but a deterministic mapping
///   keeps the matrix total over `Error`).
pub fn reason_from_error(err: &Error) -> RetryReason {
    match err {
        Error::Timeout(_) => RetryReason::Timeout,
        Error::PlanExhausted(_) => RetryReason::RateLimit,
        Error::SchemaViolation(_) => RetryReason::Schema,
        Error::Provider(_) | Error::Cache(_) | Error::Io(_) => RetryReason::Transport,
        Error::MockExhausted => RetryReason::Truncated,
        Error::InvalidArgs(_)
        | Error::InvalidApiKey(_)
        | Error::InvalidState(_)
        | Error::NeedsInput(_)
        | Error::Cancelled(_)
        | Error::Cancel(_) => RetryReason::Transport,
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

    // --- reason_from_error classification -----------------------------
    // The retry loop in phase.rs consults this helper on the
    // failure path to look up the per-(mode, reason) budget. Each
    // pin below corresponds to a row of the spec's Error ->
    // RetryReason mapping (D.21.6).

    /// `Error::Timeout` maps to `RetryReason::Timeout` so the
    /// Standard path takes its single retry slot on deadline
    /// failures.
    #[test]
    fn reason_from_error_timeout_maps_to_timeout_reason() {
        assert_eq!(
            reason_from_error(&Error::Timeout("x".into())),
            RetryReason::Timeout,
        );
    }

    /// `Error::SchemaViolation` maps to `RetryReason::Schema` so
    /// the budget looks up the parse / schema row (one attempt
    /// with `use_json_repair = true` in Standard, two in Deep).
    #[test]
    fn reason_from_error_schema_violation_maps_to_schema_reason() {
        assert_eq!(
            reason_from_error(&Error::SchemaViolation("x".into())),
            RetryReason::Schema,
        );
    }

    /// `Error::Provider` is the generic catch-all for HTTP
    /// failures; the retry loop collapses it onto
    /// `RetryReason::Transport` so the budget matrix stays
    /// total over `Error`.
    #[test]
    fn reason_from_error_provider_maps_to_transport_reason() {
        assert_eq!(
            reason_from_error(&Error::Provider("x".into())),
            RetryReason::Transport,
        );
    }

    /// `Error::PlanExhausted` is the rate-limit analogue: the
    /// provider refused to keep serving. The retry loop
    /// surfaces it as `RetryReason::RateLimit` so Deep mode
    /// takes its 3-attempt slot.
    #[test]
    fn reason_from_error_plan_exhausted_maps_to_rate_limit_reason() {
        assert_eq!(
            reason_from_error(&Error::PlanExhausted("x".into())),
            RetryReason::RateLimit,
        );
    }

    /// `Error::MockExhausted` is a non-actionable failure: the
    /// mock provider ran out of canned responses. A real retry
    /// would not help, so the helper maps to
    /// `RetryReason::Truncated` (the closest "no retry
    /// possible" bucket).
    #[test]
    fn reason_from_error_mock_exhausted_maps_to_truncated_reason() {
        assert_eq!(
            reason_from_error(&Error::MockExhausted),
            RetryReason::Truncated,
        );
    }

    /// `Error::Cancelled` / `Error::Cancel` are operator /
    /// signal failures; the retry loop bails on the first
    /// error in either case, but the classification is
    /// deterministic so the budget lookup is total.
    #[test]
    fn reason_from_error_cancellation_maps_to_transport_reason() {
        assert_eq!(
            reason_from_error(&Error::Cancelled("x".into())),
            RetryReason::Transport,
        );
        assert_eq!(
            reason_from_error(&Error::Cancel(crate::error::CancelSignal)),
            RetryReason::Transport,
        );
    }

    /// `Error::Io` and `Error::Cache` both share the transport
    /// bucket: the retry loop treats them identically and lets
    /// the per-mode budget decide the cap.
    #[test]
    fn reason_from_error_io_and_cache_map_to_transport_reason() {
        assert_eq!(
            reason_from_error(&Error::Io(crate::error::IoError::Raw(
                std::io::Error::other("x"),
            ))),
            RetryReason::Transport,
        );
        assert_eq!(
            reason_from_error(&Error::Cache("x".into())),
            RetryReason::Transport,
        );
    }

    /// Operator errors (`InvalidArgs`, `InvalidApiKey`,
    /// `InvalidState`) never reach the retry loop in practice
    /// (they're surfaced before the first provider call), but
    /// the mapping is deterministic so the helper stays
    /// total over `Error`.
    #[test]
    fn reason_from_error_operator_errors_map_to_transport_reason() {
        assert_eq!(
            reason_from_error(&Error::InvalidArgs("x".into())),
            RetryReason::Transport,
        );
        assert_eq!(
            reason_from_error(&Error::InvalidApiKey("x".into())),
            RetryReason::Transport,
        );
        assert_eq!(
            reason_from_error(&Error::InvalidState("x".into())),
            RetryReason::Transport,
        );
    }

    /// Sanity: composing `reason_from_error` with `budget_for`
    /// yields the per-mode cap the spec wants. Standard +
    /// transport = 2 attempts, the legacy behaviour for the
    /// common transient 5xx case.
    #[test]
    fn reason_from_error_then_budget_for_round_trips_through_matrix() {
        let cases = [
            (Error::Timeout("x".into()), Mode::Standard, 2),
            (Error::Provider("x".into()), Mode::Standard, 2),
            (Error::PlanExhausted("x".into()), Mode::Deep, 3),
            (Error::SchemaViolation("x".into()), Mode::Deep, 2),
            (Error::Provider("x".into()), Mode::Fast, 1),
        ];
        for (err, mode, expected) in cases {
            let b = budget_for(mode, reason_from_error(&err));
            assert_eq!(b.max_attempts, expected, "err={err:?} mode={mode:?}");
        }
    }
}
