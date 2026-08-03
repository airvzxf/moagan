-- v008_add_ons.sql
-- Sub-fase K of v0.4: add five additive index tables that close the
-- read-only / write-only helpers exposed by D.5.1 of the
-- proposal-03 catalog. The tables are independent of the existing
-- schema (no ALTER on `runs`, `calls`, `phases`, etc.) so this
-- migration is forward-only and never touches legacy databases
-- in a destructive way.
--
-- All tables use `CREATE TABLE IF NOT EXISTS` so the migration
-- is idempotent on databases that have already moved past
-- user_version 7. The `PRAGMA user_version = 8` write happens in
-- src/storage/sqlite.rs::Db::run_migrations after the batch.
--
-- outbox_events (D.1.4 / D.5.1):
--   Transactional outbox. The phase code writes the event before
--   the sidecar lands; if the SQLite write fails the sidecar is
--   left untouched so the canonical filesystem-first contract
--   stays intact. The eventual consumer (D.1.4 worker) flushes
--   every 5s. Payload is the JSON-encoded event body.
--
-- redact_audit (D.8.5 / D.5.1):
--   Per-file pattern-kind counter so operators can detect leaks
--   in past runs without re-scanning the filesystem. `run_id`
--   is nullable so a pre-pipeline redaction pass (rare, but
--   supported) still has a place to land.
--
-- manifest_events (D.5.1):
--   Lifecycle events that the manifest.json sidecar would
--   otherwise bury: 'phase_started', 'phase_ended',
--   'provider_switched', 'cache_hit', etc. Keeping them in a
--   separate table keeps the manifest sidecar small.
--
-- process_locks (D.1.5 / D.5.1):
--   Single-row lock keyed by `holder`. The lease module
--   (src/storage/lease.rs in a future sub-phase) uses this
--   table to claim a run for the local process; `fence` is a
--   monotonic counter so a stale process detects its lease has
--   been stolen.
--
-- provider_rollups (D.5.1):
--   Global cross-run rollup keyed by (provider, model).
--   Distinct from `provider_usage` (which is per-run) — this
--   table is what the future `moagan telemetry provider` view
--   reads for "total spend per model" without re-aggregating.

CREATE TABLE IF NOT EXISTS outbox_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    payload     TEXT NOT NULL,
    at_unix     INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_outbox_events_run
    ON outbox_events(run_id, at_unix);
CREATE INDEX IF NOT EXISTS idx_outbox_events_type
    ON outbox_events(event_type, at_unix);

CREATE TABLE IF NOT EXISTS redact_audit (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       TEXT,
    source_path  TEXT NOT NULL,
    pattern_kind TEXT NOT NULL,
    match_count  INTEGER NOT NULL DEFAULT 1,
    at_unix      INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_redact_audit_run
    ON redact_audit(run_id, at_unix);
CREATE INDEX IF NOT EXISTS idx_redact_audit_kind
    ON redact_audit(pattern_kind, at_unix);

CREATE TABLE IF NOT EXISTS manifest_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    details     TEXT,
    at_unix     INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_manifest_events_run
    ON manifest_events(run_id, at_unix);
CREATE INDEX IF NOT EXISTS idx_manifest_events_type
    ON manifest_events(event_type, at_unix);

CREATE TABLE IF NOT EXISTS process_locks (
    holder             TEXT PRIMARY KEY,
    acquired_at_unix   INTEGER NOT NULL,
    expires_at_unix    INTEGER NOT NULL,
    fence              TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_process_locks_expires
    ON process_locks(expires_at_unix);

CREATE TABLE IF NOT EXISTS provider_rollups (
    provider        TEXT NOT NULL,
    model           TEXT NOT NULL,
    calls           INTEGER NOT NULL DEFAULT 0,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    errors          INTEGER NOT NULL DEFAULT 0,
    last_call_unix  INTEGER,
    PRIMARY KEY (provider, model)
);

CREATE INDEX IF NOT EXISTS idx_provider_rollups_last_call
    ON provider_rollups(last_call_unix);
