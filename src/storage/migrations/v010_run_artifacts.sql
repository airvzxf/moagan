-- v010_run_artifacts.sql — Track D.2 (D.28.5): mirror the per-kind
-- artefact count from the filesystem (canonical) into SQLite so
-- `moagan repair --reindex-artifacts` can detect drift between the
-- two without scanning the filesystem twice.
--
-- The schema is a thin (run_id, kind) -> count cache. Each kind
-- matches a directory under the run root: `proposals/`, `sketches/`,
-- `evaluations/`, `critiques/`. The reindex command counts the
-- primary `*.json` files in each directory (excluding sidecars and
-- `*.tmp.*` atomic-write leftovers) and upserts the value.
--
-- The table is additive and forward-only: pre-v010 databases see no
-- columns added to existing tables, and the helpers in
-- `src/storage/sqlite.rs` short-circuit to `Ok(0)` on a pre-v010
-- `user_version` so a legacy operator never crashes the new code
-- path.

CREATE TABLE IF NOT EXISTS run_artifacts (
    run_id             TEXT NOT NULL,
    kind               TEXT NOT NULL,        -- 'proposals' | 'sketches' | 'evaluations' | 'critiques'
    count              INTEGER NOT NULL DEFAULT 0,
    last_indexed_unix  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, kind)
);

CREATE INDEX IF NOT EXISTS idx_run_artifacts_run
    ON run_artifacts(run_id);
