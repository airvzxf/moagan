//! Redaction — secret-bearing strings are scrubbed before they reach
//! any artefact. See `apply`, `RedactWriter`, and `RedactPolicy`.

pub mod apply;
pub mod patterns;
pub mod writer;

pub use apply::{RedactPolicy, Surface, apply};
pub use writer::{RedactWriter, redact_text};
