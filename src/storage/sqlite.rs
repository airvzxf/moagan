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

mod sql_v002 {
    pub(super) const V002: &str = include_str!("migrations/v002_warnings.sql");
}

mod sql_v003 {
    pub(super) const V003: &str = include_str!("migrations/v003_calls_status.sql");
}

mod sql_v004 {
    pub(super) const V004: &str = include_str!("migrations/v004_calls_body_sha256.sql");
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
        if current < 2 {
            conn.execute_batch(sql_v002::V002)?;
            conn.execute_batch("PRAGMA user_version = 2;")?;
        }
        if current < 3 {
            conn.execute_batch(sql_v003::V003)?;
            conn.execute_batch("PRAGMA user_version = 3;")?;
        }
        if current < 4 {
            conn.execute_batch(sql_v004::V004)?;
            conn.execute_batch("PRAGMA user_version = 4;")?;
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
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let http_status_u16 = http_status.and_then(|s| u16::try_from(s).ok());
        let status = call_status(http_status_u16, error);
        conn.execute(
            "INSERT INTO calls (call_id, run_id, phase, role, provider, model, cache_key, body_sha256, cache_hit, http_status, input_tokens, output_tokens, cache_read, cache_creation, started_unix, ended_unix, error, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                     ended_unix, error \
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
#[derive(Debug, Clone)]
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
        assert_eq!(v, 4);
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
        )
        .unwrap();
        let runs = db.list_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        let calls = db.list_calls_for_run(id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body_sha256.as_deref(), Some("sha256"));
        assert_eq!(calls[0].started_unix, 100);
        assert_eq!(calls[0].ended_unix, Some(101));
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
}
