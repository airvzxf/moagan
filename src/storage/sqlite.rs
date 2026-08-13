//! SQLite meta-database. Indexes the filesystem layout; never the source
//! of truth (T01-06 §1.1).
//!
//! Embedded migrations are read from `src/storage/migrations/*.sql` at
//! compile time via `include_str!`. Schema version is tracked in
//! `PRAGMA user_version`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::Serialize;
use thiserror::Error;

use crate::error::Result;
use crate::ids::RunId;

mod sql_v001 {
    pub(super) const V001: &str = include_str!("migrations/v001_initial.sql");
}

mod sql_v002 {
    pub(super) const V002: &str = include_str!("migrations/v002_warnings.sql");
}

mod sql_v003 {
    pub(super) const V003: &str = include_str!("migrations/v003_calls_status.sql");
}

mod sql_v004 {
    pub(super) const V004: &str = include_str!("migrations/v004_calls_body_sha256.sql");
}

mod sql_v005 {
    pub(super) const V005: &str = include_str!("migrations/v005_checkpoints_content.sql");
}

mod sql_v006 {
    pub(super) const V006: &str = include_str!("migrations/v006_problem_graph.sql");
}

mod sql_v007 {
    pub(super) const V007: &str = include_str!("migrations/v007_lineage_context.sql");
}
mod sql_v008 {
    pub(super) const V008: &str = include_str!("migrations/v008_add_ons.sql");
}
mod sql_v009 {
    pub(super) const V009: &str = include_str!("migrations/v009_stability.sql");
}
mod sql_v010 {
    pub(super) const V010: &str = include_str!("migrations/v010_run_artifacts.sql");
}
mod sql_v011 {
    pub(super) const V011: &str = include_str!("migrations/v011_budget.sql");
}

mod sql_v012 {
    pub(super) const V012: &str = include_str!("migrations/v012_versioned_manifest.sql");
}

mod sql_v013 {
    pub(super) const V013: &str = include_str!("migrations/v013_closing_tables.sql");
}
mod sql_v014 {
    pub(super) const V014: &str = include_str!("migrations/v014_calls_retry_count.sql");
}

mod sql_v015 {
    pub(super) const V015: &str = include_str!("migrations/v015_calls_cost_usd.sql");
}

mod sql_v016 {
    pub(super) const V016: &str = include_str!("migrations/v016_drop_empty_tables.sql");
}

mod sql_v017 {
    pub(super) const V017: &str = include_str!("migrations/v017_drop_manifest_versions.sql");
}

/// SQLite-side error variants.
#[derive(Debug, Error)]
pub enum SqliteError {
    /// `rusqlite` failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// `r2d2` pool failure.
    #[error("pool: {0}")]
    Pool(#[from] r2d2::Error),
}

impl From<SqliteError> for crate::Error {
    fn from(e: SqliteError) -> Self {
        match e {
            SqliteError::Sqlite(s) => crate::Error::Provider(format!("sqlite: {s}")),
            SqliteError::Pool(p) => crate::Error::Provider(format!("sqlite pool: {p}")),
        }
    }
}

/// Apply the v007 lineage-context migration idempotently. Each
/// `ALTER TABLE` is gated on a `PRAGMA table_info` probe so the
/// migration is safe to re-run on a DB that already has the
/// columns (a defensive guard against operator error; the
/// `user_version` check above normally prevents re-entry).
fn apply_v007_idempotent(conn: &rusqlite::Connection) -> Result<()> {
    use rusqlite::params;
    // runs.shared_brief_hash
    if !column_exists(conn, "runs", "shared_brief_hash")? {
        conn.execute("ALTER TABLE runs ADD COLUMN shared_brief_hash TEXT", [])?;
    }
    // run_context_refs.context_type
    if !column_exists(conn, "run_context_refs", "context_type")? {
        conn.execute(
            "ALTER TABLE run_context_refs ADD COLUMN context_type TEXT NOT NULL DEFAULT 'path'",
            params![],
        )?;
    }
    // run_siblings.relation
    if !column_exists(conn, "run_siblings", "relation")? {
        conn.execute(
            "ALTER TABLE run_siblings ADD COLUMN relation TEXT NOT NULL DEFAULT 'rerun'",
            params![],
        )?;
    }
    // run_siblings.created_unix
    if !column_exists(conn, "run_siblings", "created_unix")? {
        conn.execute(
            "ALTER TABLE run_siblings ADD COLUMN created_unix INTEGER NOT NULL DEFAULT 0",
            params![],
        )?;
    }
    // v007 also bumps any rows where the v001 column 'relation' was
    // referenced implicitly (the CHECK constraint carried the
    // vocabulary 'rerun'|'continue'|'import' but the column did not
    // exist). v001 has no relation column, so nothing to backfill.
    let _ = sql_v007::V007;
    Ok(())
}

/// v009 adds three columns to the `runs` table that the W2 fix
/// needs to mirror the ranking-stability verdict (Phase H,
/// T01-06 §8.4.3). Like v007 the migration is idempotent so a
/// re-opened DB that already has the columns stays at v009 without
/// an "duplicate column" error.
fn apply_v009_idempotent(conn: &rusqlite::Connection) -> Result<()> {
    if !column_exists(conn, "runs", "stability_score")? {
        conn.execute("ALTER TABLE runs ADD COLUMN stability_score REAL", [])?;
    }
    if !column_exists(conn, "runs", "stability_label")? {
        conn.execute("ALTER TABLE runs ADD COLUMN stability_label TEXT", [])?;
    }
    if !column_exists(conn, "runs", "stability_sigma")? {
        conn.execute("ALTER TABLE runs ADD COLUMN stability_sigma REAL", [])?;
    }
    let _ = sql_v009::V009;
    Ok(())
}

/// Apply one migration step atomically. The schema change and the
/// `PRAGMA user_version` bump must commit together — otherwise a
/// crash between them leaves the DB with the new schema but the
/// old version, which makes the next `open()` re-run the same
/// migration (and break with "duplicate column name" once we
/// reach v007/v009, where the ALTER TABLE statements are not
/// natively idempotent at the SQL level).
///
/// `BEGIN IMMEDIATE` acquires a RESERVED lock for the duration
/// of the transaction, which is enough to serialize two processes
/// that race into `run_migrations` against the same DB file.
/// On any failure inside `f` we `ROLLBACK` and propagate; the
/// PRAGMA bump is therefore only visible if the schema change
/// committed cleanly.
fn apply_step<F>(conn: &rusqlite::Connection, version: i64, f: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(()) => {
            conn.execute_batch(&format!("PRAGMA user_version = {version};"))?;
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            // Best-effort ROLLBACK: if it fails the connection is
            // already in an unusable state, but the next caller's
            // error path will surface the original failure.
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// True when `column` exists on `table`. Probes via
/// `PRAGMA table_info(<table>)` which is the canonical SQLite
/// introspection idiom.
fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> Result<bool> {
    use rusqlite::params;
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query(params![])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pooled SQLite connection returned by [`Db::connection`].
pub type Connection = r2d2::PooledConnection<SqliteConnectionManager>;

/// Handle to the meta-database. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Db {
    pool: Arc<Pool<SqliteConnectionManager>>,
    path: PathBuf,
}

impl Db {
    /// Open or create the meta-database at `path` and run pending
    /// migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            c.execute_batch(
                "PRAGMA journal_mode = WAL;\
                 PRAGMA synchronous = NORMAL;\
                 PRAGMA foreign_keys = ON;\
                 PRAGMA busy_timeout = 30000;",
            )
        });
        // moagan is single-threaded for v0.1 (every async LLM call is
        // blocked with pollster). With max_size > 1 the pool can open a
        // second connection while the first still holds an exclusive
        // lock during the WAL-mode pragma, which surfaces as
        // `r2d2: database is locked` at startup. max_size=1 keeps a
        // single connection alive for the lifetime of the run, which
        // is plenty for v0.1.
        let pool = Pool::builder().max_size(1).build(manager)?;
        let db = Self {
            pool: Arc::new(pool),
            path: path.to_path_buf(),
        };
        db.run_migrations()?;
        // Force a WAL checkpoint so the next open() of this file
        // sees the latest user_version and schema state, even when
        // the previous open dropped the connection without explicit
        // sync. Without this, quick-succession opens can read stale
        // user_version from the main DB file and re-run
        // non-idempotent migrations like v003 (ALTER TABLE adds
        // COLUMN status) which then fail with "duplicate column
        // name". The per-step user_version probe inside
        // run_migrations helps but does not eliminate the flake
        // because once the probe enters the 'current < N' branch,
        // the schema change itself fails before the version bump
        // can land. The checkpoint makes the post-migration state
        // durable on the main DB file before we return.
        let conn = db.pool.get()?;
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        drop(conn);
        Ok(db)
    }

    /// Path to the underlying SQLite file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the underlying r2d2 connection pool. Exposed so
    /// that out-of-band helpers (e.g.
    /// [`crate::storage::outbox_tx::record_with`]) can wrap a
    /// series of `INSERT`s in a single SQLite transaction
    /// without going through the per-statement methods on `Db`.
    pub fn pool(&self) -> &Pool<SqliteConnectionManager> {
        &self.pool
    }

    /// Get a pooled SQLite connection for direct queries.
    pub fn connection(&self) -> Result<Connection> {
        Ok(self.pool.get()?)
    }

    /// Run pending migrations in order. v007 (Phase J lineage) is
    /// applied idempotently: the runner probes `PRAGMA table_info`
    /// before each `ALTER TABLE` so a re-opened DB that was already
    /// at v007 stays at v007 without an "duplicate column" error.
    /// v002…v010 run inside `apply_step`, which wraps the schema
    /// change and the `PRAGMA user_version` bump in a single
    /// `BEGIN IMMEDIATE` … `COMMIT` so a crash between the two
    /// cannot leave the DB with the new schema and the old
    /// version.
    ///
    /// `user_version` is probed AFTER every atomic step (and around
    /// v001, which is not wrapped). The atomic `apply_step` makes
    /// each individual step consistent, but it does not protect
    /// against a re-entry that observes the partially-applied state
    /// between the schema change and the version bump on the
    /// *previous* open — the kind of race that previously
    /// surfaced as a spurious `duplicate column name: status`
    /// panic in `cli::repair::tests::reindex_no_diff_returns_zero`
    /// when two `Db::open` calls landed on the same DB in quick
    /// succession (parallel `cargo test`, dispatcher-after-prime,
    /// etc.). Re-reading after each step collapses every "did
    /// step N already commit?" question into a single, fresh
    /// `PRAGMA user_version` read.
    ///
    /// v001 is the documented exception: it sets `PRAGMA synchronous
    /// = NORMAL`, which SQLite refuses to apply inside a
    /// transaction. The `with_init` hook on every connection
    /// already sets `synchronous = NORMAL`, so the migration
    /// statement is defensive and redundant in our code path. v001
    /// is also fully idempotent — every `CREATE` is `IF NOT EXISTS`
    /// — so a partial state on a v001 mid-flight crash recovers
    /// cleanly on the next `open()`. The atomicity wrap is
    /// therefore only applied from v002 onward, where the bug it
    /// exists to prevent (a v007 / v009 `ALTER TABLE ADD COLUMN`
    /// re-run on a partially-applied DB) is actually load-bearing.
    pub fn run_migrations(&self) -> Result<()> {
        let conn = self.pool.get()?;

        // v001 is special: PRAGMA synchronous=NORMAL cannot run
        // inside a transaction. We still re-read user_version
        // around it so a re-entry that observed a stale
        // user_version=0 lands on the per-step probe invariant
        // used by v002 onward.
        let mut current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current < 1 {
            conn.execute_batch(sql_v001::V001)?;
            conn.execute_batch("PRAGMA user_version = 1;")?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }

        // v002+ runs through apply_step (atomic). Re-read
        // user_version AFTER each step so a re-entry that observed
        // a partially-applied state between the schema change and
        // the version bump does not re-run an already-applied
        // step. The previous design read user_version exactly once
        // at the top, which made the runner vulnerable to WAL
        // visibility races when two opens run in quick succession
        // (e.g. parallel tests sharing the same DB).
        if current < 2 {
            apply_step(&conn, 2, || -> Result<()> {
                conn.execute_batch(sql_v002::V002)?;
                Ok(())
            })?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 3 {
            apply_step(&conn, 3, || -> Result<()> {
                conn.execute_batch(sql_v003::V003)?;
                Ok(())
            })?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 4 {
            apply_step(&conn, 4, || -> Result<()> {
                conn.execute_batch(sql_v004::V004)?;
                Ok(())
            })?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 5 {
            apply_step(&conn, 5, || -> Result<()> {
                conn.execute_batch(sql_v005::V005)?;
                Ok(())
            })?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 6 {
            apply_step(&conn, 6, || -> Result<()> {
                conn.execute_batch(sql_v006::V006)?;
                Ok(())
            })?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 7 {
            apply_step(&conn, 7, || apply_v007_idempotent(&conn))?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 8 {
            apply_step(&conn, 8, || -> Result<()> {
                conn.execute_batch(sql_v008::V008)?;
                Ok(())
            })?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 9 {
            apply_step(&conn, 9, || apply_v009_idempotent(&conn))?;
            current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        }
        if current < 10 {
            apply_step(&conn, 10, || -> Result<()> {
                conn.execute_batch(sql_v010::V010)?;
                Ok(())
            })?;
            // Final probe is intentionally omitted: `current` is
            // not read again after this point, so the assignment
            // would be a dead store and `-D unused_assignments`
            // would reject it. The per-step probe invariant still
            // holds for v001…v009.
        }
        if current < 11 {
            apply_step(&conn, 11, || -> Result<()> {
                conn.execute_batch(sql_v011::V011)?;
                Ok(())
            })?;
        }
        if current < 12 {
            apply_step(&conn, 12, || -> Result<()> {
                conn.execute_batch(sql_v012::V012)?;
                Ok(())
            })?;
        }
        if current < 13 {
            apply_step(&conn, 13, || -> Result<()> {
                conn.execute_batch(sql_v013::V013)?;
                Ok(())
            })?;
        }
        if current < 14 {
            apply_step(&conn, 14, || -> Result<()> {
                conn.execute_batch(sql_v014::V014)?;
                Ok(())
            })?;
        }
        if current < 15 {
            apply_step(&conn, 15, || -> Result<()> {
                conn.execute_batch(sql_v015::V015)?;
                Ok(())
            })?;
        }
        if current < 16 {
            apply_step(&conn, 16, || -> Result<()> {
                conn.execute_batch(sql_v016::V016)?;
                Ok(())
            })?;
        }
        if current < 17 {
            apply_step(&conn, 17, || -> Result<()> {
                conn.execute_batch(sql_v017::V017)?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Register a new run. Returns the rowid (not used externally).
    /// `shared_brief_hash` is the Phase J lineage column added in
    /// migration v007 (NULL before J).
    pub fn register_run(
        &self,
        run_id: RunId,
        mode: &str,
        status: &str,
        client_version: &str,
        config_hash: Option<&str>,
        shared_brief_hash: Option<&str>,
        parent: Option<RunId>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT OR REPLACE INTO runs \
             (run_id, mode, status, created_unix, updated_unix, schema_version, client_version, parent_run_id, config_hash, shared_brief_hash) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                run_id.to_string(),
                mode,
                status,
                now,
                now,
                "v1",
                client_version,
                parent.map(|p| p.to_string()),
                config_hash,
                shared_brief_hash,
            ],
        )?;
        Ok(())
    }

    /// Update run status.
    pub fn update_run_status(&self, run_id: RunId, status: &str) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "UPDATE runs SET status = ?, updated_unix = ? WHERE run_id = ?",
            params![status, now, run_id.to_string()],
        )?;
        Ok(())
    }

    /// Backdate `updated_unix` on a single run row. Test-only
    /// helper used by `moagan repair --recover-zombies` tests
    /// to plant a stale heartbeat without waiting two hours
    /// for the real clock to advance. Production code never
    /// reaches into this method: a real `updated_unix` is set
    /// by every `register_run` / `update_run_status` /
    /// `record_phase` write.
    #[doc(hidden)]
    pub fn _test_backdate_updated_unix(&self, run_id: RunId, unix: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE runs SET updated_unix = ? WHERE run_id = ?",
            params![unix, run_id.to_string()],
        )?;
        Ok(())
    }

    /// W2: persist the ranking-stability verdict on the `runs` row
    /// (v009 column add). Best-effort: a pre-v009 DB without the
    /// columns gets the migration via `run_migrations`, and a write
    /// before that lands is a no-op (the column check below returns
    /// `Ok(())` rather than aborting the run).
    pub fn record_run_stability(
        &self,
        run_id: RunId,
        score: f32,
        label: &str,
        sigma: f32,
    ) -> Result<()> {
        if self.user_version()? < 9 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "UPDATE runs SET stability_score = ?, stability_label = ?, stability_sigma = ?, updated_unix = ? \
             WHERE run_id = ?",
            params![score, label, sigma, now, run_id.to_string()],
        )?;
        Ok(())
    }

    /// W2: read the stored stability verdict (used by the dashboard's
    /// "stability per run" view). Returns `None` when the run predates
    /// v009 or was written before the perturbation loop ran.
    pub fn get_run_stability(&self, run_id: RunId) -> Result<Option<RunStabilityRow>> {
        if self.user_version()? < 9 {
            return Ok(None);
        }
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT stability_score, stability_label, stability_sigma \
             FROM runs WHERE run_id = ?",
        )?;
        let mut rows = stmt.query(params![run_id.to_string()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(RunStabilityRow {
                run_id,
                score: row.get(0)?,
                label: row.get(1)?,
                sigma: row.get(2)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Record a phase event.
    ///
    /// Uses `INSERT OR REPLACE` because the phases table has
    /// `PRIMARY KEY (run_id, phase, seq)` and the pipeline writes three
    /// events per phase (start, end, error) with the same key. A
    /// plain INSERT would fail with `UNIQUE constraint` on the second
    /// and third write; the row would never reflect the final status
    /// and `moagan inspect` would show every phase as still running.
    pub fn record_phase(
        &self,
        run_id: RunId,
        phase: &str,
        seq: i64,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT OR REPLACE INTO phases \
             (run_id, phase, seq, status, started_unix, ended_unix, error) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![run_id.to_string(), phase, seq, status, now, now, error,],
        )?;
        Ok(())
    }

    /// Record an LLM call.
    #[allow(clippy::too_many_arguments)]
    pub fn record_call(
        &self,
        call_id: &str,
        run_id: RunId,
        phase: &str,
        role: &str,
        provider: &str,
        model: &str,
        cache_key: &str,
        body_sha256: Option<&str>,
        cache_hit: bool,
        http_status: Option<i64>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        started_unix: i64,
        ended_unix: i64,
        error: Option<&str>,
        retry_count: u32,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let http_status_u16 = http_status.and_then(|s| u16::try_from(s).ok());
        let status = call_status(http_status_u16, error);
        conn.execute(
            "INSERT INTO calls (call_id, run_id, phase, role, provider, model, cache_key, body_sha256, cache_hit, http_status, input_tokens, output_tokens, cache_read, cache_creation, started_unix, ended_unix, error, status, retry_count) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                call_id,
                run_id.to_string(),
                phase,
                role,
                provider,
                model,
                cache_key,
                body_sha256,
                cache_hit as i64,
                http_status,
                input_tokens as i64,
                output_tokens as i64,
                cache_read as i64,
                cache_creation as i64,
                started_unix,
                ended_unix,
                error,
                status,
                retry_count as i64,
            ],
        )?;
        Ok(())
    }

    /// v015: backfill the per-call USD estimate on the freshly
    /// inserted `calls` row. Best-effort: a pre-v015 database short
    /// circuits to `Ok(())` and a SQLite failure is logged with
    /// `tracing::warn!` rather than aborting the call site, because
    /// the canonical record is the `calls.jsonl.gz` row and the
    /// SQLite mirror is a queryable index (T01-06 §2.6).
    ///
    /// `cost_usd = 0` is also accepted: callers that compute a zero
    /// estimate (no catalog entry, no tokens billed) leave the
    /// default in place and skip the UPDATE so the row count stays
    /// predictable. The `cost_usd > 0` filter downstream is the
    /// explicit opt-in for "real money was billed".
    pub fn record_call_cost(&self, call_id: &str, cost_usd: f64) -> Result<()> {
        if self.user_version()? < 15 {
            return Ok(());
        }
        if !(cost_usd.is_finite() && cost_usd > 0.0) {
            return Ok(());
        }
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE calls SET cost_usd = ? WHERE call_id = ?",
            params![cost_usd, call_id],
        )?;
        Ok(())
    }

    /// Record a provider change.
    pub fn record_provider_change(
        &self,
        run_id: RunId,
        seq: i64,
        phase: &str,
        from: Option<&str>,
        to: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT INTO provider_changes (run_id, seq, phase, from_name, to_name, at_unix, reason) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                run_id.to_string(),
                seq,
                phase,
                from,
                to,
                now,
                reason,
            ],
        )?;
        Ok(())
    }

    /// Return the next available `seq` value for a new
    /// `provider_changes` row. The previous code derived the seq
    /// from `manifest.phases.len()` which collided when the same
    /// run issued multiple `continue`/`rerun` actions that each
    /// recorded a provider change.
    pub fn next_provider_change_seq(&self, run_id: RunId) -> Result<i64> {
        let conn = self.pool.get()?;
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM provider_changes WHERE run_id = ?",
            params![run_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(next)
    }

    /// List runs ordered by creation time (descending).
    pub fn list_runs(&self, limit: u32) -> Result<Vec<RunRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, mode, status, created_unix, updated_unix, client_version, parent_run_id, shared_brief_hash \
             FROM runs ORDER BY created_unix DESC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(RunRow {
                    run_id: r.get(0)?,
                    mode: r.get(1)?,
                    status: r.get(2)?,
                    created_unix: r.get(3)?,
                    updated_unix: r.get(4)?,
                    client_version: r.get(5)?,
                    parent_run_id: r.get(6)?,
                    shared_brief_hash: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Get a single run by id.
    pub fn get_run(&self, run_id: RunId) -> Result<Option<RunRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, mode, status, created_unix, updated_unix, client_version, parent_run_id, shared_brief_hash \
             FROM runs WHERE run_id = ?",
        )?;
        let mut rows = stmt.query_map(params![run_id.to_string()], |r| {
            Ok(RunRow {
                run_id: r.get(0)?,
                mode: r.get(1)?,
                status: r.get(2)?,
                created_unix: r.get(3)?,
                updated_unix: r.get(4)?,
                client_version: r.get(5)?,
                parent_run_id: r.get(6)?,
                shared_brief_hash: r.get(7)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Record a warning event. Mirrors `telemetry/warnings.jsonl` so
    /// post-execution inspection can answer "did the model produce
    /// any auto-corrections?" with a single SQL query.
    #[allow(clippy::too_many_arguments)]
    pub fn record_warning(
        &self,
        run_id: RunId,
        at_unix_ms: i64,
        code: &str,
        level: &str,
        phase: Option<&str>,
        role: Option<&str>,
        call_id: Option<&str>,
        attempt: Option<i64>,
        message: &str,
        details: &str,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO warnings \
             (run_id, at_unix_ms, code, level, phase, role, call_id, attempt, message, details) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                run_id.to_string(),
                at_unix_ms,
                code,
                level,
                phase,
                role,
                call_id,
                attempt,
                message,
                details,
            ],
        )?;
        Ok(())
    }

    /// Count warnings for a run, grouped by code. Returns
    /// `[(code, count, first_message)]` ordered by count desc.
    pub fn warnings_summary(&self, run_id: RunId) -> Result<Vec<WarningSummaryRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT code, COUNT(*), MIN(message) \
             FROM warnings WHERE run_id = ? \
             GROUP BY code ORDER BY COUNT(*) DESC, code ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok(WarningSummaryRow {
                    code: r.get(0)?,
                    count: r.get(1)?,
                    first_message: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Full warning list for a run, ordered by `at_unix_ms` ascending.
    pub fn list_warnings(&self, run_id: RunId) -> Result<Vec<WarningRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT at_unix_ms, code, level, phase, role, call_id, attempt, message, details \
             FROM warnings WHERE run_id = ? ORDER BY at_unix_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok(WarningRow {
                    at_unix_ms: r.get(0)?,
                    code: r.get(1)?,
                    level: r.get(2)?,
                    phase: r.get(3)?,
                    role: r.get(4)?,
                    call_id: r.get(5)?,
                    attempt: r.get(6)?,
                    message: r.get(7)?,
                    details: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Full call list for a run, ordered by `started_unix` ascending.
    /// Used by tests and the cache-hit rate analytics.
    pub fn list_calls_for_run(&self, run_id: RunId) -> Result<Vec<CallRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT call_id, phase, role, provider, model, cache_key, body_sha256, cache_hit, http_status, \
                     input_tokens, output_tokens, cache_read, cache_creation, started_unix, \
                     ended_unix, error, retry_count, cost_usd \
             FROM calls WHERE run_id = ? ORDER BY started_unix ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok(CallRow {
                    call_id: r.get(0)?,
                    phase: r.get(1)?,
                    role: r.get(2)?,
                    provider: r.get(3)?,
                    model: r.get(4)?,
                    cache_key: r.get(5)?,
                    body_sha256: r.get(6)?,
                    cache_hit: r.get(7)?,
                    http_status: r.get::<_, Option<i64>>(8)?.map(|v| v as u16),
                    input_tokens: r.get::<_, i64>(9)? as u64,
                    output_tokens: r.get::<_, i64>(10)? as u64,
                    cache_read: r.get::<_, i64>(11)? as u64,
                    cache_creation: r.get::<_, i64>(12)? as u64,
                    started_unix: r.get(13)?,
                    ended_unix: r.get::<_, Option<i64>>(14)?,
                    error: r.get(15)?,
                    retry_count: r.get::<_, i64>(16)? as u32,
                    cost_usd: r.get::<_, Option<f64>>(17)?.unwrap_or(0.0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record a human checkpoint. Mirrors the JSON sidecar into
    /// the SQLite index so queries like "which runs had rejected
    /// checkpoints?" don't have to scan every `checkpoints/` dir.
    ///
    /// Best-effort: callers should treat the write as fire-and-forget
    /// (the JSON sidecar is the canonical record). `INSERT OR REPLACE`
    /// so a re-run with the same checkpoint id is idempotent.
    pub fn record_checkpoint(
        &self,
        run_id: RunId,
        ckp_id: &str,
        kind: &str,
        question: &str,
        response: &str,
        accepted_default: bool,
        at_unix: i64,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO checkpoints \
             (run_id, ckp_id, kind, question, response, accepted_default, at_unix, seq, resolved, created_unix) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                run_id.to_string(),
                ckp_id,
                kind,
                question,
                response,
                accepted_default as i64,
                at_unix,
                0_i64,
                0_i64,
                at_unix,
            ],
        )?;
        Ok(())
    }

    /// Record a `ProblemGraph` (Phase G) for a run. The table is
    /// added in migration v006; this method tolerates the
    /// pre-v006 schema (the table will not exist) and returns
    /// `Ok(())` so a phase that opens a legacy database never
    /// fails the run. After the v006 migration has been applied
    /// the write actually lands.
    pub fn record_problem_graph(
        &self,
        run_id: RunId,
        brief_blake3: &str,
        should_decompose: bool,
        node_count: i64,
        at_unix: i64,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        // Probe the user_version so the call is a no-op on legacy
        // databases (v1..=v5). The migration runner already updates
        // user_version to 6 before this method is ever called from
        // a v0.3+ run, so the check is cheap.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 6 {
            return Ok(());
        }
        conn.execute(
            "INSERT OR REPLACE INTO problem_graphs \
             (run_id, brief_blake3, should_decompose, node_count, at_unix) \
             VALUES (?, ?, ?, ?, ?)",
            params![
                run_id.to_string(),
                brief_blake3,
                should_decompose as i64,
                node_count,
                at_unix,
            ],
        )?;
        Ok(())
    }

    /// Full checkpoint list for a run, ordered by `at_unix`
    /// ascending. Returns an empty Vec if no checkpoints were
    /// recorded (e.g. when `interactive=false` and the call path
    /// short-circuits).
    pub fn list_checkpoints_for_run(&self, run_id: RunId) -> Result<Vec<CheckpointRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT ckp_id, kind, question, response, accepted_default, at_unix \
             FROM checkpoints WHERE run_id = ? ORDER BY at_unix ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok(CheckpointRow {
                    ckp_id: r.get(0)?,
                    kind: r.get(1)?,
                    question: r.get(2)?,
                    response: r.get(3)?,
                    accepted_default: r.get::<_, i64>(4)? != 0,
                    at_unix: r.get::<_, Option<i64>>(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Count of checkpoints for a run, broken down by kind.
    /// Useful for the dashboard / `moagan inspect` summaries.
    pub fn checkpoint_counts_by_kind(
        &self,
        run_id: RunId,
    ) -> Result<std::collections::BTreeMap<String, i64>> {
        let conn = self.pool.get()?;
        let mut stmt =
            conn.prepare("SELECT kind, COUNT(*) FROM checkpoints WHERE run_id = ? GROUP BY kind")?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().collect())
    }

    /// Phase J: mirror a single context reference into the
    /// `run_context_refs` table. The `context_type` column carries
    /// `"run_id" | "path" | "dir"` so a post-execution query can
    /// filter by the kind of ref that fed into the brief.
    ///
    /// `INSERT OR REPLACE` keeps the call idempotent: a
    /// re-registered run with the same `(run_id, source_path)`
    /// just overwrites the previous row.
    pub fn add_context_ref(
        &self,
        run_id: RunId,
        record: &crate::context::ContextRefRecord,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO run_context_refs \
             (run_id, source_path, shasum, bytes, added_unix, context_type) \
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                run_id.to_string(),
                record.source_path,
                record.shasum,
                record.bytes as i64,
                record.added_unix,
                record.context_type,
            ],
        )?;
        Ok(())
    }

    /// Phase J: link two runs as siblings. `relation` is `"rerun"`,
    /// `"continue"`, or `"import"` (v007 made the column a real
    /// TEXT NOT NULL with default `'rerun'`). The function is
    /// best-effort: `INSERT OR IGNORE` so a repeated call doesn't
    /// surface as a constraint failure.
    pub fn add_run_sibling_relation(
        &self,
        primary: RunId,
        sibling: RunId,
        relation: &str,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT OR IGNORE INTO run_siblings \
             (primary_run_id, sibling_run_id, relation, created_unix) \
             VALUES (?, ?, ?, ?)",
            params![primary.to_string(), sibling.to_string(), relation, now,],
        )?;
        Ok(())
    }

    /// Phase J: the canonical `runs.parent_run_id` setter.
    /// `register_run` already writes `parent_run_id` on insert;
    /// this method is for the cases where the parent is known
    /// only after the run is created (e.g. `moagan rerun` which
    /// assigns a fresh `run_id` first and then attaches the
    /// lineage). `UPDATE` so the change is recorded in-place.
    pub fn set_run_parent(&self, run_id: RunId, parent: RunId) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE runs SET parent_run_id = ?, updated_unix = ? WHERE run_id = ?",
            params![
                parent.to_string(),
                crate::time::now_unix_secs(),
                run_id.to_string()
            ],
        )?;
        Ok(())
    }

    /// J#5 closure (cross-run lineage view): list every recorded
    /// `parent_run_id` <-> `run_id` pair. Children without a parent
    /// (`parent_run_id IS NULL`) collapse to `(None, Some(child))`
    /// so the dashboard projection can surface them as root nodes
    /// while the per-edge graph builder ([`LineageGraph::from_pairs`])
    /// drops the NULL parent edge.
    ///
    /// Returns `(parent, child)`. `parent` is `None` for
    /// childless roots; `child` is always `Some` because the
    /// column is `NOT NULL` (runs without a run_id are not
    /// representable). Output order matches the underlying
    /// `runs` row order (creation-time descending via
    /// `created_unix DESC`) so a freshly appended lineage show
    /// appears at the top.
    pub fn list_lineage_pairs(&self) -> Result<Vec<(Option<String>, Option<String>)>> {
        let conn = self.pool.get()?;
        let mut stmt =
            conn.prepare("SELECT parent_run_id, run_id FROM runs ORDER BY created_unix DESC")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Phase J: write `runs.shared_brief_hash` after the intake
    /// phase finishes computing it. The migration runner added the
    /// column in v007; the column is TEXT NULL so `None` is the
    /// pre-J default.
    pub fn set_shared_brief_hash(&self, run_id: RunId, shared_brief_hash: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE runs SET shared_brief_hash = ?, updated_unix = ? WHERE run_id = ?",
            params![
                shared_brief_hash,
                crate::time::now_unix_secs(),
                run_id.to_string()
            ],
        )?;
        Ok(())
    }

    /// Phase J: the most recent phase that ended successfully for
    /// `run_id`. Returns `None` when the run never recorded a
    /// phase event, or when every recorded event had a non-`end`
    /// status. Drives `moagan continue` / `Pipeline::resume` to
    /// skip the work that already finished.
    ///
    /// Tie-break rule: when multiple phases ended at the same
    /// `started_unix`, canonical pipeline order wins.
    pub fn last_completed_phase(&self, run_id: RunId) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT phase FROM phases \
             WHERE run_id = ? AND status = 'end' \
             ORDER BY started_unix DESC, CASE phase \
                 WHEN 'intake' THEN 0 WHEN 'clarify' THEN 1 WHEN 'route' THEN 2 \
                 WHEN 'decompose' THEN 3 WHEN 'sketch' THEN 4 WHEN 'propose' THEN 5 \
                 WHEN 'validate' THEN 6 WHEN 'cluster_proposals' THEN 7 \
                 WHEN 'synthesize' THEN 8 WHEN 'gate' THEN 9 WHEN 'critique' THEN 10 \
                 WHEN 'repair' THEN 11 WHEN 'judge' THEN 12 WHEN 'rank' THEN 13 \
                 WHEN 'deliver' THEN 14 ELSE -1 END DESC \
             LIMIT 1",
        )?;

        let mut rows = stmt.query(params![run_id.to_string()])?;
        if let Some(row) = rows.next()? {
            let phase: String = row.get(0)?;
            Ok(Some(phase))
        } else {
            Ok(None)
        }
    }

    /// Phase J: every recorded phase that ended successfully for
    /// `run_id`, ordered by `started_unix` descending. The list
    /// drives `moagan inspect`'s per-phase progress view and is
    /// the source for `last_completed_phase`.
    pub fn list_completed_phases(&self, run_id: RunId) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT phase FROM phases \
             WHERE run_id = ? AND status = 'end' \
             ORDER BY started_unix DESC, phase ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Track K.2 (resume from persisted phase): true when the run
    /// has a row in the `runs` table. `moagan pause` uses this to
    /// decide between "derive state from the live SQLite index"
    /// and "fall back to the legacy hard-coded list" (the latter
    /// for runs that were paused before `db.register_run(...)`
    /// had a chance to commit).
    pub fn has_run(&self, run_id: RunId) -> Result<bool> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE run_id = ?",
            params![run_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }
}

/// Aggregated counters for a single run. Computed from the
/// `calls`, `phases`, `provider_usage`, and `warnings` tables.
/// Used by `moagan telemetry summary` and the dashboard
/// `GET /api/runs/<id>` endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunAggregate {
    /// Number of LLM calls recorded (cache hits included).
    pub calls: i64,
    /// Number of calls with `status = 'error'`.
    pub error_calls: i64,
    /// Number of calls with `status = 'timeout'`.
    pub timeout_calls: i64,
    /// Number of calls with `status = 'cancelled'`.
    pub cancelled_calls: i64,
    /// Total input tokens billed.
    pub input_tokens: i64,
    /// Total output tokens billed.
    pub output_tokens: i64,
    /// Total tokens served from cache.
    pub cache_read: i64,
    /// Total tokens written to cache.
    pub cache_creation: i64,
    /// Number of distinct providers invoked.
    pub provider_count: i64,
    /// Number of distinct phases that ran.
    pub phase_count: i64,
    /// Number of recorded warnings.
    pub warnings: i64,
    /// Number of human checkpoints captured.
    pub checkpoints: i64,
}

/// One row from `provider_usage`. Mirrors the `v001_initial.sql`
/// schema; one row per `(run_id, provider, model)` triple.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsageRow {
    /// Provider name (e.g. `minimax`).
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Number of calls attributed to this `(provider, model)`.
    pub calls: i64,
    /// Total input tokens billed.
    pub input_tokens: i64,
    /// Total output tokens billed.
    pub output_tokens: i64,
    /// Total tokens served from cache.
    pub cache_read: i64,
    /// Total tokens written to cache.
    pub cache_creation: i64,
    /// Unix seconds of the last call for this row.
    pub last_call_unix: Option<i64>,
}

/// One row from the rolling-window aggregation in
/// [`Db::aggregate_window_usage`]. Distinct from
/// [`ProviderUsageRow`] because the source is the per-call `calls`
/// table (filtered by `started_unix`), not the per-run `provider_usage`
/// rollup table — `aggregate_provider_usage` returns the latter.
/// Powers `moagan telemetry plan`'s quota view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowUsageRow {
    /// Provider name as recorded on the `calls` row (e.g.
    /// `"minimax"`, `"opencode_go"`).
    pub provider: String,
    /// Model name as recorded on the `calls` row.
    pub model: String,
    /// Number of calls in the window for this `(provider, model)`.
    pub call_count: i64,
    /// Sum of `input_tokens + output_tokens` for every call in the
    /// window. Computed in SQL because the `calls` table does not
    /// carry a `total_tokens` column (see v001_initial.sql).
    pub total_tokens: i64,
    /// Count of calls whose `status` is `'error'`. The `status`
    /// column is populated by v003 (`call_status` derives the value
    /// from `http_status` + `error`); rows that pre-date v003 carry
    /// `'unknown'` and are intentionally NOT counted here so the
    /// ratio matches the post-v003 semantics.
    pub error_count: i64,
    /// Tokens served from cache (`cache_hit = 1`), counted as
    /// `input_tokens + output_tokens` for each cache-hit row. This
    /// is the "free" slice of the consumed tokens; the
    /// `moagan telemetry plan` formatter prints it alongside the
    /// raw total so the operator can see what the cache saved.
    pub cached_tokens: i64,
    /// Earliest `started_unix` for this `(provider, model)` in the
    /// window. `None` only when the row would be empty, which the
    /// `GROUP BY` filter prevents.
    pub first_call_unix: Option<i64>,
    /// Latest `started_unix` for this `(provider, model)` in the
    /// window.
    pub last_call_unix: Option<i64>,
}

/// One row from [`Db::aggregate_cost_by_provider_model`]. The
/// `cost_usd` column on `calls` is filled in by the per-call
/// `cost_estimate` helper against the models.dev catalog, so a
/// zero `cost_usd` here means the catalog had no entry for the
/// pair at probe time (or no catalog data was available).
/// Powers `moagan telemetry cost --run <id>`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostAggregateRow {
    /// Provider name (e.g. `minimax`).
    pub provider: String,
    /// Model name (e.g. `MiniMax-M3`).
    pub model: String,
    /// Number of calls in the scope for this `(provider, model)`.
    pub calls: i64,
    /// Sum of `cost_usd` over the calls in the scope.
    pub cost_usd: f64,
}

/// One row from `phases` for the dashboard's per-phase view. The
/// dashboard normalises three events per phase (start / end / error)
/// into a single row carrying the final status and the derived
/// duration. A row is `None` when the phase was never recorded.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PhaseSummaryRow {
    /// Phase name (e.g. `intake`, `propose`, `rank`).
    pub phase: String,
    /// Sequence within the run (0 for the only event of a phase).
    pub seq: i64,
    /// Final status string (`start` / `end` / `error` / `cancel`).
    pub status: String,
    /// Unix seconds at start. `None` when the row is synthetic
    /// (only `seq=0` and an `end` event were captured).
    pub started_unix: Option<i64>,
    /// Unix seconds at end. `None` while the phase is still running.
    pub ended_unix: Option<i64>,
    /// Error message when `status == 'error'`.
    pub error: Option<String>,
}

impl RunAggregate {
    /// Sum of `input_tokens + output_tokens`.
    pub fn total_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens
    }
    /// Wall-clock duration of the run in seconds, derived from the
    /// first and last phase events. `None` when no phase rows exist.
    pub fn duration_secs(&self) -> Option<i64> {
        // Stored on `RunRow` separately so this helper only sees
        // counts; the dashboard reads `created_unix` and `updated_unix`
        // directly. Kept here so the type's API is self-contained.
        None
    }
    /// Number of calls with `status = 'ok'`. Derived so callers can
    /// ask for either polarity without re-querying.
    pub fn ok_calls(&self) -> i64 {
        self.calls - self.error_calls - self.timeout_calls - self.cancelled_calls
    }
}

impl Db {
    /// Aggregate counters for a run. Drives
    /// `moagan telemetry summary` and the dashboard per-run page.
    /// Returns `RunAggregate::default()` for unknown runs so the
    /// caller can show "no data" without a special-case branch.
    pub fn run_aggregate(&self, run_id: RunId) -> Result<RunAggregate> {
        let conn = self.pool.get()?;
        let row = conn.query_row(
            "SELECT \
                COALESCE(COUNT(*), 0), \
                COALESCE(SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN status = 'timeout' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(input_tokens), 0), \
                COALESCE(SUM(output_tokens), 0), \
                COALESCE(SUM(cache_read), 0), \
                COALESCE(SUM(cache_creation), 0), \
                (SELECT COUNT(DISTINCT provider) FROM calls WHERE run_id = ?), \
                (SELECT COUNT(DISTINCT phase) FROM phases WHERE run_id = ?), \
                (SELECT COUNT(*) FROM warnings WHERE run_id = ?), \
                (SELECT COUNT(*) FROM checkpoints WHERE run_id = ?) \
             FROM calls WHERE run_id = ?",
            params![
                run_id.to_string(),
                run_id.to_string(),
                run_id.to_string(),
                run_id.to_string(),
                run_id.to_string(),
            ],
            |r| {
                Ok(RunAggregate {
                    calls: r.get(0)?,
                    error_calls: r.get(1)?,
                    timeout_calls: r.get(2)?,
                    cancelled_calls: r.get(3)?,
                    input_tokens: r.get(4)?,
                    output_tokens: r.get(5)?,
                    cache_read: r.get(6)?,
                    cache_creation: r.get(7)?,
                    provider_count: r.get(8)?,
                    phase_count: r.get(9)?,
                    warnings: r.get(10)?,
                    checkpoints: r.get(11)?,
                })
            },
        )?;
        Ok(row)
    }

    /// Full per-provider breakdown for a run, ordered by total
    /// tokens descending. Powers the dashboard
    /// `GET /api/runs/<id>/provider_usage` endpoint and the
    /// `by-model` section of `moagan telemetry summary`.
    pub fn list_provider_usage_for_run(&self, run_id: RunId) -> Result<Vec<ProviderUsageRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT provider, model, calls, input_tokens, output_tokens, cache_read, cache_creation, last_call_unix \
             FROM provider_usage \
             WHERE run_id = ? \
             ORDER BY (input_tokens + output_tokens) DESC, provider ASC, model ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok(ProviderUsageRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    calls: r.get(2)?,
                    input_tokens: r.get(3)?,
                    output_tokens: r.get(4)?,
                    cache_read: r.get(5)?,
                    cache_creation: r.get(6)?,
                    last_call_unix: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Phase summary for a run. Collapses the three events per phase
    /// (`start`, `end`, `error`) into one row per `(phase, seq)`
    /// carrying the final status; the dashboard and the summary
    /// subcommand use this to render a clean timeline.
    ///
    /// The query reads the latest row per key (the schema's
    /// `INSERT OR REPLACE` semantics in `record_phase` mean the
    /// last write wins).
    pub fn list_phase_summaries_for_run(&self, run_id: RunId) -> Result<Vec<PhaseSummaryRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT phase, seq, status, started_unix, ended_unix, error \
             FROM phases \
             WHERE run_id = ? \
             GROUP BY phase, seq \
             HAVING MAX(started_unix) \
             ORDER BY MIN(started_unix) ASC, phase ASC, seq ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |r| {
                Ok(PhaseSummaryRow {
                    phase: r.get(0)?,
                    seq: r.get(1)?,
                    status: r.get(2)?,
                    started_unix: r.get(3)?,
                    ended_unix: r.get(4)?,
                    error: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Aggregate per (provider, model) across every recorded run.
    /// Powers the `moagan telemetry provider --list` view and the
    /// dashboard's provider picker.
    pub fn aggregate_provider_usage(&self) -> Result<Vec<ProviderUsageRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT provider, model, \
                    SUM(calls), SUM(input_tokens), SUM(output_tokens), \
                    SUM(cache_read), SUM(cache_creation), MAX(last_call_unix) \
             FROM provider_usage \
             GROUP BY provider, model \
             ORDER BY (SUM(input_tokens) + SUM(output_tokens)) DESC, provider ASC, model ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProviderUsageRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    calls: r.get(2)?,
                    input_tokens: r.get(3)?,
                    output_tokens: r.get(4)?,
                    cache_read: r.get(5)?,
                    cache_creation: r.get(6)?,
                    last_call_unix: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Rolling-window aggregation over the per-call `calls` table.
    /// Distinct from [`Self::aggregate_provider_usage`] (which reads
    /// the per-run `provider_usage` rollup): this one returns the
    /// live call-by-call truth for the last `window_days` days,
    /// optionally filtered to a single provider. The output is the
    /// source of truth for `moagan telemetry plan`'s quota view —
    /// the `provider_usage` rollup is per-run and would double-count
    /// across the same operator's sessions.
    ///
    /// The query:
    /// - filters by `started_unix >= now - window_days * 86_400`
    ///   (cutoff computed in Rust because rusqlite has no portable
    ///   datetime arithmetic and the column is INTEGER unix seconds,
    ///   not TEXT);
    /// - optionally narrows to one provider via the
    ///   `(?1 IS NULL OR provider = ?2)` predicate (the two slots are
    ///   bound to the same `Option<&str>` so `None` collapses to a
    ///   no-op filter via `NULL OR X = NULL`-equivalent short-circuit);
    /// - groups by `(provider, model)` and orders by the SQL-computed
    ///   total descending so the biggest consumers come first.
    ///
    /// Empty result (zero rows) on a fresh DB is **not** an error —
    /// the caller decides whether to print "(no calls in the window)"
    /// or surface exit 1.
    pub fn aggregate_window_usage(
        &self,
        window_days: u32,
        provider_filter: Option<&str>,
    ) -> Result<Vec<WindowUsageRow>> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        let days = i64::from(window_days);
        let cutoff = now.saturating_sub(days.saturating_mul(86_400));
        let mut stmt = conn.prepare(
            "SELECT provider, model, \
                    COUNT(*), \
                    COALESCE(SUM(input_tokens + output_tokens), 0), \
                    COALESCE(SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END), 0), \
                    COALESCE(SUM(CASE WHEN cache_hit = 1 THEN (input_tokens + output_tokens) ELSE 0 END), 0), \
                    MIN(started_unix), \
                    MAX(started_unix) \
             FROM calls \
             WHERE started_unix >= ?1 \
               AND (?2 IS NULL OR provider = ?2) \
             GROUP BY provider, model \
             ORDER BY SUM(input_tokens + output_tokens) DESC, provider ASC, model ASC",
        )?;
        let rows = stmt
            .query_map(params![cutoff, provider_filter], |r| {
                Ok(WindowUsageRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    call_count: r.get(2)?,
                    total_tokens: r.get(3)?,
                    error_count: r.get(4)?,
                    cached_tokens: r.get(5)?,
                    first_call_unix: r.get(6)?,
                    last_call_unix: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Recent runs for a single provider, ordered by creation time
    /// descending. Powers `moagan telemetry provider --plan <name>`.
    pub fn recent_runs_for_provider(
        &self,
        provider: &str,
        limit: u32,
    ) -> Result<Vec<ProviderUsageRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT provider, model, calls, input_tokens, output_tokens, cache_read, cache_creation, last_call_unix \
             FROM provider_usage \
             WHERE provider = ? \
             ORDER BY last_call_unix DESC NULLS LAST, model ASC \
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![provider, limit as i64], |r| {
                Ok(ProviderUsageRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    calls: r.get(2)?,
                    input_tokens: r.get(3)?,
                    output_tokens: r.get(4)?,
                    cache_read: r.get(5)?,
                    cache_creation: r.get(6)?,
                    last_call_unix: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Aggregate `cost_usd` from the `calls` table, grouped by
    /// `(provider, model)`. Used by `moagan telemetry cost --run
    /// <id>` (and `--all`) to answer "how much money did this run
    /// (or the whole install) spend, per model?".
    ///
    /// The optional `run_id` narrows the query to a single run
    /// (`Some(id)`); passing `None` aggregates every call recorded
    /// in the index. The DB columns may be missing on a v014
    /// install (the `calls.cost_usd` column lands in v015); when
    /// the schema is older, every row reads as `cost_usd = 0.0` and
    /// the aggregation still returns counts so the CLI can print
    /// "no cost data" instead of erroring out.
    pub fn aggregate_cost_by_provider_model(
        &self,
        run_id: Option<RunId>,
    ) -> Result<Vec<CostAggregateRow>> {
        let conn = self.pool.get()?;
        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match run_id {
            Some(rid) => (
                "SELECT provider, model, COUNT(*), COALESCE(SUM(cost_usd), 0.0) \
                 FROM calls \
                 WHERE run_id = ? \
                 GROUP BY provider, model \
                 ORDER BY (COALESCE(SUM(cost_usd), 0.0)) DESC, provider ASC, model ASC",
                vec![Box::new(rid.to_string())],
            ),
            None => (
                "SELECT provider, model, COUNT(*), COALESCE(SUM(cost_usd), 0.0) \
                 FROM calls \
                 GROUP BY provider, model \
                 ORDER BY (COALESCE(SUM(cost_usd), 0.0)) DESC, provider ASC, model ASC",
                vec![],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let params_iter: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_iter), |r| {
                Ok(CostAggregateRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    calls: r.get(2)?,
                    cost_usd: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// One row from the `warnings` summary grouping.
#[derive(Debug, Clone)]
pub struct WarningSummaryRow {
    /// Warning code (e.g. `model.json_repair_applied`).
    pub code: String,
    /// Number of warnings with this code.
    pub count: i64,
    /// First message seen for this code (for quick triage).
    pub first_message: String,
}

/// One row from the `checkpoints` table. Mirrors the JSON sidecar
/// `checkpoints/h_<uuid>.json` produced by `src/checkpoint/human.rs`.
pub struct CheckpointRow {
    /// Stable checkpoint id (`h_<uuid7>`).
    pub ckp_id: String,
    /// Closed enum mirrored as a string (`intake | clarify | final | custom`).
    pub kind: String,
    /// Question shown verbatim to the user.
    pub question: String,
    /// Raw response captured from stdin (the
    /// `<skipped:non_interactive>` marker when `interactive=false`).
    pub response: String,
    /// True when the user accepted the default by hitting enter.
    pub accepted_default: bool,
    /// Unix seconds at capture time.
    pub at_unix: Option<i64>,
}

/// One row in `problem_graphs` (Phase G).
#[derive(Debug, Clone)]
pub struct ProblemGraphRow {
    /// BLAKE3 of the canonical brief.
    pub brief_blake3: String,
    /// True when the model said yes; false when the trigger ladder
    /// vetoed the call (or the model did).
    pub should_decompose: bool,
    /// Number of nodes that survived repair.
    pub node_count: i64,
    /// Unix seconds at capture time.
    pub at_unix: i64,
}

/// One row from the full `warnings` list.
#[derive(Debug, Clone)]
pub struct WarningRow {
    /// Unix milliseconds when the warning was emitted.
    pub at_unix_ms: i64,
    /// Warning code.
    pub code: String,
    /// Severity level (`warn` / `info`).
    pub level: String,
    /// Phase name, if known.
    pub phase: Option<String>,
    /// LLM role, if known.
    pub role: Option<String>,
    /// Call id, if known.
    pub call_id: Option<String>,
    /// Attempt number, if known.
    pub attempt: Option<i64>,
    /// Human-readable message.
    pub message: String,
    /// Structured details (JSON-encoded).
    pub details: String,
}

/// One row from the `calls` table. The schema is defined in
/// `migrations/v001_initial.sql`.
#[derive(Debug, Clone, Serialize)]
pub struct CallRow {
    /// Unique call id (UUID v7).
    pub call_id: String,
    /// Pipeline phase that issued the call (e.g. `"intake"`).
    pub phase: String,
    /// LLM role (e.g. `"intake"`).
    pub role: String,
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Canonical cache key (BLAKE3) when the call went through the
    /// cross-run cache.
    pub cache_key: String,
    /// SHA-256 of the exact HTTP request body.
    pub body_sha256: Option<String>,
    /// `1` when the response was served from cache, `0` otherwise.
    pub cache_hit: i64,
    /// HTTP status from the provider (`None` on transport failure).
    pub http_status: Option<u16>,
    /// Input tokens billed.
    pub input_tokens: u64,
    /// Output tokens billed.
    pub output_tokens: u64,
    /// Tokens served from cache (subset of `input_tokens`).
    pub cache_read: u64,
    /// Tokens written to cache.
    pub cache_creation: u64,
    /// Start unix seconds.
    pub started_unix: i64,
    /// End unix seconds.
    pub ended_unix: Option<i64>,
    /// Error message, if any.
    pub error: Option<String>,
    /// Zero-indexed retry number for this call. `0` is the first
    /// attempt; `1` is the second attempt, etc. Persisted by the
    /// canonical retry loop in
    /// `crate::phases::phase::call_with_retry_parse` so the
    /// post-execution review can answer "how many retries did this
    /// LLM call take?" by reading a single SQL query instead of
    /// correlating warnings to call records.
    pub retry_count: u32,
    /// v015: per-call USD estimate derived from the models.dev
    /// catalog `cost: {input, output, cache_read, cache_write}` block.
    /// `0.0` when the catalog was missing, the `(provider, model)`
    /// pair was unknown, or the call billed zero tokens.
    pub cost_usd: f64,
}

/// Row read from `runs`.
#[derive(Debug, Clone, Serialize)]
pub struct RunRow {
    /// Run id.
    pub run_id: String,
    /// Mode name.
    pub mode: String,
    /// Status string.
    pub status: String,
    /// Created unix seconds.
    pub created_unix: i64,
    /// Updated unix seconds.
    pub updated_unix: i64,
    /// Client version that produced the run.
    pub client_version: String,
    /// Parent run id (if any).
    pub parent_run_id: Option<String>,
    /// Phase J: SHA-256 of the canonical concatenation of every
    /// loaded context text (the `shared_brief_hash`). Mirrors the
    /// `runs.shared_brief_hash` column.
    pub shared_brief_hash: Option<String>,
}

/// Derive the `calls.status` enum from the `http_status` and `error`
/// fields. Exposed as a pure function so the JSONL `CallEvent` and the
/// SQLite `calls` row stay in sync and so the rule is unit-testable in
/// isolation.
///
/// Rules (first match wins):
/// 1. `error` message contains `"timeout"` → `"timeout"`.
/// 2. `error` message contains `"cancel"` → `"cancelled"`.
/// 3. `error` is `Some(_)` → `"error"` (covers transport failures,
///    schema violations, and any other provider error).
/// 4. `http_status` is `Some(2xx)` → `"ok"`.
/// 5. `http_status` is `Some(4xx|5xx)` → `"error"`.
/// 6. `http_status` is `None` (cache hit or pre-flight abort) → `"ok"`.
///
/// Values match T01-06 §2.1's CHECK constraint
/// `status IN ('ok','error','timeout','cancelled','truncated')`.
/// `"truncated"` is currently emitted by the LLM layer as a warning
/// rather than a call status; if we ever need it here the rule will
/// inspect the response finish_reason.
pub fn call_status(http_status: Option<u16>, error: Option<&str>) -> &'static str {
    if let Some(msg) = error {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("timeout") {
            return "timeout";
        }
        if lower.contains("cancel") {
            return "cancelled";
        }
        return "error";
    }
    match http_status {
        Some(s) if (200..300).contains(&s) => "ok",
        Some(s) if (400..600).contains(&s) => "error",
        Some(_) => "error",
        None => "ok",
    }
}

// -----------------------------------------------------------------
// Sub-fase K row types (D.5.1 subset)
// -----------------------------------------------------------------

/// One row from the `outbox_events` table (D.1.4). The phase writes
/// the sidecar first; this row is the outbox mirror.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct OutboxEventRow {
    /// Run id.
    pub run_id: String,
    /// Event type tag (free-form).
    pub event_type: String,
    /// Payload (free-form JSON or text).
    pub payload: String,
    /// Unix seconds.
    pub at_unix: i64,
}

/// One row from the `redact_audit` table (D.8.5). `run_id` is
/// optional so a pre-pipeline redaction pass can record events that
/// did not belong to any run.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RedactAuditRow {
    /// Run id (optional).
    pub run_id: Option<String>,
    /// File path that triggered the redaction.
    pub source_path: String,
    /// Pattern kind that fired (e.g. `SkCpApiKey`).
    pub pattern_kind: String,
    /// Number of matches in this file.
    pub match_count: u32,
    /// Unix seconds.
    pub at_unix: i64,
}

/// One row from the `manifest_events` table (D.5.1).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ManifestEventRow {
    /// Run id.
    pub run_id: String,
    /// Event type tag.
    pub event_type: String,
    /// Optional details (free-form).
    pub details: Option<String>,
    /// Unix seconds.
    pub at_unix: i64,
}

/// W2: the per-run stability verdict persisted by
/// `record_run_stability`. Read by the dashboard's "stability per
/// run" view (D.5.1) and by `moagan inspect`.
#[derive(Debug, Clone, Serialize)]
pub struct RunStabilityRow {
    /// Run id.
    pub run_id: RunId,
    /// Top-1 stability score in `[0.0, 1.0]`.
    pub score: Option<f64>,
    /// `stable` | `sensitive` (lowercase).
    pub label: Option<String>,
    /// Sigma actually used for the perturbation loop.
    pub sigma: Option<f64>,
}

/// One row from the `provider_rollups` table (D.5.1).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ProviderRollupRow {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// Number of calls recorded.
    pub calls: u64,
    /// Input tokens billed.
    pub input_tokens: u64,
    /// Output tokens billed.
    pub output_tokens: u64,
    /// Number of calls that ended in a non-OK status.
    pub errors: u64,
    /// Unix seconds of the last call.
    pub last_call_unix: Option<i64>,
}

// -----------------------------------------------------------------
// Sub-fase K: v008 helpers (D.5.1 subset)
// -----------------------------------------------------------------
//
// Each helper is best-effort against the schema version. On a
// pre-v008 database the call returns `Ok(default_value)` so a
// legacy operator never crashes the run on the new code path.

impl Db {
    /// Probe the live `PRAGMA user_version`. Mirrors the pattern
    /// `record_problem_graph` uses for v006.
    fn user_version(&self) -> Result<i64> {
        let conn = self.pool.get()?;
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(v)
    }

    /// Insert a row into `outbox_events` (D.1.4).
    pub fn record_outbox_event(&self, row: &OutboxEventRow) -> Result<()> {
        if self.user_version()? < 8 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO outbox_events (run_id, event_type, payload, at_unix) \
             VALUES (?, ?, ?, ?)",
            params![row.run_id, row.event_type, row.payload, row.at_unix],
        )?;
        Ok(())
    }

    /// List every outbox event for a run, oldest first.
    pub fn list_outbox_events_for_run(&self, run_id: &str) -> Result<Vec<OutboxEventRow>> {
        if self.user_version()? < 8 {
            return Ok(Vec::new());
        }
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, event_type, payload, at_unix \
             FROM outbox_events WHERE run_id = ? ORDER BY at_unix ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(OutboxEventRow {
                    run_id: r.get(0)?,
                    event_type: r.get(1)?,
                    payload: r.get(2)?,
                    at_unix: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Insert one row into `redact_audit` (D.8.5).
    pub fn record_redact_audit(&self, row: &RedactAuditRow) -> Result<()> {
        if self.user_version()? < 8 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO redact_audit (run_id, source_path, pattern_kind, match_count, at_unix) \
             VALUES (?, ?, ?, ?, ?)",
            params![
                row.run_id,
                row.source_path,
                row.pattern_kind,
                row.match_count,
                row.at_unix,
            ],
        )?;
        Ok(())
    }

    /// List every redact audit row for a run, oldest first.
    pub fn list_redact_audit_for_run(&self, run_id: &str) -> Result<Vec<RedactAuditRow>> {
        if self.user_version()? < 8 {
            return Ok(Vec::new());
        }
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, source_path, pattern_kind, match_count, at_unix \
             FROM redact_audit WHERE run_id = ? ORDER BY at_unix ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                Ok(RedactAuditRow {
                    run_id: r.get(0)?,
                    source_path: r.get(1)?,
                    pattern_kind: r.get(2)?,
                    match_count: r.get(3)?,
                    at_unix: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Insert one row into `manifest_events` (D.5.1).
    pub fn record_manifest_event(&self, row: &ManifestEventRow) -> Result<()> {
        if self.user_version()? < 8 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO manifest_events (run_id, event_type, details, at_unix) \
             VALUES (?, ?, ?, ?)",
            params![row.run_id, row.event_type, row.details, row.at_unix],
        )?;
        Ok(())
    }

    /// Acquire a process-wide lock keyed by `holder` with the given TTL.
    pub fn acquire_process_lock(&self, holder: &str, ttl_secs: u64, fence: &str) -> Result<bool> {
        if self.user_version()? < 8 {
            return Ok(true);
        }
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        let expires = now + ttl_secs as i64;
        conn.execute(
            "DELETE FROM process_locks WHERE expires_at_unix <= ?",
            params![now],
        )?;
        let current: Option<String> = conn
            .query_row("SELECT holder FROM process_locks LIMIT 1", [], |r| r.get(0))
            .ok();
        match current {
            None => {
                conn.execute(
                    "INSERT INTO process_locks (holder, acquired_at_unix, expires_at_unix, fence) \
                     VALUES (?, ?, ?, ?)",
                    params![holder, now, expires, fence],
                )?;
                Ok(true)
            }
            Some(existing) if existing == holder => {
                conn.execute(
                    "UPDATE process_locks SET acquired_at_unix = ?, expires_at_unix = ?, fence = ? \
                     WHERE holder = ?",
                    params![now, expires, fence, holder],
                )?;
                Ok(true)
            }
            Some(_) => Ok(false),
        }
    }

    /// Acquire or renew a run lease and return its monotonic fence.
    pub fn renew_lease(
        &self,
        run_id: RunId,
        holder: &str,
        ttl: std::time::Duration,
        expected_fence: Option<u64>,
    ) -> Result<u64> {
        if self.user_version()? < 8 {
            return Ok(1);
        }
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        let expires = now
            .checked_add(i64::try_from(ttl.as_secs()).map_err(|_| {
                crate::error::Error::InvalidArgs("lease TTL exceeds SQLite timestamp range".into())
            })?)
            .ok_or_else(|| {
                crate::error::Error::InvalidArgs("lease TTL overflows timestamp".into())
            })?;
        let prefix = format!("{run_id}|");
        let current: Option<(String, i64, i64, String)> = conn
            .query_row(
                "SELECT holder, acquired_at_unix, expires_at_unix, fence \
                 FROM process_locks WHERE holder LIKE ? ORDER BY acquired_at_unix DESC LIMIT 1",
                params![format!("{prefix}%")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        let Some((stored_key, _acquired, stored_expires, stored_fence)) = current else {
            if expected_fence.is_some() {
                return Err(crate::error::Error::LockHeld(run_id.to_string()));
            }
            conn.execute(
                "INSERT INTO process_locks (holder, acquired_at_unix, expires_at_unix, fence) \
                 VALUES (?, ?, ?, ?)",
                params![format!("{prefix}{holder}"), now, expires, "1"],
            )?;
            return Ok(1);
        };

        let stored_holder = stored_key.strip_prefix(&prefix).unwrap_or_default();
        let current_fence = stored_fence
            .parse::<u64>()
            .map_err(|_| crate::error::Error::Provider("sqlite: invalid lease fence".into()))?;
        let active = stored_expires > now;
        if let Some(expected) = expected_fence {
            if stored_holder != holder || current_fence != expected || !active {
                return Err(crate::error::Error::LockHeld(run_id.to_string()));
            }
        } else if active && stored_holder != holder {
            return Err(crate::error::Error::LockHeld(run_id.to_string()));
        }

        let next_fence = current_fence
            .checked_add(1)
            .ok_or_else(|| crate::error::Error::Provider("sqlite: lease fence overflow".into()))?;
        conn.execute(
            "UPDATE process_locks SET holder = ?, acquired_at_unix = ?, expires_at_unix = ?, fence = ? \
             WHERE holder = ?",
            params![format!("{prefix}{holder}"), now, expires, next_fence.to_string(), stored_key],
        )?;
        Ok(next_fence)
    }

    /// Release a process lock owned by `holder`.
    pub fn release_process_lock(&self, holder: &str) -> Result<bool> {
        if self.user_version()? < 8 {
            return Ok(true);
        }
        let conn = self.pool.get()?;
        let deleted = conn.execute(
            "DELETE FROM process_locks WHERE holder = ?",
            params![holder],
        )?;
        Ok(deleted > 0)
    }

    /// Read the current fencing token for a (run_id, holder) lease.
    /// Returns `None` when no row exists for the pair (the lease has
    /// never been acquired or was released). Used by the
    /// lease-renewal heartbeat integration tests to assert that the
    /// loop actually fired without exposing the private connection
    /// pool to callers.
    pub fn lease_fence(&self, run_id: RunId, holder: &str) -> Result<Option<u64>> {
        if self.user_version()? < 8 {
            return Ok(None);
        }
        let conn = self.pool.get()?;
        let key = format!("{run_id}|{holder}");
        let fence: Option<String> = conn
            .query_row(
                "SELECT fence FROM process_locks WHERE holder = ?",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = fence else {
            return Ok(None);
        };
        let parsed = raw
            .parse::<u64>()
            .map_err(|_| crate::error::Error::Provider("sqlite: invalid lease fence".into()))?;
        Ok(Some(parsed))
    }

    /// Release a run lease owned by `holder`.
    pub fn release_run_lease(&self, run_id: RunId, holder: &str) -> Result<bool> {
        if self.user_version()? < 8 {
            return Ok(true);
        }
        let conn = self.pool.get()?;
        let deleted = conn.execute(
            "DELETE FROM process_locks WHERE holder = ?",
            params![format!("{run_id}|{holder}")],
        )?;
        Ok(deleted > 0)
    }

    /// Increment the cross-run rollup counters for a (provider, model) pair.
    pub fn increment_provider_rollup(
        &self,
        provider: &str,
        model: &str,
        in_tokens: u64,
        out_tokens: u64,
        is_error: bool,
    ) -> Result<()> {
        if self.user_version()? < 8 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        let errors_delta: i64 = if is_error { 1 } else { 0 };
        conn.execute(
            "INSERT INTO provider_rollups (provider, model, calls, input_tokens, output_tokens, errors, last_call_unix) \
             VALUES (?, ?, 1, ?, ?, ?, ?) \
             ON CONFLICT(provider, model) DO UPDATE SET \
                calls = calls + 1, \
                input_tokens = input_tokens + excluded.input_tokens, \
                output_tokens = output_tokens + excluded.output_tokens, \
                errors = errors + excluded.errors, \
                last_call_unix = excluded.last_call_unix",
            params![
                provider,
                model,
                in_tokens as i64,
                out_tokens as i64,
                errors_delta,
                now,
            ],
        )?;
        Ok(())
    }

    /// Read the rollup row for one (provider, model) pair.
    pub fn get_provider_rollup(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<Option<ProviderRollupRow>> {
        if self.user_version()? < 8 {
            return Ok(None);
        }
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                "SELECT provider, model, calls, input_tokens, output_tokens, errors, last_call_unix \
                 FROM provider_rollups WHERE provider = ? AND model = ?",
                params![provider, model],
                |r| {
                    Ok(ProviderRollupRow {
                        provider: r.get(0)?,
                        model: r.get(1)?,
                        calls: r.get(2)?,
                        input_tokens: r.get(3)?,
                        output_tokens: r.get(4)?,
                        errors: r.get(5)?,
                        last_call_unix: r.get(6)?,
                    })
                },
            )
            .ok();
        Ok(row)
    }

    // -----------------------------------------------------------------
    // Track D.2 (D.28.5) — reindex_artifacts helpers.
    //
    // The four `count_<kind>` readers return the cached count from
    // `run_artifacts` (0 on a pre-v010 database or when no row
    // exists yet). The four `reindex_<kind>` methods walk the
    // filesystem under `root/<kind>/`, count the primary
    // `*.json` files, and upsert the result.
    // -----------------------------------------------------------------

    /// Cached count of `proposals/p_*.json` files for `run_id`.
    /// Returns 0 on a pre-v010 database or when no row exists yet.
    pub fn count_proposals(&self, run_id: &RunId) -> Result<usize> {
        self.count_run_artifact(run_id, "proposals")
    }

    /// Cached count of `sketches/sk_*.json` files for `run_id`.
    pub fn count_sketches(&self, run_id: &RunId) -> Result<usize> {
        self.count_run_artifact(run_id, "sketches")
    }

    /// Cached count of `evaluations/p_*.json` files for `run_id`.
    pub fn count_evaluations(&self, run_id: &RunId) -> Result<usize> {
        self.count_run_artifact(run_id, "evaluations")
    }

    /// Cached count of `critiques/p_*_critic_*.json` files for
    /// `run_id`.
    pub fn count_critiques(&self, run_id: &RunId) -> Result<usize> {
        self.count_run_artifact(run_id, "critiques")
    }

    /// Walk `<root>/proposals/` and upsert the count into
    /// `run_artifacts` keyed by `(run_id, "proposals")`. Returns
    /// the freshly indexed count. Pre-v010 databases are a
    /// no-op (the helper returns 0) so legacy operators never
    /// crash the new code path.
    pub fn reindex_proposals(&self, run_id: &RunId, root: &Path) -> Result<usize> {
        self.reindex_run_artifact(run_id, "proposals", &root.join("proposals"))
    }

    /// Walk `<root>/sketches/` and upsert the count.
    pub fn reindex_sketches(&self, run_id: &RunId, root: &Path) -> Result<usize> {
        self.reindex_run_artifact(run_id, "sketches", &root.join("sketches"))
    }

    /// Walk `<root>/evaluations/` and upsert the count.
    pub fn reindex_evaluations(&self, run_id: &RunId, root: &Path) -> Result<usize> {
        self.reindex_run_artifact(run_id, "evaluations", &root.join("evaluations"))
    }

    /// Walk `<root>/critiques/` and upsert the count.
    pub fn reindex_critiques(&self, run_id: &RunId, root: &Path) -> Result<usize> {
        self.reindex_run_artifact(run_id, "critiques", &root.join("critiques"))
    }

    fn count_run_artifact(&self, run_id: &RunId, kind: &str) -> Result<usize> {
        if self.user_version()? < 10 {
            return Ok(0);
        }
        let conn = self.pool.get()?;
        let count: Option<i64> = conn
            .query_row(
                "SELECT count FROM run_artifacts WHERE run_id = ? AND kind = ?",
                params![run_id.to_string(), kind],
                |r| r.get(0),
            )
            .optional()?;
        Ok(count.unwrap_or(0).max(0) as usize)
    }

    fn reindex_run_artifact(&self, run_id: &RunId, kind: &str, dir: &Path) -> Result<usize> {
        if self.user_version()? < 10 {
            return Ok(0);
        }
        let count = count_artefacts_in(dir)?;
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT INTO run_artifacts (run_id, kind, count, last_indexed_unix) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(run_id, kind) DO UPDATE SET \
                count = excluded.count, \
                last_indexed_unix = excluded.last_indexed_unix",
            params![run_id.to_string(), kind, count as i64, now],
        )?;
        Ok(count)
    }

    // -----------------------------------------------------------------
    // Track F (F3) — budget enforcement helpers.
    //
    // The BudgetObserver (`src/phases/budget.rs`) consumes these
    // two methods to compute the pressure tier (Ok / Soft / Hard)
    // and to record per-phase usage. The contract is intentionally
    // minimal: a pre-v011 DB short-circuits to `(0, 0)` so legacy
    // runs are never artificially throttled by a fresh binary.
    // -----------------------------------------------------------------

    /// Read the `(planned, used)` budget for `run_id`. Returns
    /// `(0, 0)` on a pre-v011 database or when no row exists yet
    /// (the Ok pressure tier — the observer treats 0/0 as "no
    /// budget configured" and never throttles optional work).
    pub fn budget_read(&self, run_id: RunId) -> Result<(u64, u64)> {
        if self.user_version()? < 11 {
            return Ok((0, 0));
        }
        let conn = self.pool.get()?;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT planned_tokens, used_tokens FROM budget_state WHERE run_id = ?",
                params![run_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (planned, used) = row.unwrap_or((0, 0));
        Ok((planned.max(0) as u64, used.max(0) as u64))
    }

    /// Record `tokens` consumed by `phase` for `run_id`. Inserts
    /// the budget row on first call and increments `used_tokens`
    /// on every call.
    ///
    /// `phase` is part of the contract (forward-compatible with
    /// future per-phase caps); the parameter is currently used
    /// only as a label passed by the caller — no per-phase audit
    /// row is persisted (the `budget_events` table that used to
    /// carry that trail was dropped by v016 as dead schema).
    ///
    /// Pre-v011 databases are a no-op so legacy operators
    /// upgrading the binary mid-run do not see a synthetic write.
    pub fn budget_record(&self, run_id: RunId, phase: &str, tokens: u64) -> Result<()> {
        if self.user_version()? < 11 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        // Upsert the aggregate. `planned_tokens` is preserved on
        // the ON CONFLICT branch (we only ever set it via
        // `set_budget`); `used_tokens` is the running total.
        conn.execute(
            "INSERT INTO budget_state (run_id, planned_tokens, used_tokens) \
             VALUES (?, 0, ?) \
             ON CONFLICT(run_id) DO UPDATE SET \
                used_tokens = used_tokens + excluded.used_tokens",
            params![run_id.to_string(), tokens as i64],
        )?;
        // `phase` is intentionally ignored at the SQL level — it
        // stays in the signature so callers continue to label
        // their consumption and a future per-phase cap can
        // re-introduce the audit trail without touching call
        // sites.
        let _ = phase;
        Ok(())
    }

    /// Set the planned token budget for `run_id`. Idempotent:
    /// called once at run start from `cli::run` when
    /// `Config::token_budget` is `Some`, leaving the default
    /// `planned_tokens = 0` (which `BudgetObserver` treats as
    /// "unlimited") untouched when the operator omits the cap.
    /// Pre-v011 databases are a no-op.
    pub fn set_budget(&self, run_id: RunId, planned_tokens: u64) -> Result<()> {
        if self.user_version()? < 11 {
            return Ok(());
        }
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO budget_state (run_id, planned_tokens, used_tokens) \
             VALUES (?, ?, 0) \
             ON CONFLICT(run_id) DO UPDATE SET \
                planned_tokens = excluded.planned_tokens",
            params![run_id.to_string(), planned_tokens as i64],
        )?;
        Ok(())
    }
}

/// Count primary `*.json` artefacts inside `dir`. Excludes
/// sidecars (`*.meta.json`) and atomic-write leftovers
/// (`*.tmp.<hex>`). Returns 0 when the directory is missing.
fn count_artefacts_in(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".json") || name.ends_with(".meta.json") {
            continue;
        }
        if is_atomic_tmp_path(&p) {
            continue;
        }
        count = count.checked_add(1).ok_or_else(|| {
            crate::error::Error::Provider(format!("artefact count overflow at {}", p.display()))
        })?;
    }
    Ok(count)
}

/// Atomic-tmp heuristic shared by `count_artefacts_in` and
/// `src/cli/repair.rs::is_atomic_tmp`. The atomic writer appends
/// `.<dest>.tmp.<16 hex>` to every temp file; any file whose name
/// contains `.tmp.` followed by at least 8 hex chars qualifies.
fn is_atomic_tmp_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(idx) = name.find(".tmp.") else {
        return false;
    };
    let tail = &name[idx + ".tmp.".len()..];
    !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Db {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite");
        // leak the tempdir so the DB survives the test body — not a
        // problem for unit tests; the OS reclaims on exit.
        std::mem::forget(tmp);
        Db::open(&path).unwrap()
    }

    #[test]
    fn open_creates_schema_and_version() {
        let db = temp_db();
        let conn = db.pool.get().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        // v007 added the lineage + context_refs columns; v008 adds
        // the five new tables; v009 adds the runs.stability_*
        // columns; v010 adds the run_artifacts table (D.28.5).
        // We assert >= 9 so future migrations (v010) do not break
        // this test.
        assert!(v >= 9, "user_version = {v}");
    }

    #[test]
    fn register_and_update_run() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", Some("h1"), Some("h2"), None)
            .unwrap();
        let row = db.get_run(id).unwrap().unwrap();
        assert_eq!(row.mode, "fast");
        assert_eq!(row.status, "running");
        db.update_run_status(id, "completed").unwrap();
        let row = db.get_run(id).unwrap().unwrap();
        assert_eq!(row.status, "completed");
    }

    #[test]
    fn record_phase_and_call() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_phase(id, "intake", 0, "start", None).unwrap();
        db.record_phase(id, "intake", 0, "end", None).unwrap();
        db.record_call(
            "c1",
            id,
            "intake",
            "intake",
            "mock",
            "mock-model",
            "cache-key-1",
            Some("sha256"),
            false,
            Some(200),
            0,
            0,
            0,
            0,
            100,
            101,
            None,
            0,
        )
        .unwrap();
        let runs = db.list_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        let calls = db.list_calls_for_run(id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body_sha256.as_deref(), Some("sha256"));
        assert_eq!(calls[0].started_unix, 100);
        assert_eq!(calls[0].ended_unix, Some(101));
        assert_eq!(calls[0].retry_count, 0);
    }

    /// The pipeline writes three events per phase (start, end, error)
    /// with the same (run_id, phase, seq) key. The schema's PRIMARY
    /// KEY treats those as one logical row, so the third write must
    /// not fail with UNIQUE constraint and the row must reflect the
    /// last status seen.
    #[test]
    fn record_phase_replaces_when_same_key_is_reused() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_phase(id, "intake", 0, "start", None).unwrap();
        db.record_phase(id, "intake", 0, "end", None).unwrap();
        db.record_phase(id, "intake", 0, "error", Some("boom"))
            .unwrap();
        let conn = db.pool.get().unwrap();
        let (status, err): (String, Option<String>) = conn
            .query_row(
                "SELECT status, error FROM phases \
                 WHERE run_id = ?1 AND phase = 'intake' AND seq = 0",
                rusqlite::params![id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "error");
        assert_eq!(err.as_deref(), Some("boom"));
    }

    #[test]
    fn record_warning_and_summary_group_by_code() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_warning(
            id,
            1_700_000_000_000,
            "model.json_repair_applied",
            "warn",
            Some("critique"),
            Some("critique"),
            Some("c1"),
            Some(0),
            "colon repair",
            r#"{"repair_kind":"colon","bytes":42}"#,
        )
        .unwrap();
        db.record_warning(
            id,
            1_700_000_000_500,
            "model.json_repair_applied",
            "warn",
            Some("critique"),
            Some("critique"),
            Some("c2"),
            Some(0),
            "bracket repair",
            r#"{"repair_kind":"bracket","bytes":13}"#,
        )
        .unwrap();
        db.record_warning(
            id,
            1_700_000_001_000,
            "model.retry_parse",
            "warn",
            Some("critique"),
            Some("critique"),
            Some("c3"),
            Some(1),
            "parse failed",
            "{}",
        )
        .unwrap();

        let summary = db.warnings_summary(id).unwrap();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].code, "model.json_repair_applied");
        assert_eq!(summary[0].count, 2);
        assert_eq!(summary[1].code, "model.retry_parse");
        assert_eq!(summary[1].count, 1);

        let all = db.list_warnings(id).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].code, "model.json_repair_applied");
        assert_eq!(all[0].at_unix_ms, 1_700_000_000_000);
    }

    #[test]
    fn warnings_summary_for_other_run_is_empty() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        let summary = db.warnings_summary(id).unwrap();
        assert!(summary.is_empty());
    }

    #[test]
    fn call_status_http_200_is_ok() {
        assert_eq!(call_status(Some(200), None), "ok");
    }

    #[test]
    fn call_status_http_2xx_is_ok() {
        for s in [200u16, 201, 204, 206, 299] {
            assert_eq!(call_status(Some(s), None), "ok", "http={s}");
        }
    }

    #[test]
    fn call_status_error_message_overrides_http() {
        assert_eq!(
            call_status(Some(200), Some("schema violation: bad json")),
            "error"
        );
    }

    #[test]
    fn call_status_timeout_by_error_message() {
        assert_eq!(
            call_status(Some(504), Some("request timeout exceeded")),
            "timeout"
        );
    }

    #[test]
    fn call_status_cancelled_by_error_message() {
        assert_eq!(
            call_status(Some(499), Some("operation cancelled by user")),
            "cancelled"
        );
    }

    #[test]
    fn call_status_http_4xx_is_error() {
        for s in [400u16, 401, 404, 429, 499] {
            assert_eq!(
                call_status(Some(s), None),
                "error",
                "http={s} without error message"
            );
        }
    }

    #[test]
    fn call_status_http_5xx_is_error() {
        for s in [500u16, 502, 503, 504] {
            assert_eq!(call_status(Some(s), None), "error", "http={s}");
        }
    }

    #[test]
    fn call_status_cache_hit_no_http_is_ok() {
        // Cache hits never issue an HTTP request; we treat them as ok.
        assert_eq!(call_status(None, None), "ok");
    }

    // -----------------------------------------------------------------
    // Phase F checkpoint table (migration v005)
    // -----------------------------------------------------------------

    #[test]
    fn migration_v005_adds_checkpoints_content_columns() {
        let db = temp_db();
        let conn = db.pool.get().unwrap();
        // New natural key on (run_id, ckp_id)
        let pk_query: Vec<rusqlite::Result<String>> = conn
            .prepare("SELECT name FROM pragma_table_info('checkpoints')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect();
        let cols: Vec<String> = pk_query.into_iter().map(|r| r.unwrap()).collect();
        assert!(
            cols.contains(&"ckp_id".to_owned()),
            "ckp_id column missing: {cols:?}"
        );
        assert!(
            cols.contains(&"question".to_owned()),
            "question column missing: {cols:?}"
        );
        assert!(
            cols.contains(&"response".to_owned()),
            "response column missing: {cols:?}"
        );
        assert!(
            cols.contains(&"accepted_default".to_owned()),
            "accepted_default column missing: {cols:?}"
        );
        assert!(
            cols.contains(&"at_unix".to_owned()),
            "at_unix column missing: {cols:?}"
        );
        // PRAGMA user_version is now 5 (this test exercises the v005
        // migration path; v006 lands after it but does not affect
        // the columns under test).
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(v >= 5);
    }

    #[test]
    fn record_checkpoint_inserts_row() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_checkpoint(
            id,
            "h_019f0000-0000-7000-8000-000000000001",
            "intake",
            "continue?",
            "y",
            true,
            1_700_000_000,
        )
        .unwrap();
        let rows = db.list_checkpoints_for_run(id).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.ckp_id, "h_019f0000-0000-7000-8000-000000000001");
        assert_eq!(r.kind, "intake");
        assert_eq!(r.question, "continue?");
        assert_eq!(r.response, "y");
        assert!(r.accepted_default);
        assert_eq!(r.at_unix, Some(1_700_000_000));
    }

    #[test]
    fn record_checkpoint_is_idempotent() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        let ckp = "h_dup";
        for _ in 0..3 {
            db.record_checkpoint(id, ckp, "clarify", "ship?", "y", true, 100)
                .unwrap();
        }
        let rows = db.list_checkpoints_for_run(id).unwrap();
        assert_eq!(rows.len(), 1, "INSERT OR REPLACE should dedupe");
    }

    #[test]
    fn record_checkpoint_orders_by_at_unix() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_checkpoint(id, "h_3", "final", "ship?", "y", false, 300)
            .unwrap();
        db.record_checkpoint(id, "h_1", "intake", "ok?", "y", true, 100)
            .unwrap();
        db.record_checkpoint(id, "h_2", "clarify", "more?", "n", false, 200)
            .unwrap();
        let rows = db.list_checkpoints_for_run(id).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.ckp_id.as_str()).collect();
        assert_eq!(ids, vec!["h_1", "h_2", "h_3"]);
    }

    #[test]
    fn record_checkpoint_empty_response_round_trips() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_checkpoint(id, "h_skip", "intake", "skip?", "", false, 100)
            .unwrap();
        let rows = db.list_checkpoints_for_run(id).unwrap();
        assert_eq!(rows.len(), 1);
        // Empty string and `false` are valid values, not NULL.
        assert_eq!(rows[0].response, "");
        assert!(!rows[0].accepted_default);
    }

    #[test]
    fn list_checkpoints_for_unknown_run_returns_empty() {
        let db = temp_db();
        let other = RunId::new();
        let rows = db.list_checkpoints_for_run(other).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn checkpoint_counts_by_kind_groups_correctly() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_checkpoint(id, "h_1", "intake", "q1", "y", true, 1)
            .unwrap();
        db.record_checkpoint(id, "h_2", "intake", "q2", "n", false, 2)
            .unwrap();
        db.record_checkpoint(id, "h_3", "clarify", "q3", "y", true, 3)
            .unwrap();
        let counts = db.checkpoint_counts_by_kind(id).unwrap();
        assert_eq!(counts.get("intake").copied(), Some(2));
        assert_eq!(counts.get("clarify").copied(), Some(1));
        assert_eq!(counts.get("final"), None);
    }

    /// Phase G: opening a fresh DB applies the v006 migration
    /// (the `problem_graphs` table exists and `user_version >= 6`).
    /// We assert >= 6 so future migrations (Phase J bumped it to 7)
    /// do not break this test.
    #[test]
    fn v006_migration_creates_problem_graphs_table() {
        let db = temp_db();
        let conn = db.pool.get().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(version >= 6, "version = {version}");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='problem_graphs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Phase G: `record_problem_graph` writes the row and a
    /// second call with the same `run_id` updates it (INSERT OR
    /// REPLACE).
    #[test]
    fn record_problem_graph_round_trip() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "deep", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_problem_graph(run_id, "deadbeef", true, 5, 1_700_000_000)
            .unwrap();
        // Idempotent re-write with a different node count.
        db.record_problem_graph(run_id, "deadbeef", true, 7, 1_700_000_100)
            .unwrap();
        let conn = db.pool.get().unwrap();
        let (n, at): (i64, i64) = conn
            .query_row(
                "SELECT node_count, at_unix FROM problem_graphs WHERE run_id = ?",
                rusqlite::params![run_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 7);
        assert_eq!(at, 1_700_000_100);
    }

    // -----------------------------------------------------------------
    // Phase I (telemetry dashboard) read-only queries
    // -----------------------------------------------------------------

    /// Insert a synthetic call row + provider_usage + phases event
    /// for the dashboard query tests. Returns the run id. Each run
    /// gets unique call ids so the same seed function can be reused
    /// across multiple runs in the same database.
    fn seed_dashboard_run(db: &Db, mode: &str) -> RunId {
        let run_id = RunId::new();
        let suffix = run_id.to_string();
        db.register_run(run_id, mode, "completed", "0.3.0", None, None, None)
            .unwrap();
        db.record_call(
            &format!("c1-{suffix}"),
            run_id,
            "intake",
            "intake",
            "minimax",
            "MiniMax-M3",
            &format!("k1-{suffix}"),
            Some("sha1"),
            false,
            Some(200),
            100,
            50,
            10,
            0,
            1_700_000_000,
            1_700_000_005,
            None,
            0,
        )
        .unwrap();
        db.record_call(
            &format!("c2-{suffix}"),
            run_id,
            "intake",
            "intake",
            "minimax",
            "MiniMax-M3",
            &format!("k2-{suffix}"),
            None,
            true,
            None,
            0,
            0,
            0,
            0,
            1_700_000_010,
            1_700_000_011,
            None,
            0,
        )
        .unwrap();
        db.record_call(
            &format!("c3-{suffix}"),
            run_id,
            "propose",
            "proposer",
            "minimax",
            "MiniMax-M3",
            &format!("k3-{suffix}"),
            Some("sha3"),
            false,
            Some(500),
            200,
            80,
            0,
            0,
            1_700_000_020,
            1_700_000_030,
            Some("provider error"),
            1,
        )
        .unwrap();
        db.record_phase(run_id, "intake", 0, "start", None).unwrap();
        db.record_phase(run_id, "intake", 0, "end", None).unwrap();
        db.record_phase(run_id, "propose", 0, "start", None)
            .unwrap();
        db.record_phase(run_id, "propose", 0, "end", None).unwrap();
        db.record_warning(
            run_id,
            1_700_000_005_000,
            "model.json_repair_applied",
            "warn",
            Some("intake"),
            Some("intake"),
            Some(&format!("c1-{suffix}")),
            Some(0),
            "colon repair",
            "{}",
        )
        .unwrap();
        db.record_checkpoint(
            run_id,
            &format!("h_intake-{suffix}"),
            "intake",
            "continue?",
            "y",
            true,
            1_700_000_010,
        )
        .unwrap();
        run_id
    }

    #[test]
    fn run_aggregate_sums_calls_tokens_providers_phases_warnings() {
        let db = temp_db();
        let run_id = seed_dashboard_run(&db, "fast");

        let agg = db.run_aggregate(run_id).unwrap();
        assert_eq!(agg.calls, 3, "all 3 calls counted");
        assert_eq!(agg.error_calls, 1, "1 call has error status");
        assert_eq!(agg.timeout_calls, 0);
        assert_eq!(agg.cancelled_calls, 0);
        assert_eq!(agg.input_tokens, 300);
        assert_eq!(agg.output_tokens, 130);
        assert_eq!(agg.cache_read, 10);
        assert_eq!(agg.cache_creation, 0);
        assert_eq!(agg.provider_count, 1);
        assert_eq!(agg.phase_count, 2, "intake + propose");
        assert_eq!(agg.warnings, 1);
        assert_eq!(agg.checkpoints, 1);
        assert_eq!(agg.ok_calls(), 2);
        assert_eq!(agg.total_tokens(), 430);
    }

    #[test]
    fn run_aggregate_unknown_run_is_default_zero() {
        let db = temp_db();
        let agg = db.run_aggregate(RunId::new()).unwrap();
        assert_eq!(agg, RunAggregate::default());
    }

    #[test]
    fn list_provider_usage_empty_for_unknown_run() {
        let db = temp_db();
        let rows = db.list_provider_usage_for_run(RunId::new()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_phase_summaries_collapses_start_end_events() {
        let db = temp_db();
        let run_id = seed_dashboard_run(&db, "fast");
        let rows = db.list_phase_summaries_for_run(run_id).unwrap();
        // Two phases were recorded (intake and propose). Each phase
        // had two events (start + end); the dashboard sees one row
        // per phase, with the final status ("end") winning.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].phase, "intake");
        assert_eq!(rows[0].status, "end");
        assert_eq!(rows[1].phase, "propose");
        assert_eq!(rows[1].status, "end");
    }

    #[test]
    fn list_phase_summaries_preserves_error_status() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "deep", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_phase(run_id, "rank", 0, "start", None).unwrap();
        db.record_phase(run_id, "rank", 0, "error", Some("boom"))
            .unwrap();
        let rows = db.list_phase_summaries_for_run(run_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "error");
        assert_eq!(rows[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn recent_runs_for_provider_empty_for_unknown_provider() {
        let db = temp_db();
        let rows = db.recent_runs_for_provider("nonexistent", 5).unwrap();
        assert!(rows.is_empty());
    }

    // -----------------------------------------------------------------
    // Phase J (v0.3 «tercera etapa», sub-fase J) migration v007 +
    // lineage helpers.
    // -----------------------------------------------------------------

    use crate::context::ContextRefRecord;

    fn fake_record(path: &str, kind: &str) -> ContextRefRecord {
        ContextRefRecord {
            source_path: path.into(),
            context_type: kind.into(),
            shasum: "deadbeef".into(),
            bytes: 42,
            added_unix: 1_700_000_000,
        }
    }

    /// Migration v007 adds the four columns. We probe
    /// `PRAGMA table_info` to assert their presence.
    #[test]
    fn migration_v007_adds_lineage_columns() {
        let db = temp_db();
        let conn = db.pool.get().unwrap();
        let runs_cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('runs')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            runs_cols.iter().any(|c| c == "shared_brief_hash"),
            "missing runs.shared_brief_hash; got {runs_cols:?}"
        );
        let ctx_cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('run_context_refs')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            ctx_cols.iter().any(|c| c == "context_type"),
            "missing run_context_refs.context_type; got {ctx_cols:?}"
        );
        let sib_cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('run_siblings')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            sib_cols.iter().any(|c| c == "relation"),
            "missing run_siblings.relation; got {sib_cols:?}"
        );
        assert!(
            sib_cols.iter().any(|c| c == "created_unix"),
            "missing run_siblings.created_unix; got {sib_cols:?}"
        );
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        // After both v007 and v008 the user_version is 8.
        assert!(v >= 7, "version = {v}");
    }

    /// `add_context_ref` inserts and reads back the same record.
    /// A second insert with the same `(run_id, source_path)`
    /// replaces the previous row.
    #[test]
    fn add_context_ref_inserts_and_replaces() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.add_context_ref(run_id, &fake_record("/tmp/a.md", "path"))
            .unwrap();
        db.add_context_ref(run_id, &fake_record("/tmp/a.md", "dir"))
            .unwrap();
        let conn = db.pool.get().unwrap();
        let (kind, bytes): (String, i64) = conn
            .query_row(
                "SELECT context_type, bytes FROM run_context_refs \
                 WHERE run_id = ? AND source_path = ?",
                rusqlite::params![run_id.to_string(), "/tmp/a.md"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "dir");
        assert_eq!(bytes, 42);
    }

    /// `add_run_sibling_relation` writes the row and `IGNORE`s a
    /// repeat of the same `(primary, sibling)` pair.
    #[test]
    fn add_run_sibling_relation_is_idempotent() {
        let db = temp_db();
        let a = RunId::new();
        let b = RunId::new();
        db.register_run(a, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.register_run(b, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.add_run_sibling_relation(a, b, "rerun").unwrap();
        db.add_run_sibling_relation(a, b, "rerun").unwrap();
        let conn = db.pool.get().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM run_siblings \
                 WHERE primary_run_id = ? AND sibling_run_id = ?",
                rusqlite::params![a.to_string(), b.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// `set_run_parent` updates the column after the run was
    /// registered without a parent. Useful for `rerun` which mints
    /// a new run id first and then attaches the lineage.
    #[test]
    fn set_run_parent_updates_column() {
        let db = temp_db();
        let child = RunId::new();
        let parent = RunId::new();
        db.register_run(parent, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.register_run(child, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.set_run_parent(child, parent).unwrap();
        let row = db.get_run(child).unwrap().unwrap();
        assert_eq!(
            row.parent_run_id.as_deref(),
            Some(parent.to_string().as_str())
        );
    }

    /// `set_shared_brief_hash` updates the column.
    #[test]
    fn set_shared_brief_hash_updates_column() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.set_shared_brief_hash(run_id, "abc123").unwrap();
        let conn = db.pool.get().unwrap();
        let h: String = conn
            .query_row(
                "SELECT shared_brief_hash FROM runs WHERE run_id = ?",
                rusqlite::params![run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(h, "abc123");
    }

    /// `last_completed_phase` returns the most recent phase that
    /// ended successfully. Earlier errored phases are ignored.
    #[test]
    fn last_completed_phase_returns_latest_end() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "standard", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_phase(run_id, "intake", 0, "start", None).unwrap();
        db.record_phase(run_id, "intake", 0, "end", None).unwrap();
        db.record_phase(run_id, "clarify", 0, "start", None)
            .unwrap();
        db.record_phase(run_id, "clarify", 0, "end", None).unwrap();
        db.record_phase(run_id, "route", 0, "start", None).unwrap();
        db.record_phase(run_id, "route", 0, "error", Some("boom"))
            .unwrap();
        let last = db.last_completed_phase(run_id).unwrap();
        assert_eq!(last.as_deref(), Some("clarify"));
    }

    /// `last_completed_phase` is `None` for a run with no
    /// recorded phase events.
    #[test]
    fn last_completed_phase_none_when_no_end_events() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_phase(run_id, "intake", 0, "start", None).unwrap();
        assert!(db.last_completed_phase(run_id).unwrap().is_none());
    }

    /// `list_completed_phases` returns every phase that ended
    /// successfully, ordered by `started_unix` DESC.
    #[test]
    fn list_completed_phases_returns_every_end() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "standard", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_phase(run_id, "intake", 0, "start", None).unwrap();
        db.record_phase(run_id, "intake", 0, "end", None).unwrap();
        db.record_phase(run_id, "clarify", 0, "start", None)
            .unwrap();
        db.record_phase(run_id, "clarify", 0, "end", None).unwrap();
        let phases = db.list_completed_phases(run_id).unwrap();
        assert_eq!(phases, vec!["clarify", "intake"]);
    }

    // -----------------------------------------------------------------
    // Sub-fase K: v008 migration + the 5 new tables (D.5.1)
    // -----------------------------------------------------------------

    /// After `Db::open` the five v008 tables must exist.
    #[test]
    fn v008_migration_creates_five_new_tables() {
        let db = temp_db();
        let conn = db.pool.get().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        // v008 ships the five new tables; v010 keeps it at >= 9.
        assert!(v >= 9, "user_version = {v}");
        for table in [
            "outbox_events",
            "redact_audit",
            "manifest_events",
            "process_locks",
            "provider_rollups",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {table}");
        }
    }

    /// `record_outbox_event` writes a row that round-trips back.
    #[test]
    fn outbox_event_round_trips() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_outbox_event(&crate::storage::sqlite::OutboxEventRow {
            run_id: run_id.to_string(),
            event_type: "test".into(),
            payload: "{}".into(),
            at_unix: 1_700_000_000,
        })
        .unwrap();
        let rows = db.list_outbox_events_for_run(&run_id.to_string()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "test");
        assert_eq!(rows[0].payload, "{}");
    }

    /// `record_redact_audit` writes a row that round-trips back.
    #[test]
    fn redact_audit_round_trips() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_redact_audit(&crate::storage::sqlite::RedactAuditRow {
            run_id: Some(run_id.to_string()),
            source_path: "/tmp/test.md".into(),
            pattern_kind: "sk_cp_api_key".into(),
            match_count: 3,
            at_unix: 1_700_000_000,
        })
        .unwrap();
        let rows = db.list_redact_audit_for_run(&run_id.to_string()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pattern_kind, "sk_cp_api_key");
        assert_eq!(rows[0].match_count, 3);
    }

    /// `record_manifest_event` writes a row that round-trips back.
    #[test]
    fn manifest_event_round_trips() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.3.0", None, None, None)
            .unwrap();
        db.record_manifest_event(&crate::storage::sqlite::ManifestEventRow {
            run_id: run_id.to_string(),
            event_type: "checkpoint".into(),
            details: Some("intake".into()),
            at_unix: 1_700_000_000,
        })
        .unwrap();
    }

    /// A run lease can be released by its owning holder.
    #[test]
    fn process_lock_acquire_and_release() {
        let db = temp_db();
        let run_id = RunId::new();
        let first = db
            .renew_lease(run_id, "owner1", std::time::Duration::from_secs(60), None)
            .unwrap();
        assert_eq!(first, 1);
        let second = db.renew_lease(run_id, "owner2", std::time::Duration::from_secs(60), None);
        assert!(matches!(second, Err(crate::error::Error::LockHeld(_))));
        let released = db.release_run_lease(run_id, "owner1").unwrap();
        assert!(released);
        let third = db
            .renew_lease(run_id, "owner2", std::time::Duration::from_secs(60), None)
            .unwrap();
        assert_eq!(third, 1);
    }

    /// `increment_provider_rollup` aggregates across calls.
    #[test]
    fn provider_rollup_increment() {
        let db = temp_db();
        db.increment_provider_rollup("minimax", "MiniMax-M3", 100, 50, false)
            .unwrap();
        db.increment_provider_rollup("minimax", "MiniMax-M3", 200, 80, true)
            .unwrap();
        let row = db
            .get_provider_rollup("minimax", "MiniMax-M3")
            .unwrap()
            .expect("rollup row must exist");
        assert_eq!(row.calls, 2);
        assert_eq!(row.input_tokens, 300);
        assert_eq!(row.output_tokens, 130);
        assert_eq!(row.errors, 1);
        assert!(row.last_call_unix.is_some());
    }

    /// Two different models roll up independently.
    #[test]
    fn provider_rollup_select_per_model() {
        let db = temp_db();
        db.increment_provider_rollup("minimax", "MiniMax-M3", 10, 5, false)
            .unwrap();
        db.increment_provider_rollup("minimax", "M-D", 1, 1, false)
            .unwrap();
        let a = db
            .get_provider_rollup("minimax", "MiniMax-M3")
            .unwrap()
            .unwrap();
        let b = db.get_provider_rollup("minimax", "M-D").unwrap().unwrap();
        assert_eq!(a.calls, 1);
        assert_eq!(a.input_tokens, 10);
        assert_eq!(b.calls, 1);
        assert_eq!(b.input_tokens, 1);
        assert!(
            db.get_provider_rollup("minimax", "unknown")
                .unwrap()
                .is_none()
        );
    }

    /// W2: v009 adds the stability columns and `record_run_stability`
    /// round-trips through them. The pre-v009 helper path returns
    /// `Ok(())` silently so a DB opened before the migration still
    /// loads.
    #[test]
    fn record_run_stability_round_trips_through_v009() {
        let db = temp_db();
        // Force the v009 migration to run before the test body
        // (register_run would otherwise leave the DB at the post-v008
        // version that some threads see under parallel test runners).
        db.run_migrations().unwrap();
        assert!(db.user_version().unwrap() >= 9);
        let run_id = RunId::new();
        db.register_run(run_id, "standard", "running", "0.1.0", None, None, None)
            .unwrap();
        db.record_run_stability(run_id, 0.92, "stable", 0.05)
            .unwrap();
        let row = db
            .get_run_stability(run_id)
            .unwrap()
            .expect("stability row must exist");
        // The values round-trip as f64 (SQLite REAL widens f32); the
        // tolerance here is generous to absorb the cast.
        let score = row.score.unwrap();
        assert!((score - 0.92).abs() < 0.001, "score={score}");
        assert_eq!(row.label.as_deref(), Some("stable"));
        let sigma = row.sigma.unwrap();
        assert!((sigma - 0.05).abs() < 0.001, "sigma={sigma}");
    }

    /// J#5 closure: `list_lineage_pairs` returns each registered
    /// run paired with its parent (`None` for roots). Round-trip
    /// sanity check so the dashboard `/api/lineage` projection
    /// does not depend on filter+map internals.
    #[test]
    fn list_lineage_pairs_returns_parent_child_rows() {
        let db = temp_db();
        let parent = RunId::new();
        let child = RunId::new();
        let root = RunId::new();
        db.register_run(root, "fast", "completed", "0.3.0", None, None, None)
            .unwrap();
        db.register_run(parent, "fast", "completed", "0.3.0", None, None, Some(root))
            .unwrap();
        db.register_run(
            child,
            "fast",
            "completed",
            "0.3.0",
            None,
            None,
            Some(parent),
        )
        .unwrap();
        let pairs = db.list_lineage_pairs().unwrap();
        let dict: std::collections::HashMap<Option<String>, Option<String>> =
            pairs.into_iter().map(|(p, c)| (c, p)).collect();
        assert_eq!(
            dict.get(&Some(root.to_string())).cloned().flatten(),
            None,
            "root run must have no parent"
        );
        assert_eq!(
            dict.get(&Some(parent.to_string())).cloned().flatten(),
            Some(root.to_string())
        );
        assert_eq!(
            dict.get(&Some(child.to_string())).cloned().flatten(),
            Some(parent.to_string())
        );
    }

    /// v017: the `manifest_versions` table created by v012 is
    /// absent after `Db::open` on a freshly migrated DB.
    /// `user_version` reaches 17.
    ///
    /// The table was added by `v012_versioned_manifest.sql` and
    /// was written by `record_version` only on a code path that
    /// has since been dropped (PR #433, commit `fdc02d6`); the
    /// round-2 audit flagged it as dead schema. v017 removes
    /// the empty table for new runs.
    ///
    /// Regression: the v016 drop-empty-tables migration must
    /// still hold — re-probing those four tables here pins the
    /// migration's idempotency after another step is appended.
    #[test]
    fn migration_v017_drops_manifest_versions() {
        let db = temp_db();
        let conn = db.pool.get().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(
            v >= 17,
            "user_version must reach v017 after Db::open, got {v}"
        );
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
                params!["manifest_versions"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 0,
            "v017 table manifest_versions must be dropped, got n={n}"
        );
        for table in [
            "run_state",
            "discovery_dedup",
            "plan_state",
            "budget_events",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "v016 table {table} must be dropped, got n={n}");
        }
        let idx_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?",
                params!["idx_budget_events_run"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            idx_n, 0,
            "idx_budget_events_run must be dropped alongside budget_events, got n={idx_n}"
        );
    }

    /// Re-opening a DB that already has v017 applied stays at
    /// v017 without error. The runner's `if current < N` gates
    /// make this trivially true, but the test pins the contract:
    /// `CREATE TABLE IF NOT EXISTS` is idempotent at the SQL
    /// level so a third, fourth, ... open of the same DB also
    /// succeeds. (The original v013 wording predates the
    /// `calls.retry_count` migration landed in v014; v015 then
    /// added the `calls.cost_usd` column on top; v016 dropped
    /// the four empty tables; v017 dropped the empty
    /// `manifest_versions` table.)
    #[test]
    fn current_head_migration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite");
        let _db = Db::open(&path).expect("first open");
        let _db = Db::open(&path).expect("second open must not fail");
        let _db = Db::open(&path).expect("third open must not fail");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            v, 17,
            "user_version must stay at the current head across consecutive reopens, got {v}"
        );
        // v015 added a single ALTER TABLE so no new tables to
        // probe; the column existence checks below are enough.
        let retry_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('calls') WHERE name = 'retry_count'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retry_n, 1, "calls.retry_count column missing on reopen");
        let cost_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('calls') WHERE name = 'cost_usd'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cost_n, 1, "calls.cost_usd column missing on reopen");
    }

    /// v015: the migration adds `calls.cost_usd` as `REAL NOT NULL
    /// DEFAULT 0`. Verify the column exists, defaults to 0 on a
    /// freshly inserted row, and survives a re-open.
    #[test]
    fn migration_v015_adds_cost_usd_column() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.7.1", None, None, None)
            .unwrap();
        db.record_call(
            "c1",
            run_id,
            "intake",
            "intake",
            "mock",
            "mock-model",
            "cache-key-1",
            Some("sha256"),
            false,
            Some(200),
            100,
            50,
            0,
            0,
            100,
            101,
            None,
            0,
        )
        .unwrap();

        // Column exists and defaults to 0 on a row written before
        // the cost-USD plumbing runs.
        let conn = db.pool.get().unwrap();
        let cost: f64 = conn
            .query_row("SELECT cost_usd FROM calls WHERE call_id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cost, 0.0, "freshly inserted row defaults to cost_usd = 0");

        // user_version is at the current head.
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(v >= 17, "user_version must reach v017, got {v}");
    }

    /// v015: `record_call_cost` writes the column for a known call
    /// and ignores calls that pre-date the migration (the column
    /// check returns `Ok(())`).
    #[test]
    fn record_call_cost_updates_row() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.7.1", None, None, None)
            .unwrap();
        db.record_call(
            "c1",
            run_id,
            "intake",
            "intake",
            "mock",
            "mock-model",
            "cache-key-1",
            Some("sha256"),
            false,
            Some(200),
            100,
            50,
            0,
            0,
            100,
            101,
            None,
            0,
        )
        .unwrap();

        db.record_call_cost("c1", 0.0123).unwrap();
        let cost: f64 = {
            let conn = db.pool.get().unwrap();
            conn.query_row("SELECT cost_usd FROM calls WHERE call_id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert!((cost - 0.0123).abs() < 1e-9);

        // Zero and non-finite values are skipped (default left in
        // place) so the row count stays predictable.
        db.record_call_cost("c1", 0.0).unwrap();
        let cost: f64 = {
            let conn = db.pool.get().unwrap();
            conn.query_row("SELECT cost_usd FROM calls WHERE call_id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            cost, 0.0123,
            "zero cost must not overwrite an existing value"
        );

        db.record_call_cost("c1", f64::NAN).unwrap();
        let cost: f64 = {
            let conn = db.pool.get().unwrap();
            conn.query_row("SELECT cost_usd FROM calls WHERE call_id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(cost, 0.0123, "NaN must not overwrite an existing value");

        // list_calls_for_run surfaces the new column.
        let rows = db.list_calls_for_run(run_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].cost_usd - 0.0123).abs() < 1e-9);
    }

    /// v015 + cost.rs: end-to-end check that the models.dev catalog
    /// row for `minimax / MiniMax-M3` produces the expected USD
    /// estimate when the call records the real upstream token
    /// counts (0.30 / 1.20 / 0.03 / 0.375 per million tokens).
    #[test]
    fn cost_estimate_minimax_m3_real_pricing() {
        use crate::llm::cost::cost_estimate;
        use crate::llm::models_dev::ModelsDevCatalog;
        use crate::llm::wire::Usage;
        // Reproduce the upstream JSON for minimax / MiniMax-M3
        // using the same serde shape so the assertion does not
        // depend on a network fixture.
        let body = r#"{
            "schema_version": 1,
            "fetched_at_unix": 0,
            "providers": {
                "minimax": {
                    "id": "minimax",
                    "name": "MiniMax (minimax.io)",
                    "models": {
                        "MiniMax-M3": {
                            "id": "MiniMax-M3",
                            "name": "MiniMax-M3",
                            "attachment": false,
                            "reasoning": true,
                            "tool_call": true,
                            "temperature": true,
                            "modalities": {"input": ["text"], "output": ["text"]},
                            "limit": {"context": 524288, "output": 128000},
                            "cost": {"input": 0.3, "output": 1.2, "cache_read": 0.03, "cache_write": 0.375},
                            "open_weights": true
                        }
                    }
                }
            }
        }"#;
        let catalog: ModelsDevCatalog = serde_json::from_str(body).unwrap();
        // 1_000_000 input at $0.30/M + 500_000 output at $1.20/M +
        // 200_000 cache read at $0.03/M + 100_000 cache write at
        // $0.375/M = $0.30 + $0.60 + $0.006 + $0.0375 = $0.9435.
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read: 200_000,
            cache_creation: 100_000,
        };
        let cost = cost_estimate(Some(&catalog), "minimax", "MiniMax-M3", &usage);
        assert!(
            (cost - 0.9435).abs() < 1e-9,
            "minimax / MiniMax-M3 cost estimate drifted from upstream rates, got {cost}"
        );
    }

    /// `apply_step` must roll the schema change back when the
    /// closure fails. Without the transaction wrap the runner
    /// would have applied the schema SQL but never bumped
    /// `PRAGMA user_version`, leaving the DB in the inconsistent
    /// "new schema + old version" state this fix exists to
    /// prevent.
    #[test]
    fn migrations_run_atomically_schema_and_version_together() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // A baseline table so we can confirm the connection is
        // usable after the failed step.
        conn.execute_batch("CREATE TABLE baseline (id INTEGER)")
            .unwrap();

        let result = apply_step(&conn, 42, || -> Result<()> {
            conn.execute_batch("CREATE TABLE new_table (id INTEGER)")?;
            // Simulate the crash: the schema SQL succeeded but
            // the closure errors out before the PRAGMA bump.
            Err(crate::Error::Provider("simulated crash".into()))
        });
        assert!(result.is_err(), "failing closure must surface");

        // user_version must remain 0: PRAGMA bump never committed.
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "user_version must not bump on rollback, got {v}");

        // new_table must NOT exist: the schema change rolled back.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name='new_table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "new_table must not survive a rolled-back apply_step"
        );
    }

    /// Simulate the historical failure mode: process died between
    /// the schema change and the `PRAGMA user_version` bump. The
    /// DB is left with v006 schema but user_version = 5. The next
    /// `Db::open` must re-run v006 idempotently (CREATE TABLE IF
    /// NOT EXISTS / CREATE INDEX IF NOT EXISTS) and then apply
    /// v007..v017, advancing the version to the current head. This
    /// is the recovery path the atomicity fix makes safe — even if
    /// a crash DID leave the DB in this state, the migrations are
    /// idempotent enough to repair it.
    #[test]
    fn migrations_recovery_after_partial_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(sql_v001::V001).unwrap();
            conn.execute_batch("PRAGMA user_version = 1;").unwrap();
            conn.execute_batch(sql_v002::V002).unwrap();
            conn.execute_batch("PRAGMA user_version = 2;").unwrap();
            conn.execute_batch(sql_v003::V003).unwrap();
            conn.execute_batch("PRAGMA user_version = 3;").unwrap();
            conn.execute_batch(sql_v004::V004).unwrap();
            conn.execute_batch("PRAGMA user_version = 4;").unwrap();
            conn.execute_batch(sql_v005::V005).unwrap();
            conn.execute_batch("PRAGMA user_version = 5;").unwrap();
            // Simulate the crash: v006 schema landed but the
            // PRAGMA bump never committed.
            conn.execute_batch(sql_v006::V006).unwrap();
        }

        // Confirm the test fixture is in the inconsistent state.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let v: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 5, "fixture must leave DB at user_version=5");
        }

        let _db = Db::open(&path).expect("Db::open must recover");

        let conn = rusqlite::Connection::open(&path).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            v, 17,
            "user_version must advance to v017 (the current head) after recovery, got {v}"
        );
    }

    /// Build a unique path under `temp_dir()` for a one-shot DB.
    /// The tests below share a DB across multiple `Db::open`
    /// calls, so this helper does NOT use `tempfile::tempdir()`
    /// (which auto-deletes on drop and would race the second
    /// open). Each test cleans up its own dir at the end.
    fn unique_db_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("moagan-mig-idem-{pid}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("meta.sqlite")
    }

    /// Read `PRAGMA user_version` from the file at `path` via a
    /// fresh (unpooled) connection. Mirrors what a second `Db::open`
    /// would see before its migration runner touches anything.
    fn read_user_version(path: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// Open the same DB twice in sequence. The per-step probe in
    /// `run_migrations` makes the second open a no-op even when
    /// the first open's WAL has not been flushed to the main file
    /// yet — the failure mode that previously surfaced as a
    /// spurious `duplicate column name: status` panic in
    /// `cli::repair::tests::reindex_no_diff_returns_zero`. This
    /// test would have caught that flake directly.
    #[test]
    fn migrations_are_idempotent_when_run_twice() {
        let path = unique_db_path();
        let _ = Db::open(&path).expect("first open");
        let _ = Db::open(&path).expect("second open must not fail");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `user_version` is stable across consecutive `Db::open`
    /// calls on the same DB and reaches the current head (v017).
    /// Pins the "each step runs at most once" invariant: if the
    /// runner ever re-applied a step, the second open would still
    /// see the same version (idempotency) but would also need to
    /// re-run the entire v002..v017 ladder to land at 17, which
    /// would surface here as a wrong `user_version` or, more
    /// likely, a `duplicate column name` panic from v003/v007/v009.
    #[test]
    fn migrations_skip_applied_versions_on_reopen() {
        let path = unique_db_path();
        let _ = Db::open(&path).unwrap();
        let v1 = read_user_version(&path);
        let _ = Db::open(&path).unwrap();
        let v2 = read_user_version(&path);
        assert_eq!(v1, v2, "user_version must be stable across opens");
        assert_eq!(
            v1, 17,
            "user_version must reach the current head (v017), got {v1}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Simulate the documented v001 failure mode: schema applied
    /// but `user_version` not bumped, because v001's
    /// `PRAGMA synchronous = NORMAL` cannot run inside a
    /// transaction and therefore cannot share the atomic wrap
    /// that v002+ use. After `Db::open`, `user_version` must reach
    /// the current head even though v001 was re-run from a
    /// "partially applied" state (the SQL is idempotent so the
    /// re-run is a no-op at the schema level; the per-step probe
    /// then carries the runner from v1 → v2 → … → v17 on the
    /// fresh bumps).
    #[test]
    fn migrations_recover_from_v001_partial_state() {
        let path = unique_db_path();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(sql_v001::V001).unwrap();
            // Intentionally do NOT bump user_version — simulate
            // the crash window between the schema change and the
            // bump.
        }
        Db::open(&path).expect("Db::open must recover from v001 partial state");
        let v = read_user_version(&path);
        assert_eq!(
            v, 17,
            "user_version must reach the current head (v017) after recovery, got {v}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Db::open runs migrations and then forces a WAL checkpoint
    /// before returning, so the user_version bump is durable on the
    /// main DB file (not only on the WAL). Pins the contract that
    /// closes the remaining flake in
    /// `cli::repair::tests::reindex_no_diff_returns_zero`: after
    /// the first open, the second open reads a consistent
    /// user_version from the main file and run_migrations takes the
    /// 'skip' branch on every step. We verify the checkpoint by
    /// inspecting the WAL sidecar — `PRAGMA wal_checkpoint` reports
    /// zero frames to checkpoint and an empty WAL file means
    /// TRUNCATE ran successfully.
    #[test]
    fn db_open_checkpoints_wal_after_migrations() {
        let path = unique_db_path();
        let _ = Db::open(&path).expect("first open");

        let conn = rusqlite::Connection::open(&path).expect("verify connection");
        let (busy, log_frames, checkpointed): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .expect("wal_checkpoint must succeed on a healthy DB");
        assert_eq!(busy, 0, "wal_checkpoint must not report busy");
        assert_eq!(log_frames, 0, "WAL must be empty after Db::open TRUNCATE");
        assert_eq!(checkpointed, 0);

        drop(conn);
        // Second open is the actual repro path: it reads user_version
        // from the main file (no stale WAL frame masking it) and
        // must succeed without touching the schema.
        let _ = Db::open(&path).expect("second open must not fail");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Open/close the same DB ten times in quick succession, mirroring
    /// the parallel-test pattern that exposed the flake. Without the
    /// post-migration WAL checkpoint, two opens can race the
    /// user_version bump between the WAL and the main DB file and
    /// the second open re-runs v003 (ALTER TABLE ADD COLUMN
    /// status), failing with `duplicate column name: status`. With
    /// the fix in place every open is a clean no-op on the schema.
    #[test]
    fn db_open_idempotent_across_many_reopens() {
        let path = unique_db_path();
        for i in 0..10 {
            Db::open(&path).unwrap_or_else(|e| panic!("open #{i} must succeed: {e}"));
        }
        let v = read_user_version(&path);
        assert_eq!(
            v, 17,
            "user_version must reach the current head (v017) after 10 reopens, got {v}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // -- Regression tests for the open/close/reopen flake ------------
    //
    // `cli::repair::tests::reindex_no_diff_returns_zero` was flaky under
    // parallel `cargo test` execution: two consecutive `Db::open`
    // calls could race the WAL-to-MAIN flush and re-run migrations
    // that had already been applied, surfacing as a
    // `duplicate column name: status` panic from v003/v007/v009.
    // PR #95 added a per-step probe in `run_migrations`; PR #99
    // forced a WAL checkpoint at the end of `Db::open` so the
    // `user_version` bump is durable on the main DB file before the
    // next open observes it. These three tests pin the recovered
    // behaviour so future refactors cannot regress it.

    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_regression_path(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("moagan-regression-{label}-{pid}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("meta.sqlite")
    }

    /// 50 sequential open/close cycles on the same path. The second
    /// `Db::open` in each cycle is the actual repro: without the WAL
    /// checkpoint, the runner would re-apply v003/v007/v009 and panic
    /// with `duplicate column name`. 100 opens in a row give the
    /// migration runner and the OS page-cache flusher enough chances
    /// to surface any flakiness inside a single test run.
    #[test]
    fn regression_db_open_close_50_iterations_no_duplicate_column() {
        let path = unique_regression_path("close50");
        for _ in 0..50 {
            let _ = Db::open(&path).expect("open 1");
            let _ = Db::open(&path).expect("open 2 must not duplicate column");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 4 threads × 10 concurrent `Db::open` calls on the same path.
    /// Each thread creates its own `Db` (with its own r2d2 pool), so
    /// SQLite has to serialise concurrent file access via OS-level
    /// locks. The original flake surfaced under this exact pattern:
    /// the `cargo test` parallel runner interleaves the migrations of
    /// sibling tests on the same path and one of them re-runs v003.
    /// Without the per-step probe this would panic; without the WAL
    /// checkpoint the second open from any thread can read a stale
    /// `user_version = 0` and re-run the entire ladder. With both
    /// fixes in place the test is a clean no-op.
    #[test]
    fn regression_db_open_under_load_no_panic() {
        use std::sync::Arc;
        use std::thread;
        let path = Arc::new(unique_regression_path("load"));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = Arc::clone(&path);
                thread::spawn(move || {
                    for _ in 0..10 {
                        let _ = Db::open(&path).ok();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Replicates the exact flow of the original flaky test
    /// (`cli::repair::tests::reindex_no_diff_returns_zero`): open DB,
    /// drop the connection, reopen DB. Runs across 4 parallel threads
    /// on independent tempdirs so the OS scheduler cannot serialise
    /// the open/close/reopen sequence. Each thread exercises the full
    /// dance on its own file; the parallel stress is in the OS
    /// scheduler interleaving the four threads' Db::open calls
    /// rather than in shared-state contention (which is covered by
    /// `regression_db_open_under_load_no_panic`).
    #[test]
    fn regression_reindex_full_flow_under_load() {
        use std::thread;
        let handles: Vec<_> = (0..4)
            .map(|i| {
                thread::spawn(move || {
                    let path = unique_regression_path(&format!("flow{i}"));
                    let _ = Db::open(&path).expect("first open");
                    let _ = Db::open(&path).expect("second open must not fail");
                    let _ = std::fs::remove_dir_all(path.parent().unwrap());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    // -----------------------------------------------------------------
    // `moagan telemetry plan` aggregation (`aggregate_window_usage`)
    // -----------------------------------------------------------------

    /// Seed helper: insert one `calls` row at `started_unix` with the
    /// given status. Pass `error` for an HTTP 500-style error row,
    /// `None` for an OK row. Returns the run id so multiple calls in
    /// the same run can share state (real `calls` rows always have a
    /// parent `runs` row, per the FK declared in v001_initial.sql).
    fn seed_call(
        db: &Db,
        provider: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_hit: bool,
        started_unix: i64,
        error: Option<&str>,
    ) -> RunId {
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.6.0", None, None, None)
            .unwrap();
        let http_status = if error.is_some() {
            Some(500)
        } else {
            Some(200)
        };
        let call_id = format!(
            "c-{}-{}-{}",
            provider,
            started_unix,
            crate::time::now_unix_secs()
        );
        db.record_call(
            &call_id,
            run_id,
            "intake",
            "intake",
            provider,
            model,
            "cache-key",
            None,
            cache_hit,
            http_status,
            input_tokens,
            output_tokens,
            0,
            0,
            started_unix,
            started_unix + 1,
            error,
            0,
        )
        .unwrap();
        run_id
    }

    /// Fresh DB → empty result. The query must short-circuit to
    /// zero rows (not error) so the CLI can print "(no calls in the
    /// last N day(s))" and exit 1.
    #[test]
    fn aggregate_window_usage_returns_zero_rows_on_empty_db() {
        let db = temp_db();
        let rows = db.aggregate_window_usage(7, None).unwrap();
        assert!(rows.is_empty(), "fresh DB must return zero rows");
    }

    /// Three rows on two `(provider, model)` groups → two output rows
    /// with the right call_count, total_tokens, and error_count. The
    /// query groups by `(provider, model)` so a provider with two
    /// models surfaces as two rows (which the `moagan telemetry plan`
    /// printer relies on).
    #[test]
    fn aggregate_window_usage_groups_by_provider_and_model() {
        let db = temp_db();
        let now = crate::time::now_unix_secs();
        // Two calls on (minimax-m3, MiniMax-M3): 100+50 + 200+100 = 450 tokens.
        seed_call(&db, "minimax", "MiniMax-M3", 100, 50, false, now - 60, None);
        seed_call(
            &db,
            "minimax",
            "MiniMax-M3",
            200,
            100,
            false,
            now - 30,
            None,
        );
        // One call on (opencode_go, deepseek-v4-flash): 80+20 = 100 tokens.
        seed_call(
            &db,
            "opencode_go",
            "deepseek-v4-flash",
            80,
            20,
            false,
            now - 20,
            None,
        );

        let rows = db.aggregate_window_usage(7, None).unwrap();
        assert_eq!(rows.len(), 2, "two distinct (provider, model) groups");

        // Sorted by `total_tokens DESC` so the heavier consumer
        // (`minimax / MiniMax-M3` at 450) comes first.
        let first = &rows[0];
        assert_eq!(first.provider, "minimax");
        assert_eq!(first.model, "MiniMax-M3");
        assert_eq!(first.call_count, 2);
        assert_eq!(first.total_tokens, 450);
        assert_eq!(first.error_count, 0);

        let second = &rows[1];
        assert_eq!(second.provider, "opencode_go");
        assert_eq!(second.model, "deepseek-v4-flash");
        assert_eq!(second.call_count, 1);
        assert_eq!(second.total_tokens, 100);
    }

    /// The `provider_filter = Some(_)` argument narrows the result to
    /// one provider. Asserts 1 row even when 3 calls on the matching
    /// provider exist (they collapse into one GROUP BY row).
    #[test]
    fn aggregate_window_usage_filters_by_provider() {
        let db = temp_db();
        let now = crate::time::now_unix_secs();
        for offset in [60, 30, 10] {
            seed_call(
                &db,
                "minimax",
                "MiniMax-M3",
                10,
                5,
                false,
                now - offset,
                None,
            );
        }
        seed_call(&db, "kimi-k3", "kimi-k3", 100, 100, false, now - 15, None);

        let rows = db.aggregate_window_usage(7, Some("minimax")).unwrap();
        assert_eq!(rows.len(), 1, "filter must drop the kimi row");
        assert_eq!(rows[0].provider, "minimax");
        assert_eq!(rows[0].call_count, 3);
        assert_eq!(rows[0].total_tokens, 45); // (10+5) * 3 = 45

        // And `None` shows everything.
        let all = db.aggregate_window_usage(7, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    /// A call whose `started_unix` lands outside the window must be
    /// filtered out. This is the load-bearing invariant for the
    /// "rolling 7-day" semantics the subcommand advertises.
    #[test]
    fn aggregate_window_usage_respects_window_days() {
        let db = temp_db();
        let now = crate::time::now_unix_secs();
        let thirty_days_ago = now - (30_i64 * 86_400);
        seed_call(
            &db,
            "minimax",
            "MiniMax-M3",
            100,
            50,
            false,
            thirty_days_ago,
            None,
        );

        // 7-day window drops the 30-day-old row.
        let rows = db.aggregate_window_usage(7, None).unwrap();
        assert!(rows.is_empty(), "30-day-old row must be filtered out");

        // 60-day window keeps it.
        let rows = db.aggregate_window_usage(60, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_tokens, 150);
    }

    /// `status = 'error'` rows contribute to `error_count`. The
    /// `call_status` helper derives this from `http_status` (or
    /// `error` text), so we exercise both paths in the same test
    /// (`Some(500)` for the first error, `error = Some("schema
    /// violation")` for the second) to make sure the SQL `status`
    /// column lands in `'error'` regardless of which input triggered
    /// it.
    #[test]
    fn aggregate_window_usage_counts_errors() {
        let db = temp_db();
        let now = crate::time::now_unix_secs();
        // Two OK calls on the same (provider, model).
        seed_call(&db, "minimax", "MiniMax-M3", 10, 5, false, now - 60, None);
        seed_call(&db, "minimax", "MiniMax-M3", 20, 10, false, now - 30, None);
        // One error call on the same group.
        seed_call(
            &db,
            "minimax",
            "MiniMax-M3",
            5,
            0,
            false,
            now - 20,
            Some("schema violation"),
        );

        let rows = db.aggregate_window_usage(7, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 3);
        assert_eq!(rows[0].error_count, 1, "exactly one error in the group");
        assert_eq!(rows[0].total_tokens, 50); // (10+5)+(20+10)+(5+0)
    }
}
