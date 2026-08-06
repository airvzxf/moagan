//! D.26.5: structured JSON error output for the CLI.
//!
//! The CLI emits a single `JsonError` line on failure so
//! scripts can `moagan ... --format json 2> err.json` and
//! parse the failure without scraping the human-readable
//! log output. The struct mirrors the T01-06 §12.3 exit
//! codes via the `exit_code` field; `code` carries the
//! stable `ErrorCode` wire form introduced in D.12.12.
//!
//! Callers build the struct from an `Error` (or any
//! other source) and then call `to_json` to produce the
//! single-line wire format.

use serde::Serialize;

/// One-line JSON representation of a CLI failure.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct JsonError {
    /// Stable wire-form error code (e.g. `INVALID_ARGS`).
    pub code: String,
    /// Human-readable error message.
    pub message: String,
    /// Process exit code (T01-06 §12.3).
    pub exit_code: i32,
    /// Optional source error chain (joined with `" -> "`).
    pub source: Option<String>,
}

impl JsonError {
    /// Build a `JsonError` from raw components. Used by
    /// callers that already have an `Error` (and therefore
    /// `error.code()` + `error.exit_code()`) in hand.
    pub fn from_error_code(
        code: &str,
        message: &str,
        exit_code: i32,
        source: Option<&str>,
    ) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            exit_code,
            source: source.map(|s| s.to_string()),
        }
    }

    /// Serialize to a single-line JSON string. Falls back
    /// to an empty string if serialization fails (which
    /// should not happen for a struct of owned strings).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}
