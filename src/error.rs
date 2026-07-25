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
    match err {
        Error::InvalidArgs(_) => 2,
        Error::InvalidApiKey(_) => 3,
        Error::PlanExhausted(_) => 4,
        Error::Timeout(_) => 5,
        Error::Cancelled(_) | Error::Cancel(_) => 6,
        Error::SchemaViolation(_) => 7,
        Error::Io(_) => 8,
        Error::MockExhausted | Error::Provider(_) | Error::Cache(_) | Error::InvalidState(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_per_variant() {
        assert_eq!(exit_code(&Error::InvalidArgs("x".into())), 2);
        assert_eq!(exit_code(&Error::InvalidApiKey("x".into())), 3);
        assert_eq!(exit_code(&Error::PlanExhausted("x".into())), 4);
        assert_eq!(exit_code(&Error::Timeout("x".into())), 5);
        assert_eq!(exit_code(&Error::Cancelled("x".into())), 6);
        assert_eq!(exit_code(&Error::Cancel(CancelSignal)), 6);
        assert_eq!(exit_code(&Error::SchemaViolation("x".into())), 7);
        assert_eq!(exit_code(&Error::MockExhausted), 1);
        assert_eq!(exit_code(&Error::Provider("x".into())), 1);
    }

    #[test]
    fn cancel_signal_display() {
        let s = format!("{}", CancelSignal);
        assert!(s.contains("cancel"));
    }
}
