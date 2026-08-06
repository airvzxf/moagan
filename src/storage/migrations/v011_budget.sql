-- v011_budget.sql — Track F, F3: budget enforcement and reduce-under-
-- pressure. The BudgetObserver (`src/phases/budget.rs`) reads
-- `planned_tokens` and `used_tokens` per run, computes the pressure
-- tier (Ok / Soft / Hard) from the soft/hard percentage thresholds
-- configured on the observer, and gates the optional work in
-- RankPhase (stability check), SynthesizePhase (synthesis merge), and
-- JudgePhase (adversary pass) so a long-running run does not overspend.
--
-- Schema:
--   budget_state(run_id, planned_tokens, used_tokens)
--     * `planned_tokens = 0` means "no plan / unlimited". The
--       observer treats this as Ok at any usage level so a run that
--       never sets a budget is never artificially throttled.
--     * `used_tokens` accumulates every `budget_record` write.
--       Atomic under `BEGIN IMMEDIATE` so concurrent phase writers
--       cannot race the increment.
--   budget_events(run_id, phase, tokens, at_unix)
--     * Per-phase audit trail so operators can later answer
--       "which phase burned the budget?" without re-aggregating the
--       `calls` table. `at_unix` is stamped with
--       `crate::time::now_unix_secs()` at write time.
--
-- The migration is additive and forward-only: no existing tables are
-- altered. Pre-v011 databases see no new columns; a legacy
-- `Db::budget_read` returns `(0, 0)` (the Ok pressure) so old code
-- paths continue to work without a migration bump.

CREATE TABLE IF NOT EXISTS budget_state (
    run_id         TEXT PRIMARY KEY,
    planned_tokens INTEGER NOT NULL DEFAULT 0,
    used_tokens    INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS budget_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     TEXT NOT NULL,
    phase      TEXT NOT NULL,
    tokens     INTEGER NOT NULL,
    at_unix    INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_budget_events_run
    ON budget_events(run_id, at_unix);
