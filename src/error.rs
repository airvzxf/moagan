//! Crate-wide error type. `thiserror` for the library; `main.rs` wraps
//! everything in `anyhow::Result`.
//!
//! Exit codes follow T01-06 §12.3:
//! 0 ok, 1 generic, 2 invalid args, 3 invalid api key, 4 plan exhausted,
//! 5 timeout, 6 cancelled, 7 schema violation, 8 io error.

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
    /// Process interrupted by SIGINT.
    SigInt = 130,
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
            Self::Provider(_) => ErrorCode::InvalidResponse,
            Self::MockExhausted => ErrorCode::NeedsInput,
            Self::Cache(_) => ErrorCode::Io,
            Self::Cancel(_) => ErrorCode::Cancelled,
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
            Self::Cache(_) | Self::InvalidState(_) => ExitCode::ContextError,
        }
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
}
