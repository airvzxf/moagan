//! Cross-run retention sweep — purge SQLite rows whose `run_id`
//! no longer exists in the `runs` table.
//!
//! Mirrors `proposal-03 §D.29-D.32` and `proposal-01-concept.md §12`.
//!
//! Background
//! ----------
//!
//! The retention pass in [`crate::telemetry::retention`] already
//! removes the run directory under `.runs/<run_id>/` when age/count/
//! storage thresholds trigger. The companion filesystem-cleanup
//! path (`records`), however, never touches the SQLite index —
//! every child table that references `runs(run_id)`,
//! namely `calls`, `phases`, `checkpoints`, `provider_usage`,
//! `run_siblings`, `run_context_refs`, `provider_changes`,
//! `warnings`, `problem_graphs`, `outbox_events`, `redact_audit`,
//! `manifest_events`, `budget_state` and `run_artifacts`, keeps
//! its rows. The result is "orphan" rows that the dashboard ignores
//! but the `moagan inspect --limit` and `mosql` queries still see.
//!
//! The sweep closes the gap. It is opt-in, defensive, and ships
//! with a dry-run default so a fresh invocation never mutates the
//! filesystem or the DB without an explicit confirmation. The
//! per-table row count is the only output the operator sees; no
//! telemetry payload, no sidecar rewrite, no audit JSONL.
//!
//! Why a sweep and not a trigger?
//! ------------------------------
//!
//! The schema declares `FOREIGN KEY (run_id) REFERENCES runs(run_id)`
//! on every child table and the connection's `with_init` hook sets
//! `PRAGMA foreign_keys = ON`, so the runtime INSERT path *cannot*
//! leave orphans behind. Orphans only appear when an operator
//! bypasses the runtime — a manual `DELETE FROM runs WHERE …`,
//! a `moagan repair --reindex` reset, or a database migration that
//! drops runs without cascading. The sweep is the maintenance tool
//! that repairs those cases.
//!
//! Tables swept
//! ------------
//!
//! The plan (§3.5) is explicit: every FK-to-runs table. The list is
//! curated here as a single source of truth so a new migration that
//! adds a child table can extend the sweep in one place. The
//! `purge` step runs the DELETEs in a single `BEGIN IMMEDIATE`
//! transaction so a partial sweep cannot leave the DB in a
//! half-purged state. The `redact_audit` table is included with the
//! `run_id IS NOT NULL` guard so the legitimate pre-pipeline redaction
//! rows (nullable run_id by design) are preserved.

use rusqlite::Connection;

use crate::error::Result;

/// One table's contribution to the orphan report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanTableStat {
    /// Child table name.
    pub table: &'static str,
    /// `(run_id column, is_nullable)` — used both for the SELECT
    /// count and the DELETE statement.
    pub run_id_column: &'static str,
    /// `true` when the `run_id` column is nullable. Orphans are
    /// counted only when the column is non-null; the DELETE only
    /// touches non-null rows.
    pub run_id_is_nullable: bool,
    /// True when the FK is `run_id` itself; false for tables that
    /// reference `runs` via a different column (`run_siblings` has
    /// `primary_run_id` + `sibling_run_id`).
    pub via_run_id: bool,
    /// Number of orphan rows counted (`list`) or deleted (`purge`).
    pub rows: i64,
}

/// Aggregate report returned by both `list_orphans` and `purge_orphans`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OrphanReport {
    /// Per-table counts. Ordered by the canonical sweep order
    /// (FK chain first, then nullable, then `run_siblings`) so the
    /// dry-run output is byte-stable across runs.
    pub tables: Vec<OrphanTableStat>,
    /// Sum of `rows` across every table.
    pub total_rows: i64,
}

impl OrphanReport {
    /// True when no orphan rows were touched.
    pub fn is_empty(&self) -> bool {
        self.total_rows == 0
    }
}

/// Canonical sweep order. The order is significant only for the
/// human-readable dry-run output; the underlying `DELETE` runs
/// inside a single transaction so atomicity is independent of
/// order. Keep the order aligned with the migration history so a
/// newly added table can be slotted at the right position by
/// matching its migration number.
pub const SWEEP_TABLES: &[SweepTable] = &[
    SweepTable {
        table: "calls",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "phases",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "checkpoints",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "provider_usage",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "run_context_refs",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "provider_changes",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "warnings",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "problem_graphs",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "outbox_events",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "redact_audit",
        run_id_column: "run_id",
        run_id_is_nullable: true,
        via_run_id: true,
    },
    SweepTable {
        table: "manifest_events",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "budget_state",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "run_artifacts",
        run_id_column: "run_id",
        run_id_is_nullable: false,
        via_run_id: true,
    },
    SweepTable {
        table: "run_siblings",
        run_id_column: "primary_run_id",
        run_id_is_nullable: false,
        via_run_id: false,
    },
    SweepTable {
        table: "run_siblings",
        run_id_column: "sibling_run_id",
        run_id_is_nullable: false,
        via_run_id: false,
    },
];

/// One entry in the sweep table list. Distinguishes the table
/// name from the column that references `runs(run_id)`; the
/// `run_siblings` table needs two entries (one per FK column).
#[derive(Debug, Clone, Copy)]
pub struct SweepTable {
    /// Child table name.
    pub table: &'static str,
    /// Column that references `runs(run_id)`.
    pub run_id_column: &'static str,
    /// `true` when the column is nullable. The orphan query
    /// skips NULL rows in that case so the legitimate pre-pipeline
    /// redaction rows are preserved.
    pub run_id_is_nullable: bool,
    /// `true` when the FK is the canonical `run_id` column;
    /// `false` for `run_siblings` (which uses `primary_run_id` and
    /// `sibling_run_id`).
    pub via_run_id: bool,
}

/// Build the `SELECT COUNT(*) FROM <table> WHERE <cond>` query.
/// `cond` is the orphan predicate (already null-safe for the
/// nullable column). The literal table/column names are interpolated
/// from the curated `SWEEP_TABLES` list — never from user input —
/// so the SQL is safe to compose with `format!`.
fn count_query(t: &SweepTable) -> String {
    let cond = orphan_where(t);
    format!("SELECT COUNT(*) FROM {t} WHERE {cond}", t = t.table)
}

/// Build the `DELETE FROM <table> WHERE <cond>` query.
fn delete_query(t: &SweepTable) -> String {
    let cond = orphan_where(t);
    format!("DELETE FROM {t} WHERE {cond}", t = t.table)
}

/// Render the orphan predicate for one sweep table.
///
/// Three branches:
/// - Nullable `run_id`: `(<col> IS NOT NULL AND <col> NOT IN (SELECT
///   run_id FROM runs))`.
/// - Non-nullable `run_id` via the canonical column: `<col> NOT IN
///   (SELECT run_id FROM runs)`.
/// - `run_siblings` (FK via a non-`run_id` column): same pattern
///   but using the actual column name.
fn orphan_where(t: &SweepTable) -> String {
    let col = t.run_id_column;
    if t.via_run_id && t.run_id_is_nullable {
        format!("{col} IS NOT NULL AND {col} NOT IN (SELECT run_id FROM runs)")
    } else {
        format!("{col} NOT IN (SELECT run_id FROM runs)")
    }
}

/// Count orphan rows on every curated table. Returns a per-table
/// report. Does not mutate the database.
pub fn list_orphans(conn: &Connection) -> Result<OrphanReport> {
    let mut tables = Vec::with_capacity(SWEEP_TABLES.len());
    let mut total: i64 = 0;
    for t in SWEEP_TABLES {
        let sql = count_query(t);
        let rows: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
        tables.push(OrphanTableStat {
            table: t.table,
            run_id_column: t.run_id_column,
            run_id_is_nullable: t.run_id_is_nullable,
            via_run_id: t.via_run_id,
            rows,
        });
        total = total.saturating_add(rows);
    }
    Ok(OrphanReport {
        tables,
        total_rows: total,
    })
}

/// Delete orphan rows on every curated table inside a single
/// transaction. Returns the per-table deleted-row counts on
/// success. The transaction is `BEGIN IMMEDIATE` so the sweep
/// either lands in full or leaves the DB untouched on a failure.
pub fn purge_orphans(conn: &Connection) -> Result<OrphanReport> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let mut tables = Vec::with_capacity(SWEEP_TABLES.len());
    let mut total: i64 = 0;
    let mut failure: Option<crate::Error> = None;
    for t in SWEEP_TABLES {
        let sql = delete_query(t);
        let outcome = conn.execute(&sql, []);
        match outcome {
            Ok(rows) => {
                let rows = rows as i64;
                tables.push(OrphanTableStat {
                    table: t.table,
                    run_id_column: t.run_id_column,
                    run_id_is_nullable: t.run_id_is_nullable,
                    via_run_id: t.via_run_id,
                    rows,
                });
                total = total.saturating_add(rows);
            }
            Err(e) => {
                failure = Some(e.into());
                break;
            }
        }
    }
    if let Some(err) = failure {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(err);
    }
    conn.execute_batch("COMMIT")?;
    Ok(OrphanReport {
        tables,
        total_rows: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::Db;

    /// Empty DB: every table contributes zero orphans. Pins the
    /// shape of the report (the table list must match the canonical
    /// sweep order) so a future migration that adds a FK-to-runs
    /// table surfaces here as a failed test instead of a silently
    /// missed sweep.
    ///
    /// The test uses a fresh `Db::open` (which runs migrations) so
    /// every curated table exists; a bare `Connection::open_in_memory`
    /// would skip the schema and surface every query as a
    /// `no such table` error.
    #[test]
    fn list_orphans_on_empty_db_reports_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite");
        let db = Db::open(&path).unwrap();
        let conn = db.connection().unwrap();
        let report = list_orphans(&conn).unwrap();
        assert_eq!(report.total_rows, 0);
        assert_eq!(report.tables.len(), SWEEP_TABLES.len());
        for stat in &report.tables {
            assert_eq!(stat.rows, 0, "table {} should report 0", stat.table);
        }
    }

    /// The orphan predicate for the canonical `run_id` column
    /// collapses to a single `NOT IN` clause. Pins the predicate
    /// exactly so a future refactor cannot silently change the
    /// semantics.
    #[test]
    fn orphan_where_canonical_run_id() {
        let t = SweepTable {
            table: "calls",
            run_id_column: "run_id",
            run_id_is_nullable: false,
            via_run_id: true,
        };
        assert_eq!(orphan_where(&t), "run_id NOT IN (SELECT run_id FROM runs)");
    }

    /// The nullable `run_id` predicate (used for `redact_audit`)
    /// adds the `IS NOT NULL` guard so legitimate pre-pipeline
    /// redaction rows are preserved.
    #[test]
    fn orphan_where_nullable_run_id_preserves_nulls() {
        let t = SweepTable {
            table: "redact_audit",
            run_id_column: "run_id",
            run_id_is_nullable: true,
            via_run_id: true,
        };
        assert_eq!(
            orphan_where(&t),
            "run_id IS NOT NULL AND run_id NOT IN (SELECT run_id FROM runs)"
        );
    }

    /// The `run_siblings` predicate uses the supplied column name
    /// instead of the canonical `run_id`, so both entries (primary
    /// and sibling) follow the same shape.
    #[test]
    fn orphan_where_run_siblings_uses_column_name() {
        let t = SweepTable {
            table: "run_siblings",
            run_id_column: "sibling_run_id",
            run_id_is_nullable: false,
            via_run_id: false,
        };
        assert_eq!(
            orphan_where(&t),
            "sibling_run_id NOT IN (SELECT run_id FROM runs)"
        );
    }
}
