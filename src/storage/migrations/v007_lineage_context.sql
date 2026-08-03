-- v007_lineage_context.sql — Phase J (v0.3 «tercera etapa», sub-fase J):
-- extend `runs`, `run_context_refs`, and `run_siblings` to hold the
-- lineage + context-ref metadata that the new CLI subcommands
-- (`continue`, `resume`, `rerun`, `import`, `--context`) need.
--
-- The migration runner (`src/storage/sqlite.rs::run_migrations`)
-- applies each `ALTER TABLE` only when the target column is
-- absent. SQLite < 3.35 does not support `ADD COLUMN IF NOT
-- EXISTS`, so the runner probes `PRAGMA table_info(<table>)`
-- first. That keeps the migration idempotent across repeated
-- `open()` calls on an already-migrated DB.
--
-- Columns added:
--   runs.shared_brief_hash            TEXT NULL
--   run_context_refs.context_type     TEXT NOT NULL DEFAULT 'path'
--   run_siblings.relation             TEXT NOT NULL DEFAULT 'rerun'
--   run_siblings.created_unix         INTEGER NOT NULL DEFAULT 0

ALTER TABLE runs ADD COLUMN shared_brief_hash TEXT;
ALTER TABLE run_context_refs ADD COLUMN context_type TEXT NOT NULL DEFAULT 'path';
ALTER TABLE run_siblings ADD COLUMN relation TEXT NOT NULL DEFAULT 'rerun';
ALTER TABLE run_siblings ADD COLUMN created_unix INTEGER NOT NULL DEFAULT 0;
