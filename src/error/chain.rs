//! D.29.9: error chain with input + source separation.
//!
//! The CLI diagnostics surface wants two distinct pieces
//! of information when a step fails:
//! 1. the *input* the caller passed in (the user prompt,
//!    the offending argument, the bad path), and
//! 2. the *source chain* of the underlying error (the
//!    transport error, the SQLite error, the I/O error).
//!
//! `ErrorChain` carries both side-by-side so the post-mortem
//! log line can render them on the same row without having
//! to re-derive the source chain from a `dyn Error`.

/// Capture the input the caller passed in alongside the
/// resolved source chain of the underlying error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorChain {
    /// The input the caller passed in (prompt, argument,
    /// path, etc.). Rendered verbatim — the caller is
    /// responsible for redacting secrets before creating
    /// the chain.
    pub input: String,
    /// The source chain of the underlying error, in
    /// causal order: index 0 is the error itself, each
    /// subsequent entry is the result of `error.source()`.
    pub source_chain: Vec<String>,
}

impl ErrorChain {
    /// Build a chain from the caller input and the
    /// underlying error. Walks `error.source()` until it
    /// returns `None` and stores the resulting list in
    /// `source_chain`.
    pub fn from_error(input: &str, err: &dyn std::error::Error) -> Self {
        let mut chain = vec![err.to_string()];
        let mut current = err.source();
        while let Some(s) = current {
            chain.push(s.to_string());
            current = s.source();
        }
        Self {
            input: input.to_string(),
            source_chain: chain,
        }
    }

    /// Build a chain from a caller input plus a pre-collected
    /// source chain. Useful when the caller already has the
    /// chain materialised (e.g. pulled from a log file)
    /// and just wants the typed wrapper.
    pub fn from_parts(input: impl Into<String>, source_chain: Vec<String>) -> Self {
        Self {
            input: input.into(),
            source_chain,
        }
    }
}
