-- v017: drop the `manifest_versions` table created by v012.
-- It was written by `record_version` which was dropped in PR #433
-- (commit `fdc02d6`). Older runs may have rows; this migration
-- drops the empty table for new runs.
--
-- Drop order is dependency-ordered. `manifest_versions` has no
-- foreign keys (its only column constraint is `PRIMARY KEY
-- (run_id)`) so a single `DROP TABLE IF EXISTS` is enough.
--
-- `PRAGMA foreign_keys = ON` is set defensively to mirror the
-- connection's init hook (`src/storage/sqlite.rs:228`). It is a
-- silent no-op inside the migration's `BEGIN IMMEDIATE` block on
-- the bundled SQLite (3.46+) but keeps the file self-documenting
-- for anyone running it through a standalone `sqlite3` shell.

PRAGMA foreign_keys = ON;

DROP TABLE IF EXISTS manifest_versions;
