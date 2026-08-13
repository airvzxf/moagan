-- v016: drop the four v013/v011 tables that were created
-- speculatively and never read in production. Round-2 audit
-- (2026-08-12) flagged them as dead schema:
--
--   run_state        (v013) — never written, never read
--   discovery_dedup  (v013) — never written, never read
--   plan_state       (v013) — never written, never read
--   budget_events    (v011) — written by `Db::budget_record` but
--                              never read by any caller
--
-- Drop order is dependency-ordered. All four tables reference
-- `runs(run_id)`; `runs` is the parent and is preserved. SQLite
-- drops indexes attached to the table automatically
-- (`idx_budget_events_run` disappears with `budget_events`).
--
-- `Db::budget_record` no longer appends a `budget_events` row —
-- only the `budget_state` aggregate UPSERT remains, which is the
-- only consumer of these helpers (see `BudgetObserver` in
-- `src/phases/budget.rs`).
--
-- `PRAGMA foreign_keys = ON` is set defensively to mirror the
-- connection's init hook (`src/storage/sqlite.rs:228`). It is a
-- silent no-op inside the migration's `BEGIN IMMEDIATE` block on
-- the bundled SQLite (3.46+) but keeps the file self-documenting
-- for anyone running it through a standalone `sqlite3` shell.

PRAGMA foreign_keys = ON;

DROP TABLE IF EXISTS run_state;
DROP TABLE IF EXISTS discovery_dedup;
DROP TABLE IF EXISTS plan_state;
DROP TABLE IF EXISTS budget_events;
