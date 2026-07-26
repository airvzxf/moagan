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
use rusqlite::params;
use thiserror::Error;

use crate::error::Result;
use crate::ids::RunId;

mod sql_v001 {
    pub(super) const V001: &str = include_str!("migrations/v001_initial.sql");
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
        Ok(db)
    }

    /// Path to the underlying SQLite file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run pending migrations in order.
    pub fn run_migrations(&self) -> Result<()> {
        let conn = self.pool.get()?;
        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current < 1 {
            conn.execute_batch(sql_v001::V001)?;
            conn.execute_batch("PRAGMA user_version = 1;")?;
        }
        Ok(())
    }

    /// Register a new run. Returns the rowid (not used externally).
    pub fn register_run(
        &self,
        run_id: RunId,
        mode: &str,
        status: &str,
        client_version: &str,
        config_hash: Option<&str>,
        brief_hash: Option<&str>,
        parent: Option<RunId>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT OR REPLACE INTO runs \
             (run_id, mode, status, created_unix, updated_unix, schema_version, client_version, parent_run_id, config_hash, brief_hash) \
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
                brief_hash,
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
        cache_hit: bool,
        http_status: Option<i64>,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT INTO calls (call_id, run_id, phase, role, provider, model, cache_key, cache_hit, http_status, input_tokens, output_tokens, cache_read, cache_creation, started_unix, ended_unix, error) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                call_id,
                run_id.to_string(),
                phase,
                role,
                provider,
                model,
                cache_key,
                cache_hit as i64,
                http_status,
                input_tokens as i64,
                output_tokens as i64,
                cache_read as i64,
                cache_creation as i64,
                now,
                now,
                error,
            ],
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

    /// Upsert accumulated token usage for a provider+model.
    #[allow(clippy::too_many_arguments)]
    pub fn accumulate_usage(
        &self,
        run_id: RunId,
        provider: &str,
        model: &str,
        calls_delta: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = crate::time::now_unix_secs();
        conn.execute(
            "INSERT INTO provider_usage (run_id, provider, model, calls, input_tokens, output_tokens, cache_read, cache_creation, last_call_unix) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(run_id, provider, model) DO UPDATE SET \
                calls = calls + excluded.calls, \
                input_tokens = input_tokens + excluded.input_tokens, \
                output_tokens = output_tokens + excluded.output_tokens, \
                cache_read = cache_read + excluded.cache_read, \
                cache_creation = cache_creation + excluded.cache_creation, \
                last_call_unix = excluded.last_call_unix",
            params![
                run_id.to_string(),
                provider,
                model,
                calls_delta as i64,
                input_tokens as i64,
                output_tokens as i64,
                cache_read as i64,
                cache_creation as i64,
                now,
            ],
        )?;
        Ok(())
    }

    /// List runs ordered by creation time (descending).
    pub fn list_runs(&self, limit: u32) -> Result<Vec<RunRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, mode, status, created_unix, updated_unix, client_version, parent_run_id \
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Get a single run by id.
    pub fn get_run(&self, run_id: RunId) -> Result<Option<RunRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT run_id, mode, status, created_unix, updated_unix, client_version, parent_run_id \
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
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }
}

/// Row read from `runs`.
#[derive(Debug, Clone)]
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
        assert_eq!(v, 1);
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
            false,
            Some(200),
            0,
            0,
            0,
            0,
            None,
        )
        .unwrap();
        let runs = db.list_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
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
    fn accumulate_usage_is_additive() {
        let db = temp_db();
        let id = RunId::new();
        db.register_run(id, "fast", "running", "0.1.0", None, None, None)
            .unwrap();
        db.accumulate_usage(id, "minimax", "MiniMax-M3", 1, 100, 50, 0, 0)
            .unwrap();
        db.accumulate_usage(id, "minimax", "MiniMax-M3", 1, 200, 100, 0, 0)
            .unwrap();
        let conn = db.pool.get().unwrap();
        let (calls, inp, out): (i64, i64, i64) = conn
            .query_row(
                "SELECT calls, input_tokens, output_tokens FROM provider_usage WHERE run_id = ?",
                params![id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(inp, 300);
        assert_eq!(out, 150);
    }
}
