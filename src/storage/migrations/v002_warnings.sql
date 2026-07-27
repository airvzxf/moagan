-- Moagan meta-database v002 — warnings table.
-- Applied after v001. ADDITIVE only: no FK changes, no renames.
--
-- Mirrors `telemetry/warnings.jsonl` so the `moagan inspect <run_id>`
-- post-execution review can run a single SQL query instead of streaming
-- the JSONL. The JSONL stream remains the canonical timeline; SQLite
-- is a queryable index.

CREATE TABLE IF NOT EXISTS warnings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL,
    at_unix_ms  INTEGER NOT NULL,
    code        TEXT NOT NULL,
    level       TEXT NOT NULL,
    phase       TEXT,
    role        TEXT,
    call_id     TEXT,
    attempt     INTEGER,
    message     TEXT NOT NULL,
    details     TEXT NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_warnings_run_code ON warnings(run_id, code);
CREATE INDEX IF NOT EXISTS idx_warnings_run_at ON warnings(run_id, at_unix_ms);
