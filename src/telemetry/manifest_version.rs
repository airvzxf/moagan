//! D.33.8: versioned manifest.
//!
//! When a sidecar is written, the manifest_version is also
//! recorded. Reads can detect drift and warn.

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Current numeric manifest schema version.
pub const CURRENT_MANIFEST_VERSION: u32 = 2;

/// Record the manifest schema version used for a run sidecar.
pub fn record_version(db: &Db, run_id: RunId, version: u32) -> Result<()> {
    db.connection()?.execute(
        "INSERT OR REPLACE INTO manifest_versions (run_id, manifest_version, written_at_unix) VALUES (?, ?, strftime('%s','now'))",
        rusqlite::params![run_id.to_string(), version as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CURRENT_MANIFEST_VERSION, record_version};
    use crate::ids::RunId;
    use crate::storage::sqlite::Db;
    #[test]
    fn v012_migration_creates_manifest_versions_table() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("meta.sqlite3")).unwrap();
        let table: String = db.connection().unwrap().query_row("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'manifest_versions'", [], |row| row.get(0)).unwrap();
        assert_eq!(table, "manifest_versions");
    }
    #[test]
    fn manifest_version_records_and_retrieves() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("meta.sqlite3")).unwrap();
        let run_id = RunId::new();
        record_version(&db, run_id, CURRENT_MANIFEST_VERSION).unwrap();
        let version: i64 = db
            .connection()
            .unwrap()
            .query_row(
                "SELECT manifest_version FROM manifest_versions WHERE run_id = ?",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 2);
    }
    #[test]
    fn current_head_migration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite3");
        let first = Db::open(&path).unwrap();
        first.run_migrations().unwrap();
        drop(first);
        let second = Db::open(&path).unwrap();
        let version: i64 = second
            .connection()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 15);
    }
    #[test]
    fn current_manifest_version_constant_is_2() {
        assert_eq!(CURRENT_MANIFEST_VERSION, 2);
    }
}
