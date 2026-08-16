//! Storage layer. The filesystem is the canonical source of truth; the
//! SQLite database is the index (T01-06 §1.1).

pub mod compression;
pub mod lease;
pub mod outbox_tx;
pub mod sqlite;

// Typed process-lock API (T01-06 D.1.5). The legacy primitives on
// `Db::acquire_process_lock` / `Db::release_process_lock` remain
// available for callers that need the low-level caller-supplied
// fence string; the helpers below are the recommended entry point
// for new code.
pub use lease::{ProcessLease, acquire_process_lock, heartbeat_process_lock, release_process_lock};
