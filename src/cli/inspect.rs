//! `moagan inspect` — list recent runs and show their status.

use std::path::PathBuf;

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Information about one run, as printed by `moagan inspect`.
#[derive(Debug, Clone)]
pub struct InspectEntry {
    /// Run id.
    pub run_id: RunId,
    /// Mode name.
    pub mode: String,
    /// Status string.
    pub status: String,
    /// Created unix seconds.
    pub created_unix: i64,
    /// Updated unix seconds.
    pub updated_unix: i64,
    /// Path to the run directory.
    pub path: PathBuf,
}

/// List recent runs ordered by creation time, descending.
pub fn list_recent(db: &Db, limit: u32) -> Result<Vec<InspectEntry>> {
    let rows = db.list_runs(limit)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let run_id: RunId = r.run_id.parse().unwrap_or_default();
            InspectEntry {
                run_id,
                mode: r.mode,
                status: r.status,
                created_unix: r.created_unix,
                updated_unix: r.updated_unix,
                path: PathBuf::new(),
            }
        })
        .collect())
}
