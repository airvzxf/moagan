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
/// matches proposal-03 §D.21.6 — keep both in sync when
/// tweaking either input.
///
/// Behavioural rules:
///
/// - **All modes now allow retries**, with the budget scaled to
///   the cost of running the mode and the actionability of the
///   failure. The only entry that stays single-shot is
///   `Truncated`, because the model already stopped producing
///   output and a re-issue of the same call would be
///   deterministic.
/// - `Parse` / `Schema` failures get the highest cap (5
///   attempts = 4 retries) across every mode, with the local
///   JSON repair pass enabled: model output non-determinism is
///   the dominant failure class in practice and the repair pass
///   can salvage some malformed bodies without a re-issue.
/// - `Transport` / `Timeout` get 3 attempts in `Fast`,
///   `Explore`, `Batch`, and `Standard` (4 in `Deep` because
///   the heavy path is expensive to restart).
/// - `RateLimit` follows the same envelope as transport in the
///   short modes, bumps to 4 in `Standard`, and is the most
///   generous row in `Deep` (6 attempts = 5 retries).
/// - `Truncated` is 1 attempt in `Fast`, `Explore`, `Batch` and
///   2 attempts in `Standard` / `Deep` (one extra shot in case
///   the truncation was caused by a transient quota blip).
pub fn budget_for(mode: Mode, reason: RetryReason) -> RetryBudget {
    use Mode::*;
    use RetryReason::*;
    // Parse / Schema get the highest cap in every mode; the
    // local JSON repair pass can salvage malformed output
    // without a re-issue.
    const REPAIR_ATTEMPTS: u32 = 5;
    // Short modes: 3 attempts for transients, 1 for truncated.
    const SHORT_TRANSIENT: u32 = 3;
    const SHORT_TRUNCATED: u32 = 1;
    let budget = match (mode, reason) {
        // --- Fast ----------------------------------------------------
        (Fast, Parse | Schema) => RetryBudget {
            max_attempts: REPAIR_ATTEMPTS,
            use_json_repair: true,
        },
        (Fast, RateLimit | Transport | Timeout) => RetryBudget {
            max_attempts: SHORT_TRANSIENT,
            use_json_repair: false,
        },
        (Fast, Truncated) => RetryBudget {
            max_attempts: SHORT_TRUNCATED,
            use_json_repair: false,
        },

        // --- Standard ------------------------------------------------
        (Standard, Parse | Schema) => RetryBudget {
            max_attempts: REPAIR_ATTEMPTS,
            use_json_repair: true,
        },
        (Standard, RateLimit) => RetryBudget {
            max_attempts: 4,
            use_json_repair: false,
        },
        (Standard, Transport | Timeout) => RetryBudget {
            max_attempts: SHORT_TRANSIENT,
            use_json_repair: false,
        },
        (Standard, Truncated) => RetryBudget {
            max_attempts: 2,
            use_json_repair: false,
        },

        // --- Deep (most generous) ------------------------------------
        (Deep, Parse | Schema) => RetryBudget {
            max_attempts: REPAIR_ATTEMPTS,
            use_json_repair: true,
        },
        (Deep, RateLimit) => RetryBudget {
            max_attempts: 6,
            use_json_repair: false,
        },
        (Deep, Transport | Timeout) => RetryBudget {
            max_attempts: 4,
            use_json_repair: false,
        },
        (Deep, Truncated) => RetryBudget {
            max_attempts: 2,
            use_json_repair: false,
        },

        // --- Explore / Batch (same envelope as Fast) -----------------
        (Explore, Parse | Schema) => RetryBudget {
            max_attempts: REPAIR_ATTEMPTS,
            use_json_repair: true,
        },
        (Explore, RateLimit | Transport | Timeout) => RetryBudget {
            max_attempts: SHORT_TRANSIENT,
            use_json_repair: false,
        },
        (Explore, Truncated) => RetryBudget {
            max_attempts: SHORT_TRUNCATED,
            use_json_repair: false,
        },
        (Batch, Parse | Schema) => RetryBudget {
            max_attempts: REPAIR_ATTEMPTS,
            use_json_repair: true,
        },
        (Batch, RateLimit | Transport | Timeout) => RetryBudget {
            max_attempts: SHORT_TRANSIENT,
            use_json_repair: false,
        },
        (Batch, Truncated) => RetryBudget {
            max_attempts: SHORT_TRUNCATED,
            use_json_repair: false,
        },
    };
    tracing::trace!(
        mode = ?mode,
        reason = ?reason,
        max_attempts = budget.max_attempts,
        repair = budget.use_json_repair,
        "budget_for"
    );
    budget
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
///   `InvalidState`, `Cancelled`, `Cancel`, `PayloadTooLarge`) ->
///   `Transport` (the classification is moot because the retry
///   loop bails on the first error in these cases, but a
///   deterministic mapping keeps the matrix total over
///   `Error`). `PayloadTooLarge` lands here because retrying the
///   same call would just receive another oversized body — the
///   cap is a contract, not a flaky transient.
pub fn reason_from_error(err: &Error) -> RetryReason {
    let reason = match err {
        Error::Timeout { .. } => RetryReason::Timeout,
        Error::PlanExhausted { .. } | Error::Throttled { .. } => RetryReason::RateLimit,
        Error::SchemaViolation(_) => RetryReason::Schema,
        Error::Provider { .. } | Error::Cache(_) | Error::Io(_) => RetryReason::Transport,
        Error::MockExhausted => RetryReason::Truncated,
        Error::InvalidArgs(_)
        | Error::InvalidApiKey { .. }
        | Error::InvalidState(_)
        | Error::LockHeld(_)
        | Error::NeedsInput(_)
        | Error::DiscoveryQualityTooLow { .. }
        | Error::HostilePrompt(_)
        | Error::PathTraversal(_)
        | Error::PayloadTooLarge(_)
        | Error::ModalityUnsupported(_)
        | Error::ResearchUnavailable(_)
        | Error::Cancelled(_)
        | Error::Cancel(_) => RetryReason::Transport,
    };
    tracing::trace!(reason = ?reason, "reason_from_error classified");
    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Fast` now allows retries: parse / schema failures get the
    /// full repair budget (5 attempts with `use_json_repair =
    /// true`), transient failures (transport / rate-limit /
    /// timeout) get two retries, and `Truncated` stays
    /// single-shot because the model already stopped producing
    /// output.
    #[test]
    fn budget_for_fast_at_least_five_attempts_for_parse_schema() {
        for reason in [
            RetryReason::Transport,
            RetryReason::RateLimit,
            RetryReason::Parse,
            RetryReason::Schema,
            RetryReason::Timeout,
            RetryReason::Truncated,
        ] {
            let b = budget_for(Mode::Fast, reason);
            match reason {
                RetryReason::Parse | RetryReason::Schema => {
                    assert_eq!(b.max_attempts, 5, "reason={reason:?}");
                    assert!(b.use_json_repair, "reason={reason:?}");
                }
                RetryReason::Truncated => {
                    assert_eq!(b.max_attempts, 1, "reason={reason:?}");
                    assert!(!b.use_json_repair, "reason={reason:?}");
                }
                RetryReason::Transport | RetryReason::RateLimit | RetryReason::Timeout => {
                    assert_eq!(b.max_attempts, 3, "reason={reason:?}");
                    assert!(!b.use_json_repair, "reason={reason:?}");
                }
            }
        }
    }

    /// `Fast` parse failures get the full repair budget:
    /// four retries (= five attempts) with the local repair
    /// pass enabled.
    #[test]
    fn budget_for_fast_parse_uses_json_repair_with_four_retries() {
        let b = budget_for(Mode::Fast, RetryReason::Parse);
        assert_eq!(b.max_attempts, 5);
        assert!(b.use_json_repair);
    }

    /// `Fast` truncated failures stay single-shot: the model
    /// already stopped producing output, so a re-issue would
    /// just truncate again.
    #[test]
    fn budget_for_fast_truncated_is_single_attempt() {
        let b = budget_for(Mode::Fast, RetryReason::Truncated);
        assert_eq!(b.max_attempts, 1);
        assert!(!b.use_json_repair);
    }

    /// `Standard` allows two retries for transport (the common
    /// transient 5xx).
    #[test]
    fn budget_for_standard_transport_is_three_attempts() {
        let b = budget_for(Mode::Standard, RetryReason::Transport);
        assert_eq!(b.max_attempts, 3);
        assert!(!b.use_json_repair);
    }

    /// `Standard` parse failures get the full repair budget:
    /// four retries with the local repair pass enabled.
    #[test]
    fn budget_for_standard_parse_uses_json_repair() {
        let b = budget_for(Mode::Standard, RetryReason::Parse);
        assert_eq!(b.max_attempts, 5);
        assert!(b.use_json_repair);
    }

    /// `Standard` rate-limit failures get one extra retry over
    /// the transient baseline (four attempts instead of three)
    /// because quota windows typically allow more headroom
    /// than a one-shot 5xx.
    #[test]
    fn budget_for_standard_rate_limit_four_attempts() {
        let b = budget_for(Mode::Standard, RetryReason::RateLimit);
        assert_eq!(b.max_attempts, 4);
        assert!(!b.use_json_repair);
    }

    /// `Deep` rate-limit failures get the most generous slot in
    /// the matrix: six attempts because the heavy path is
    /// expensive to restart and a transient throttle should
    /// not invalidate the run.
    #[test]
    fn budget_for_deep_rate_limit_is_six_attempts() {
        let b = budget_for(Mode::Deep, RetryReason::RateLimit);
        assert_eq!(b.max_attempts, 6);
        assert!(!b.use_json_repair);
    }

    /// Alias pin for the Deep rate-limit row, expressed in
    /// retries instead of attempts (six attempts = five
    /// retries). Keeps the public matrix anchor from drifting
    /// if a future refactor renames the other pin.
    #[test]
    fn budget_for_deep_rate_limit_remains_five_retries() {
        let b = budget_for(Mode::Deep, RetryReason::RateLimit);
        assert_eq!(b.max_attempts, 6);
        assert!(!b.use_json_repair);
    }

    /// `Deep` parse failures get the full repair budget:
    /// five attempts with `use_json_repair = true`.
    #[test]
    fn budget_for_deep_parse_uses_json_repair() {
        let b = budget_for(Mode::Deep, RetryReason::Parse);
        assert_eq!(b.max_attempts, 5);
        assert!(b.use_json_repair);
    }

    /// `Explore` mirrors the `Fast` envelope: five attempts with
    /// repair for parse / schema, three attempts for
    /// transients, one for `Truncated`.
    #[test]
    fn budget_for_explore_at_least_five_attempts_for_parse_schema() {
        for reason in [
            RetryReason::Transport,
            RetryReason::RateLimit,
            RetryReason::Parse,
            RetryReason::Schema,
            RetryReason::Timeout,
            RetryReason::Truncated,
        ] {
            let b = budget_for(Mode::Explore, reason);
            match reason {
                RetryReason::Parse | RetryReason::Schema => {
                    assert_eq!(b.max_attempts, 5, "reason={reason:?}");
                    assert!(b.use_json_repair, "reason={reason:?}");
                }
                RetryReason::Truncated => {
                    assert_eq!(b.max_attempts, 1, "reason={reason:?}");
                    assert!(!b.use_json_repair, "reason={reason:?}");
                }
                RetryReason::Transport | RetryReason::RateLimit | RetryReason::Timeout => {
                    assert_eq!(b.max_attempts, 3, "reason={reason:?}");
                    assert!(!b.use_json_repair, "reason={reason:?}");
                }
            }
        }
    }

    /// `Batch` mirrors the `Fast` envelope: five attempts with
    /// repair for parse / schema, three attempts for
    /// transients, one for `Truncated`.
    #[test]
    fn budget_for_batch_at_least_five_attempts_for_parse_schema() {
        for reason in [
            RetryReason::Transport,
            RetryReason::RateLimit,
            RetryReason::Parse,
            RetryReason::Schema,
            RetryReason::Timeout,
            RetryReason::Truncated,
        ] {
            let b = budget_for(Mode::Batch, reason);
            match reason {
                RetryReason::Parse | RetryReason::Schema => {
                    assert_eq!(b.max_attempts, 5, "reason={reason:?}");
                    assert!(b.use_json_repair, "reason={reason:?}");
                }
                RetryReason::Truncated => {
                    assert_eq!(b.max_attempts, 1, "reason={reason:?}");
                    assert!(!b.use_json_repair, "reason={reason:?}");
                }
                RetryReason::Transport | RetryReason::RateLimit | RetryReason::Timeout => {
                    assert_eq!(b.max_attempts, 3, "reason={reason:?}");
                    assert!(!b.use_json_repair, "reason={reason:?}");
                }
            }
        }
    }

    /// `Deep` transport failures retry three times (no repair
    /// — the parse layer is irrelevant here).
    #[test]
    fn budget_for_deep_transport_is_four_attempts_no_repair() {
        let b = budget_for(Mode::Deep, RetryReason::Transport);
        assert_eq!(b.max_attempts, 4);
        assert!(!b.use_json_repair);
    }

    /// `Deep` schema failures are equivalent to parse failures
    /// from the retry-budget perspective: five attempts with
    /// repair.
    #[test]
    fn budget_for_deep_schema_uses_json_repair() {
        let b = budget_for(Mode::Deep, RetryReason::Schema);
        assert_eq!(b.max_attempts, 5);
        assert!(b.use_json_repair);
    }

    /// `Standard` timeouts allow two retries (the deadline may
    /// have been hit because of an upstream blip).
    #[test]
    fn budget_for_standard_timeout_is_three_attempts_no_repair() {
        let b = budget_for(Mode::Standard, RetryReason::Timeout);
        assert_eq!(b.max_attempts, 3);
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
            reason_from_error(&Error::Timeout {
                message: "x".into(),
                http_status: None,
            }),
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
            reason_from_error(&Error::Provider {
                message: "x".into(),
                http_status: None,
            }),
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
            reason_from_error(&Error::PlanExhausted {
                message: "x".into(),
                http_status: None,
            }),
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
            reason_from_error(&Error::InvalidApiKey {
                message: "x".into(),
                http_status: None,
            }),
            RetryReason::Transport,
        );
        assert_eq!(
            reason_from_error(&Error::InvalidState("x".into())),
            RetryReason::Transport,
        );
    }

    /// Sanity: composing `reason_from_error` with `budget_for`
    /// yields the per-mode cap the spec wants. The expected
    /// values mirror the new matrix (D.21.6 update): Standard
    /// transients get three attempts, Deep rate-limit gets six
    /// attempts, Deep schema failures get five with repair,
    /// and even Fast transients are no longer single-shot.
    #[test]
    fn reason_from_error_then_budget_for_round_trips_through_matrix() {
        let cases = [
            (
                Error::Timeout {
                    message: "x".into(),
                    http_status: None,
                },
                Mode::Standard,
                3,
            ),
            (
                Error::Provider {
                    message: "x".into(),
                    http_status: None,
                },
                Mode::Standard,
                3,
            ),
            (
                Error::PlanExhausted {
                    message: "x".into(),
                    http_status: None,
                },
                Mode::Deep,
                6,
            ),
            (Error::SchemaViolation("x".into()), Mode::Deep, 5),
            (
                Error::Provider {
                    message: "x".into(),
                    http_status: None,
                },
                Mode::Fast,
                3,
            ),
        ];
        for (err, mode, expected) in cases {
            let b = budget_for(mode, reason_from_error(&err));
            assert_eq!(b.max_attempts, expected, "err={err:?} mode={mode:?}");
        }
    }
}
