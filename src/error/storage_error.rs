//! D.12.10: `StorageError` umbrella. Aggregates the four
//! storage failure classes the codebase produces today
//! (`IoError`, `rusqlite::Error`, compression failures, schema
//! failures) under a single enum so callers can match
//! exhaustively without losing precision.
//!
//! The umbrella is intentionally non-exhaustive of every
//! SQLite / compression error possible — it covers the
//! buckets the rest of the codebase actually throws. New
//! failure classes should be added as new variants here
//! when the catalog demands them; existing variants must
//! stay stable so downstream `match` expressions keep
//! compiling.
//!
//! Note on derives: `IoError` wraps `std::io::Error` and
//! `Box<dyn Error>`, neither of which implements `Clone` /
//! `PartialEq` / `Eq`. The umbrella therefore derives
//! only `Debug`; comparisons go through the variant +
//! message pair manually.

use crate::error::IoError;

/// Aggregated storage failure: I/O, SQLite, compression, or
/// schema. Lives alongside `Error::Io(IoError)` and
/// `Error::Provider(_)` so callers that want a richer view
/// of the storage stack can use this enum without losing
/// the original signal.
#[derive(Debug)]
pub enum StorageError {
    /// Filesystem / atomic-write failure.
    Io(IoError),
    /// SQLite failure. Wrapped as a plain message so the
    /// umbrella does not depend on `rusqlite`'s public
    /// types.
    Sqlite {
        /// Human-readable error message.
        message: String,
    },
    /// Compression / decompression failure.
    Compression {
        /// Human-readable error message.
        message: String,
    },
    /// Persistent schema mismatch (the on-disk artifact
    /// no longer matches the contract the runner expects).
    Schema {
        /// Human-readable error message.
        message: String,
    },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Sqlite { message } => write!(f, "sqlite: {message}"),
            Self::Compression { message } => write!(f, "compression: {message}"),
            Self::Schema { message } => write!(f, "schema: {message}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<IoError> for StorageError {
    fn from(e: IoError) -> Self {
        Self::Io(e)
    }
}
