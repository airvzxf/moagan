//! `moagan audit` — external, transparent HTTP recorder for Moagan runs.
//!
//! Two surfaces are exposed:
//! - [`proxy`]: a small HTTP/1.1 forwarder that writes a per-line
//!   CRC32-sealed JSONL.gz record of every request/response that
//!   passes through it.
//! - [`verify`]: a verifier that cross-checks the external log
//!   against Moagan's internal `calls.jsonl.gz` + SQLite and reports
//!   coverage.
//!
//! See `docs/.../audit-design.md` (in this same directory) for the
//! rationale and the on-disk JSONL shape.

pub mod format;
pub mod proxy;
pub mod verify;
