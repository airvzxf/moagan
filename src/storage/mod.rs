//! Storage layer. The filesystem is the canonical source of truth; the
//! SQLite database is the index (T01-06 §1.1).

pub mod compression;
pub mod lease;
pub mod sqlite;
