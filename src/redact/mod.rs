//! Redaction — secret-bearing strings are scrubbed before they reach
//! any artefact. See `apply`, `RedactWriter`, and `RedactPolicy`.

pub mod apply;
pub mod patterns;
pub mod stale_artifact;
pub mod writer;

pub use apply::{RedactPolicy, Surface, apply};
pub use stale_artifact::{StaleArtifact, detect_stale};
pub use writer::{RedactWriter, redact_text};
