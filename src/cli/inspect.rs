//! `moagan inspect` — list recent runs and show their status, or
//! drill into a single run with its warning summary.

use std::path::PathBuf;

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::{Db, WarningRow, WarningSummaryRow};

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

/// Warnings summary for a single run, as printed by
/// `moagan inspect <run_id>`.
#[derive(Debug, Clone)]
pub struct RunWarningsSummary {
    /// Run id.
    pub run_id: RunId,
    /// Aggregated counts per warning code.
    pub by_code: Vec<WarningSummaryRow>,
    /// Full ordered list of warnings (if any). Empty when the
    /// caller asks for the summary view only.
    pub all: Vec<WarningRow>,
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

/// Look up the warning summary for a single run. Returns
/// `Ok(None)` when the run id is not in the index. The summary is
/// empty (zero rows) when the run finished without any
/// auto-correction or retry events.
pub fn summarize_run(db: &Db, run_id: RunId) -> Result<Option<RunWarningsSummary>> {
    if db.get_run(run_id)?.is_none() {
        return Ok(None);
    }
    let by_code = db.warnings_summary(run_id)?;
    let all = db.list_warnings(run_id)?;
    Ok(Some(RunWarningsSummary {
        run_id,
        by_code,
        all,
    }))
}

/// Render a one-line summary of the run's warnings to stdout.
/// Used by `moagan inspect <run_id>`. The `verbose` flag also
/// prints every individual warning event.
pub fn print_run_summary(summary: &RunWarningsSummary, verbose: bool) {
    println!(
        "run {}  {} warning event(s) across {} code(s)",
        summary.run_id.short(),
        summary.all.len(),
        summary.by_code.len(),
    );
    if summary.by_code.is_empty() {
        println!("  (no model auto-corrections or retries recorded)");
        return;
    }
    for row in &summary.by_code {
        println!(
            "  [{}] x{}  {}",
            row.code,
            row.count,
            truncate(&row.first_message, 80),
        );
    }
    if verbose {
        println!();
        println!("events:");
        for row in &summary.all {
            let phase = row.phase.as_deref().unwrap_or("-");
            println!(
                "  +{}ms  [{}]  phase={}  {}",
                row.at_unix_ms,
                row.code,
                phase,
                truncate(&row.message, 80),
            );
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
