-- Moagan meta-database v001 — initial schema.
-- Applied by SQLite at first run, before any other query.

PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

-- One row per run. The filesystem <MOAGAN_HOME>/.runs/<run_id>/ is
-- the canonical source of truth; this row is the index.
CREATE TABLE IF NOT EXISTS runs (
    run_id         TEXT PRIMARY KEY,
    mode           TEXT NOT NULL,
    status         TEXT NOT NULL,
    created_unix   INTEGER NOT NULL,
    updated_unix   INTEGER NOT NULL,
    schema_version TEXT NOT NULL,
    client_version TEXT NOT NULL,
    parent_run_id  TEXT,
    config_hash    TEXT,
    brief_hash     TEXT,
    FOREIGN KEY (parent_run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_runs_mode ON runs(mode);
CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);
CREATE INDEX IF NOT EXISTS idx_runs_created ON runs(created_unix);

-- Sibling / lineage graph for rerun and continue.
CREATE TABLE IF NOT EXISTS run_siblings (
    primary_run_id TEXT NOT NULL,
    sibling_run_id TEXT NOT NULL,
    relation       TEXT NOT NULL,           -- 'rerun', 'continue', 'import'
    created_unix   INTEGER NOT NULL,
    PRIMARY KEY (primary_run_id, sibling_run_id),
    FOREIGN KEY (primary_run_id) REFERENCES runs(run_id),
    FOREIGN KEY (sibling_run_id) REFERENCES runs(run_id)
);

-- External context references used by the run (file or directory).
CREATE TABLE IF NOT EXISTS run_context_refs (
    run_id      TEXT NOT NULL,
    source_path TEXT NOT NULL,
    shasum      TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    added_unix  INTEGER NOT NULL,
    PRIMARY KEY (run_id, source_path),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Every provider switch recorded on the timeline.
CREATE TABLE IF NOT EXISTS provider_changes (
    run_id      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    phase       TEXT NOT NULL,
    from_name   TEXT,
    to_name     TEXT NOT NULL,
    at_unix     INTEGER NOT NULL,
    reason      TEXT,
    PRIMARY KEY (run_id, seq),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Aggregated token usage per provider per run.
CREATE TABLE IF NOT EXISTS provider_usage (
    run_id           TEXT NOT NULL,
    provider         TEXT NOT NULL,
    model            TEXT NOT NULL,
    calls            INTEGER NOT NULL DEFAULT 0,
    input_tokens     INTEGER NOT NULL DEFAULT 0,
    output_tokens    INTEGER NOT NULL DEFAULT 0,
    cache_read       INTEGER NOT NULL DEFAULT 0,
    cache_creation   INTEGER NOT NULL DEFAULT 0,
    last_call_unix   INTEGER,
    PRIMARY KEY (run_id, provider, model),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Per-phase record.
CREATE TABLE IF NOT EXISTS phases (
    run_id        TEXT NOT NULL,
    phase         TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    status        TEXT NOT NULL,            -- 'start', 'end', 'error', 'cancel'
    started_unix  INTEGER,
    ended_unix    INTEGER,
    error         TEXT,
    PRIMARY KEY (run_id, phase, seq),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

-- Per-LLM-call record. Hash is the cache key (BLAKE3).
CREATE TABLE IF NOT EXISTS calls (
    call_id        TEXT PRIMARY KEY,
    run_id         TEXT NOT NULL,
    phase          TEXT NOT NULL,
    role           TEXT NOT NULL,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    cache_key      TEXT NOT NULL,
    cache_hit      INTEGER NOT NULL DEFAULT 0,
    http_status    INTEGER,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_read     INTEGER NOT NULL DEFAULT 0,
    cache_creation INTEGER NOT NULL DEFAULT 0,
    started_unix   INTEGER NOT NULL,
    ended_unix     INTEGER,
    error          TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_calls_run_phase ON calls(run_id, phase);
CREATE INDEX IF NOT EXISTS idx_calls_cache_key ON calls(cache_key);

-- Human checkpoint records.
CREATE TABLE IF NOT EXISTS checkpoints (
    run_id      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    resolved    INTEGER NOT NULL DEFAULT 0,
    note        TEXT,
    created_unix INTEGER NOT NULL,
    resolved_unix INTEGER,
    PRIMARY KEY (run_id, seq),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);
