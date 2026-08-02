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

    /// Run pending migrations in order. v007 (Phase J lineage) is
    /// applied idempotently: the runner probes `PRAGMA table_info`
    /// before each `ALTER TABLE` so a re-opened DB that was already
    /// at v007 stays at v007 without an "duplicate column" error.
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
        if current < 5 {
            conn.execute_batch(sql_v005::V005)?;
            conn.execute_batch("PRAGMA user_version = 5;")?;
        }
        if current < 6 {
            conn.execute_batch(sql_v006::V006)?;
            conn.execute_batch("PRAGMA user_version = 6;")?;
        }
        if current < 7 {
            apply_v007_idempotent(&conn)?;
            conn.execute_batch("PRAGMA user_version = 7;")?;
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

    /// Read the `problem_graphs` row for a run. Returns `None`
    /// when the row does not exist (pre-v006 DB or a run that
    /// never reached the `decompose` phase). Best-effort: returns
    /// `Ok(None)` on the legacy schema too.
    pub fn get_problem_graph(&self, run_id: RunId) -> Result<Option<ProblemGraphRow>> {
        let conn = self.pool.get()?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 6 {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT brief_blake3, should_decompose, node_count, at_unix \
             FROM problem_graphs WHERE run_id = ?",
        )?;
        let row = stmt.query_row(params![run_id.to_string()], |r| {
            Ok(ProblemGraphRow {
                brief_blake3: r.get(0)?,
                should_decompose: r.get::<_, i64>(1)? != 0,
                node_count: r.get(2)?,
                at_unix: r.get(3)?,
            })
        });
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
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
    /// `started_unix`, the lexicographically smaller `phase` wins
    /// (deterministic across re-runs).
    pub fn last_completed_phase(&self, run_id: RunId) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT phase FROM phases \
             WHERE run_id = ? AND status = 'end' \
             ORDER BY started_unix DESC, phase ASC \
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
        // v007 added the lineage + context_refs columns.
        assert_eq!(v, 7);
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
        )
        .unwrap();
        db.accumulate_usage(run_id, "minimax", "MiniMax-M3", 3, 300, 130, 10, 0)
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
    fn list_provider_usage_orders_by_total_tokens_desc() {
        let db = temp_db();
        let run_id = seed_dashboard_run(&db, "fast");
        let suffix = run_id.to_string();

        // Add a second (provider, model) to confirm the ordering.
        db.record_call(
            &format!("c4-{suffix}"),
            run_id,
            "rank",
            "ranker",
            "mock",
            "mock-model",
            &format!("k4-{suffix}"),
            None,
            false,
            Some(200),
            1_000,
            500,
            0,
            0,
            1_700_000_100,
            1_700_000_101,
            None,
        )
        .unwrap();
        db.accumulate_usage(run_id, "mock", "mock-model", 1, 1_000, 500, 0, 0)
            .unwrap();

        let rows = db.list_provider_usage_for_run(run_id).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].provider, "mock", "mock has more total tokens");
        assert_eq!(rows[0].input_tokens, 1_000);
        assert_eq!(rows[0].output_tokens, 500);
        assert_eq!(rows[1].provider, "minimax");
        assert_eq!(rows[1].calls, 3);
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
    fn aggregate_provider_usage_groups_across_runs() {
        let db = temp_db();
        let a = seed_dashboard_run(&db, "fast");
        let b = seed_dashboard_run(&db, "standard");
        let rows = db.aggregate_provider_usage().unwrap();
        // Both runs use minimax / MiniMax-M3; the aggregate row sums
        // both, and the sort puts it at the top by total tokens.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "minimax");
        assert_eq!(rows[0].model, "MiniMax-M3");
        assert_eq!(rows[0].calls, 6, "3 + 3 calls aggregated");
        assert_eq!(rows[0].input_tokens, 600, "300 + 300");
        assert_eq!(rows[0].output_tokens, 260, "130 + 130");
        let _ = (a, b);
    }

    #[test]
    fn recent_runs_for_provider_orders_by_last_call_unix() {
        let db = temp_db();
        let run_id = seed_dashboard_run(&db, "fast");
        let rows = db.recent_runs_for_provider("minimax", 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "minimax");
        assert_eq!(rows[0].model, "MiniMax-M3");
        // `accumulate_usage` stamps `last_call_unix` at write time
        // (uses `crate::time::now_unix_secs()`), so we just assert
        // the field is populated. Ordering by `last_call_unix DESC`
        // is exercised by inserting two runs with different seeds.
        assert!(rows[0].last_call_unix.is_some());
        let _ = run_id;
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
        assert_eq!(v, 7);
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
}
