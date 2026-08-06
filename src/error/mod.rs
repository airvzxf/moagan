//! Crate-wide error type. `thiserror` for the library; `main.rs` wraps
//! everything in `anyhow::Result`.
//!
//! Exit codes follow T01-06 §12.3:
//! 0 ok, 1 generic, 2 invalid args, 3 invalid api key, 4 plan exhausted,
//! 5 timeout, 6 cancelled, 7 schema violation, 8 io error.
//!
//! Companion modules (catalog gap closures). These are additive
//! types that callers can attach to existing error paths without
//! touching the public `Error` enum.

pub mod chain;
pub mod json_output;
pub mod llm_error;
pub mod redact_display;
pub mod storage_error;

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::atomic::writer::ArtifactMeta;
use crate::error_code::ErrorCode;

/// Stable process exit codes from T01-06 §12.3 and D.12.14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// Successful execution.
    Ok = 0,
    /// Unclassified failure.
    GenericError = 1,
    /// Invalid CLI arguments from the baseline contract.
    InvalidArgs = 2,
    /// Missing or invalid provider API key.
    ApiKeyInvalid = 3,
    /// Provider plan exhausted.
    PlanExhausted = 4,
    /// Operation timed out under the baseline contract.
    Timeout = 5,
    /// Operation was cancelled.
    Cancelled = 6,
    /// Persistent schema violation.
    SchemaViolation = 7,
    /// I/O failure under the baseline contract.
    IoError = 8,
    /// Token budget exhausted.
    BudgetExhausted = 9,
    /// More user input is required.
    NeedsInput = 10,
    /// Configured budget was exceeded.
    BudgetExceeded = 20,
    /// Provider plan paused the run.
    PlanPaused = 30,
    /// Provider request failed.
    ProviderError = 40,
    /// Timeout from the extended catalog.
    TimeoutExit = 50,
    /// Invalid arguments from the extended catalog.
    InvalidArgsExit = 60,
    /// I/O failure from the extended catalog.
    IoErrorExit = 70,
    /// Run context or state failure.
    ContextError = 80,
    /// Export verification failed.
    ExportVerificationFailed = 90,
    #[allow(missing_docs)]
    Storage = 91,
    #[allow(missing_docs)]
    Llm = 92,
    #[allow(missing_docs)]
    Sandbox = 93,
    #[allow(missing_docs)]
    Research = 94,
    #[allow(missing_docs)]
    Resume = 95,
    #[allow(missing_docs)]
    Discovery = 96,
    /// Process interrupted by SIGINT.
    SigInt = 130,
}

impl From<ErrorCode> for ExitCode {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::Storage => Self::Storage,
            ErrorCode::Llm => Self::Llm,
            ErrorCode::Sandbox => Self::Sandbox,
            ErrorCode::Research => Self::Research,
            ErrorCode::Resume => Self::Resume,
            ErrorCode::Discovery => Self::Discovery,
            _ => Self::GenericError,
        }
    }
}

/// Library error type. All public APIs return `Result<T, Error>`.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure with structured context.
    #[error(transparent)]
    Io(IoError),

    /// User-supplied argument violated a constraint.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    /// API key is missing or malformed.
    #[error("invalid api key: {0}")]
    InvalidApiKey(String),

    /// Provider plan is exhausted (token budget consumed).
    #[error("plan exhausted: {0}")]
    PlanExhausted(String),

    /// Operation timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Operation cancelled by user or supervisor.
    #[error("cancelled: {0}")]
    Cancelled(String),

    /// Schema-validated output failed its contract.
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    /// State machine transition is illegal at the current state.
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// Another holder owns the requested run lease.
    #[error("lock held: {0}")]
    LockHeld(String),

    /// Provider returned a non-recoverable error.
    #[error("provider error: {0}")]
    Provider(String),

    /// Mock provider ran out of canned responses.
    #[error("mock provider exhausted")]
    MockExhausted,

    /// Cache lookup failed.
    #[error("cache: {0}")]
    Cache(String),

    /// Cancellation propagated.
    #[error(transparent)]
    Cancel(#[from] CancelSignal),

    /// The dispatcher needs an explicit confirmation to proceed
    /// (e.g. a destructive plan without `--yes`). Mirrors the
    /// `ExitCode::NeedsInput` value so CI scripts can branch on
    /// exit code 10.
    #[error("needs input: {0}")]
    NeedsInput(String),

    /// The discovery phase produced too many failed sketches to be
    /// trustworthy. Triggered when more than half of the planned
    /// attempts fail (with a minimum-attempts guard so small runs
    /// do not abort prematurely). Maps to
    /// [`ExitCode::ContextError`] because the run context
    /// (`run_dir`) is in a degraded state and continuing would
    /// persist low-quality proposals downstream.
    #[error(
        "discovery quality too low: {failed}/{total} attempts failed (threshold {threshold_pct}%)"
    )]
    DiscoveryQualityTooLow {
        /// Number of attempts that produced no usable sketch.
        failed: usize,
        /// Total number of attempts the phase issued.
        total: usize,
        /// Threshold percentage that triggered the abort.
        threshold_pct: usize,
    },

    /// E10 (catalog 10-integrada-v0 §D.20.7): the intake phase's
    /// `Role::HostilePromptDetector` classified the user's raw
    /// prompt as `hostile`, and the configured `HostilePolicy`
    /// rejected the run outright. The inner string carries the
    /// detector's `reasons[0]` (the strongest injection signal) so
    /// the operator sees why the run was rejected without having
    /// to cross-reference the telemetry stream. Maps to
    /// [`ExitCode::ContextError`] because the run is in a
    /// degraded state (the caller asked us to act on a hostile
    /// input) and refusing to continue is the safe default.
    #[error("hostile prompt: {0}")]
    HostilePrompt(String),
}

impl Error {
    /// Public, stable error code. Maps every `Error` variant to
    /// the closest `ErrorCode` (D.12.8). The mapping is
    /// best-effort: variants that do not have a clean bucket fall
    /// back to `ErrorCode::UnhandledError`. Wire form is
    /// `SCREAMING_SNAKE_CASE` (D.12.12).
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Io(_) => ErrorCode::Io,
            Self::InvalidArgs(_) => ErrorCode::InvalidArgs,
            Self::InvalidApiKey(_) => ErrorCode::Auth,
            Self::PlanExhausted(_) => ErrorCode::QuotaExceeded,
            Self::Timeout(_) => ErrorCode::TimeoutPhase,
            Self::Cancelled(_) => ErrorCode::Cancelled,
            Self::SchemaViolation(_) => ErrorCode::SchemaViolation,
            Self::InvalidState(_) => ErrorCode::InvalidState,
            Self::LockHeld(_) => ErrorCode::InvalidState,
            Self::Provider(_) => ErrorCode::InvalidResponse,
            Self::MockExhausted => ErrorCode::NeedsInput,
            Self::Cache(_) => ErrorCode::Io,
            Self::Cancel(_) => ErrorCode::Cancelled,
            Self::NeedsInput(_) => ErrorCode::NeedsInput,
            Self::DiscoveryQualityTooLow { .. } => ErrorCode::InvalidState,
            Self::HostilePrompt(_) => ErrorCode::HostilePrompt,
        }
    }

    /// Return the stable process exit code for this error.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidArgs(_) => ExitCode::InvalidArgs,
            Self::InvalidApiKey(_) => ExitCode::ApiKeyInvalid,
            Self::PlanExhausted(_) => ExitCode::PlanExhausted,
            Self::Timeout(_) => ExitCode::Timeout,
            Self::Cancelled(_) | Self::Cancel(_) => ExitCode::Cancelled,
            Self::SchemaViolation(_) => ExitCode::SchemaViolation,
            Self::Io(_) => ExitCode::IoError,
            Self::MockExhausted | Self::Provider(_) => ExitCode::ProviderError,
            Self::Cache(_) | Self::InvalidState(_) | Self::LockHeld(_) => ExitCode::ContextError,
            Self::NeedsInput(_) => ExitCode::NeedsInput,
            Self::DiscoveryQualityTooLow { .. } => ExitCode::ContextError,
            Self::HostilePrompt(_) => ExitCode::ContextError,
        }
    }

    /// Should this error count toward the per-provider circuit
    /// breaker? Mirrors [`crate::error_code::ErrorCode::is_circuit_opening`]
    /// but operates directly on the [`Error`] variant so the policy
    /// does not depend on the [`crate::Error::code`] mapping — the
    /// latter collapses several variants onto the same code
    /// (`Provider` → `InvalidResponse`, `PlanExhausted` →
    /// `QuotaExceeded`, `Timeout` → `TimeoutPhase`) and loses the
    /// HTTP-status distinction the breaker is supposed to act on.
    ///
    /// Openers:
    ///
    /// - [`Error::InvalidApiKey`] — `ErrorCode::Auth`, HTTP 401/403
    ///   from the provider. The catalog adds `Auth` to the
    ///   circuit-opening set so a provider that rejects the
    ///   credentials is sidelined instead of hammered.
    /// - [`Error::Provider`] — generic 5xx / upstream-error bucket.
    ///   `classify_status` (in `llm/http.rs`) routes HTTP 500..=599
    ///   here, so this is the path the breaker needs to catch the
    ///   common "provider is down" signal.
    /// - [`Error::PlanExhausted`] — HTTP 429 from the provider. The
    ///   mapping in `code()` collapses it onto `QuotaExceeded`,
    ///   which is not in `is_circuit_opening()`; this helper
    ///   re-asserts the spec policy directly.
    /// - [`Error::Timeout`] — HTTP 408/504/524 or any phase-level
    ///   timeout. The breaker treats sustained timeouts as a
    ///   provider-health signal, distinct from the retry budget's
    ///   per-attempt backoff.
    ///
    /// Non-openers:
    ///
    /// - [`Error::SchemaViolation`], [`Error::InvalidArgs`],
    ///   [`Error::InvalidState`] — operator / contract errors that
    ///   have nothing to do with the provider's health.
    /// - [`Error::Cancelled`] / [`Error::Cancel`] — cooperative
    ///   shutdown; the breaker must not count an operator cancel as
    ///   a provider outage.
    /// - [`Error::MockExhausted`] — the canned-response queue ran
    ///   out; the provider itself is fine.
    /// - [`Error::Io`] / [`Error::Cache`] — local disk / cache
    ///   issues; tripping a remote breaker on a local I/O blip is
    ///   the wrong granularity.
    pub fn is_circuit_opening(&self) -> bool {
        matches!(
            self,
            Self::InvalidApiKey(_) | Self::Provider(_) | Self::PlanExhausted(_) | Self::Timeout(_)
        )
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(IoError::Raw(e))
    }
}

impl From<IoError> for Error {
    fn from(e: IoError) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Provider(format!("sqlite: {e}"))
    }
}

impl From<r2d2::Error> for Error {
    fn from(e: r2d2::Error) -> Self {
        Error::Provider(format!("sqlite pool: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::SchemaViolation(format!("json: {e}"))
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Error::InvalidArgs(format!("toml: {e}"))
    }
}

/// Structured I/O errors. Splits the raw `io::Error` from the
/// context-rich variants so callers can match precisely.
#[derive(Debug, Error)]
pub enum IoError {
    /// Unstructured `io::Error` from a stdlib call.
    #[error("io: {0}")]
    Raw(io::Error),

    /// Destination path had no parent directory.
    #[error("path has no parent: {path}")]
    NoParent {
        /// Path that lacked a parent.
        path: PathBuf,
    },

    /// `create_dir_all` failed.
    #[error("create dir {path}: {source}")]
    CreateDir {
        /// Directory that failed to be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `File::create` failed.
    #[error("create file {path}: {source}")]
    CreateFile {
        /// File that failed to be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// Write to file failed.
    #[error("write {path}: {source}")]
    Write {
        /// File that failed to be written.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `sync_all` failed.
    #[error("sync {path}: {source}")]
    Sync {
        /// File that failed to be synced.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `File::open` for parent directory failed.
    #[error("open dir {path}: {source}")]
    OpenDir {
        /// Directory that failed to open.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `rename` failed.
    #[error("rename {from} -> {to}: {source}")]
    Rename {
        /// Source path of the rename.
        from: PathBuf,
        /// Destination path of the rename.
        to: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `fs::read` failed.
    #[error("read {path}: {source}")]
    Read {
        /// Path that was being read.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// Sidecar / config file failed to parse. Used by the profile
    /// loader (TOML) so the error travels with the offending path
    /// instead of bubbling through `Error::InvalidArgs`.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying parse error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Sidecar metadata failed to serialize.
    #[error("serialize meta: {0}")]
    SerializeMeta(#[source] serde_json::Error),

    /// Sidecar metadata failed to deserialize.
    #[error("deserialize meta: {0}")]
    DeserializeMeta(#[source] serde_json::Error),

    /// Data file does not match its sidecar metadata.
    #[error("meta mismatch at {path}: expected {expected:?}, got {got:?}")]
    MetaMismatch {
        /// Path whose integrity is in question.
        path: PathBuf,
        /// What the sidecar advertises.
        expected: Box<ArtifactMeta>,
        /// What the data file actually contains.
        got: Box<ArtifactMeta>,
    },
}

/// Signal that work was cancelled cooperatively. Distinct from
/// `Error::Cancelled` so callers can match the structural signal.
#[derive(Debug, Error)]
#[error("cancel signal")]
pub struct CancelSignal;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Map the error to the documented exit code (T01-06 §12.3).
pub fn exit_code(err: &Error) -> u8 {
    err.exit_code() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_discriminants_are_stable() {
        assert_eq!(ExitCode::Ok as i32, 0);
        assert_eq!(ExitCode::IoError as i32, 8);
        assert_eq!(ExitCode::ProviderError as i32, 40);
        assert_eq!(ExitCode::SigInt as i32, 130);
    }

    #[test]
    fn error_code_extended_variants_count() {
        let variants = [
            ErrorCode::Storage,
            ErrorCode::Llm,
            ErrorCode::Sandbox,
            ErrorCode::Research,
            ErrorCode::Resume,
            ErrorCode::Discovery,
        ];
        assert_eq!(variants.len(), 6);
        assert_eq!(
            variants.map(|code| ExitCode::from(code) as i32),
            [91, 92, 93, 94, 95, 96]
        );
    }

    #[test]
    fn exit_code_storage_maps_to_91() {
        assert_eq!(ExitCode::from(ErrorCode::Storage), ExitCode::Storage);
        assert_eq!(ExitCode::Storage as i32, 91);
    }

    #[test]
    fn exit_code_llm_maps_to_92() {
        assert_eq!(ExitCode::from(ErrorCode::Llm), ExitCode::Llm);
        assert_eq!(ExitCode::Llm as i32, 92);
    }

    #[test]
    fn error_exit_code_method_maps_baseline_variants() {
        assert_eq!(
            Error::Io(IoError::Raw(io::Error::other("x"))).exit_code(),
            ExitCode::IoError
        );
        assert_eq!(
            Error::InvalidArgs("x".into()).exit_code(),
            ExitCode::InvalidArgs
        );
        assert_eq!(
            Error::InvalidApiKey("x".into()).exit_code(),
            ExitCode::ApiKeyInvalid
        );
        assert_eq!(
            Error::PlanExhausted("x".into()).exit_code(),
            ExitCode::PlanExhausted
        );
        assert_eq!(Error::Timeout("x".into()).exit_code(), ExitCode::Timeout);
        assert_eq!(
            Error::Cancelled("x".into()).exit_code(),
            ExitCode::Cancelled
        );
        assert_eq!(
            Error::SchemaViolation("x".into()).exit_code(),
            ExitCode::SchemaViolation
        );
    }

    #[test]
    fn error_exit_code_method_maps_extended_variants() {
        assert_eq!(
            Error::Provider("x".into()).exit_code(),
            ExitCode::ProviderError
        );
        assert_eq!(Error::MockExhausted.exit_code(), ExitCode::ProviderError);
        assert_eq!(Error::Cache("x".into()).exit_code(), ExitCode::ContextError);
        assert_eq!(
            Error::InvalidState("x".into()).exit_code(),
            ExitCode::ContextError
        );
        assert_eq!(
            Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .exit_code(),
            ExitCode::ContextError
        );
    }
    #[test]
    fn compatibility_exit_code_function_returns_numeric_code() {
        assert_eq!(exit_code(&Error::InvalidArgs("x".into())), 2);
        assert_eq!(exit_code(&Error::InvalidApiKey("x".into())), 3);
        assert_eq!(exit_code(&Error::PlanExhausted("x".into())), 4);
        assert_eq!(exit_code(&Error::Timeout("x".into())), 5);
        assert_eq!(exit_code(&Error::Cancelled("x".into())), 6);
        assert_eq!(exit_code(&Error::Cancel(CancelSignal)), 6);
        assert_eq!(exit_code(&Error::SchemaViolation("x".into())), 7);
        assert_eq!(exit_code(&Error::MockExhausted), 40);
        assert_eq!(exit_code(&Error::Provider("x".into())), 40);
    }

    #[test]
    fn cancel_signal_display() {
        let s = format!("{}", CancelSignal);
        assert!(s.contains("cancel"));
    }

    /// `Error::code()` must return the canonical bucket for each
    /// variant. Pin every mapping so a refactor that re-routes a
    /// variant trips the test.
    #[test]
    fn code_maps_every_variant() {
        use std::io;

        assert_eq!(
            Error::Io(IoError::Raw(io::Error::other("x"))).code(),
            ErrorCode::Io
        );
        assert_eq!(
            Error::InvalidArgs("x".into()).code(),
            ErrorCode::InvalidArgs
        );
        assert_eq!(Error::InvalidApiKey("x".into()).code(), ErrorCode::Auth);
        assert_eq!(
            Error::PlanExhausted("x".into()).code(),
            ErrorCode::QuotaExceeded
        );
        assert_eq!(Error::Timeout("x".into()).code(), ErrorCode::TimeoutPhase);
        assert_eq!(Error::Cancelled("x".into()).code(), ErrorCode::Cancelled);
        assert_eq!(
            Error::SchemaViolation("x".into()).code(),
            ErrorCode::SchemaViolation
        );
        assert_eq!(
            Error::InvalidState("x".into()).code(),
            ErrorCode::InvalidState
        );
        assert_eq!(
            Error::Provider("x".into()).code(),
            ErrorCode::InvalidResponse
        );
        assert_eq!(Error::MockExhausted.code(), ErrorCode::NeedsInput);
        assert_eq!(Error::Cache("x".into()).code(), ErrorCode::Io);
        assert_eq!(Error::Cancel(CancelSignal).code(), ErrorCode::Cancelled);
        assert_eq!(
            Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .code(),
            ErrorCode::InvalidState
        );
        // E10: hostile-prompt error maps to the dedicated bucket
        // so the post-execution review can branch on the wire form.
        assert_eq!(
            Error::HostilePrompt("ignore previous instructions".into()).code(),
            ErrorCode::HostilePrompt
        );
    }

    /// The code form must round-trip through serde unchanged so
    /// external tooling can decode the on-disk error log.
    #[test]
    fn code_serializes_to_screaming_snake_case() {
        let cases = [
            (Error::InvalidArgs("x".into()), "INVALID_ARGS"),
            (Error::InvalidApiKey("x".into()), "AUTH"),
            (Error::Cancelled("x".into()), "CANCELLED"),
            (Error::SchemaViolation("x".into()), "SCHEMA_VIOLATION"),
            (Error::InvalidState("x".into()), "INVALID_STATE"),
            (Error::MockExhausted, "NEEDS_INPUT"),
        ];
        for (err, expected) in cases {
            let code = err.code();
            assert_eq!(code.stable(), expected, "code mismatch for {err:?}");
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json.trim_matches('"'), expected);
        }
    }

    /// Every code returned by `code()` must classify consistently:
    /// retriable codes are a subset of retriable-or-cancel; user
    /// errors (`InvalidArgs`, `InvalidState`) must never retriable.
    #[test]
    fn code_is_consistent_with_policy_helpers() {
        let variants = [
            Error::InvalidArgs("x".into()),
            Error::InvalidApiKey("x".into()),
            Error::PlanExhausted("x".into()),
            Error::Timeout("x".into()),
            Error::Cancelled("x".into()),
            Error::SchemaViolation("x".into()),
            Error::InvalidState("x".into()),
            Error::MockExhausted,
        ];
        for err in variants {
            let code = err.code();
            // Sanity: code must be one of the catalog variants.
            assert!(!code.stable().is_empty());
            // Cancelled codes must never be retriable.
            if matches!(err, Error::Cancelled(_) | Error::Cancel(_)) {
                assert!(!code.is_retriable(), "{code:?} must not be retriable");
            }
            // InvalidArgs and InvalidState must never be retriable
            // — they are operator / developer errors.
            if matches!(err, Error::InvalidArgs(_) | Error::InvalidState(_)) {
                assert!(!code.is_retriable(), "{code:?} must not be retriable");
            }
        }
    }

    /// `code()` must be `&self -> ErrorCode` (Copy), so the call
    /// site can pass the code by value without a clone.
    #[test]
    fn code_is_copy() {
        let err = Error::InvalidArgs("x".into());
        let a = err.code();
        let b = err.code();
        assert_eq!(a, b);
    }

    // --- is_circuit_opening -----------------------------------------
    // Pin every variant's classification so the breaker wrapper in
    // provider.rs (and any future caller) can rely on the policy
    // without re-deriving it.

    /// Openers cover the 5xx / 429 / auth / timeout set the breaker
    /// exists to catch.
    #[test]
    fn is_circuit_opening_classifies_openers() {
        assert!(Error::InvalidApiKey("x".into()).is_circuit_opening());
        assert!(Error::Provider("x".into()).is_circuit_opening());
        assert!(Error::PlanExhausted("x".into()).is_circuit_opening());
        assert!(Error::Timeout("x".into()).is_circuit_opening());
    }

    /// Non-openers are operator errors, contract mismatches, or
    /// local I/O — they do not indicate a provider outage.
    #[test]
    fn is_circuit_opening_classifies_non_openers() {
        assert!(!Error::SchemaViolation("x".into()).is_circuit_opening());
        assert!(!Error::InvalidArgs("x".into()).is_circuit_opening());
        assert!(!Error::InvalidState("x".into()).is_circuit_opening());
        assert!(!Error::Cancelled("x".into()).is_circuit_opening());
        assert!(!Error::Cancel(CancelSignal).is_circuit_opening());
        assert!(!Error::MockExhausted.is_circuit_opening());
        assert!(!Error::Io(IoError::Raw(io::Error::other("x"))).is_circuit_opening());
        assert!(!Error::Cache("x".into()).is_circuit_opening());
        assert!(!Error::NeedsInput("x".into()).is_circuit_opening());
        assert!(
            !Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .is_circuit_opening()
        );
    }

    /// The opener set matches the [`ErrorCode::is_circuit_opening`]
    /// set semantically (HTTP 5xx/429 + Auth + transport), even
    /// though the routing goes through `Error` variants instead of
    /// `ErrorCode`. The test guards against drift: if a new opener
    /// appears in `ErrorCode`, the matching `Error` variant must
    /// also flip, or the breaker integration in `provider.rs` will
    /// silently miss the new failure class.
    #[test]
    fn is_circuit_opening_aligns_with_error_code_policy() {
        // Openers in ErrorCode that map to non-opening Error variants
        // should still trip through Error::is_circuit_opening because
        // the helper compensates for the lossy code() mapping.
        assert!(ErrorCode::Http500.is_circuit_opening());
        assert!(
            Error::Provider("http 500: boom".into()).is_circuit_opening(),
            "Error::Provider must trip the breaker (covers Http500/502/503/504 upstream errors)"
        );
        assert!(ErrorCode::Http429.is_circuit_opening());
        assert!(
            Error::PlanExhausted("http 429: throttled".into()).is_circuit_opening(),
            "Error::PlanExhausted must trip the breaker (covers Http429)"
        );
        assert!(ErrorCode::Auth.is_circuit_opening());
        assert!(
            Error::InvalidApiKey("http 401: bad".into()).is_circuit_opening(),
            "Error::InvalidApiKey must trip the breaker (covers Auth)"
        );
    }
    // --- catalog gap closures: companion modules ---------------
    // D.12.10, D.12.11, D.16.2, D.26.5, D.29.9. Each new module
    // gets at least one test that pins its public contract so
    // future refactors cannot silently regress the wire form.

    /// D.12.11: `RetryAdvice` default must be the optimistic
    /// `Retry` so a freshly-constructed `LlmError` does not
    /// pessimistically suppress retries.
    #[test]
    fn retry_advice_default_is_retry() {
        use crate::error::llm_error::RetryAdvice;
        assert_eq!(RetryAdvice::default(), RetryAdvice::Retry);
        let explicit = RetryAdvice::SwitchProvider;
        assert_ne!(explicit, RetryAdvice::default());
    }

    /// D.12.10: `StorageError::from(IoError)` must aggregate
    /// the original I/O error so callers can recover the
    /// path / source without losing information.
    #[test]
    fn storage_error_from_io_error_converts() {
        use crate::error::storage_error::StorageError;
        let io = IoError::Read {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::other("boom"),
        };
        let storage: StorageError = io.into();
        match storage {
            StorageError::Io(IoError::Read { path, .. }) => {
                assert_eq!(path, PathBuf::from("/tmp/x"));
            }
            other => panic!("expected Io(Read), got {other:?}"),
        }
    }

    /// D.16.2: `RedactedDisplay` must strip known secret
    /// patterns (here a MiniMax sk-cp key) from the inner
    /// value's `Display` output before the formatter sees it.
    #[test]
    fn redacted_display_removes_known_secrets() {
        use crate::error::redact_display::redacted_display;
        let leaky = String::from("auth header: sk-cp-abcdef0123456789ABCDEF");
        let rendered = format!("{}", redacted_display(&leaky));
        assert!(
            !rendered.contains("sk-cp-abc"),
            "secret must be scrubbed, got: {rendered}"
        );
        assert!(
            rendered.contains("[REDACTED:minimax_sk_cp]"),
            "expected the marker, got: {rendered}"
        );
    }

    /// D.26.5: `JsonError` must serialize the canonical four
    /// fields with the right names and types so downstream
    /// scripts can parse the line without guessing.
    #[test]
    fn json_error_serializes_to_correct_format() {
        use crate::error::json_output::JsonError;
        let err = JsonError::from_error_code(
            "INVALID_ARGS",
            "missing --prompt",
            2,
            Some("stdin: line 1"),
        );
        let json = err.to_json();
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["code"], "INVALID_ARGS");
        assert_eq!(value["message"], "missing --prompt");
        assert_eq!(value["exit_code"], 2);
        assert_eq!(value["source"], "stdin: line 1");
    }

    /// D.29.9: `ErrorChain` must keep the input and the
    /// source chain as separate fields and walk
    /// `error.source()` to the root.
    #[test]
    fn error_chain_captures_input_and_source() {
        use crate::error::chain::ErrorChain;
        use std::io;

        let outer = io::Error::other("open failed");
        // Build a two-level chain so the test does not
        // depend on a specific catalog error type.
        #[derive(Debug)]
        struct Nested {
            msg: String,
            inner: Option<Box<dyn std::error::Error + Send + Sync>>,
        }
        impl std::fmt::Display for Nested {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.msg)
            }
        }
        impl std::error::Error for Nested {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.inner
                    .as_deref()
                    .map(|e| e as &(dyn std::error::Error + 'static))
            }
        }
        let inner: Box<dyn std::error::Error + Send + Sync> = Box::new(outer);
        let nested = Nested {
            msg: "open failed".into(),
            inner: Some(inner),
        };
        let chain = ErrorChain::from_error("user prompt: build X", &nested);
        assert_eq!(chain.input, "user prompt: build X");
        assert!(!chain.source_chain.is_empty());
        assert_eq!(chain.source_chain[0], "open failed");
        assert!(chain.source_chain.len() >= 2);
    }

    /// D.12.10: `StorageError::Display` must render every
    /// variant with its discriminator prefix so log lines
    /// can be filtered by category.
    #[test]
    fn storage_error_display_format() {
        use crate::error::storage_error::StorageError;
        let cases = [
            (
                StorageError::Sqlite {
                    message: "no such table".into(),
                },
                "sqlite: no such table",
            ),
            (
                StorageError::Compression {
                    message: "gzip header".into(),
                },
                "compression: gzip header",
            ),
            (
                StorageError::Schema {
                    message: "version 3 vs 4".into(),
                },
                "schema: version 3 vs 4",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(format!("{err}"), expected);
        }
    }
}
