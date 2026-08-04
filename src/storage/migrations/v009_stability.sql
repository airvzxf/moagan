-- v009_stability.sql — Phase A fix W2: persist the ranking-stability
-- verdict into SQLite so the dashboard's "stability per run" view
-- can answer without re-running the perturbation loop.
--
-- Columns added to `runs` (additive, idempotent via the v009
-- idempotent applier in src/storage/sqlite.rs):
--   runs.stability_score    REAL  -- [0.0, 1.0]
--   runs.stability_label    TEXT  -- 'stable' | 'sensitive'
--   runs.stability_sigma    REAL  -- perturbation sigma used

-- The migration runner detects columns via `PRAGMA table_info(runs)`
-- and only adds the missing ones, so re-running v009 on a DB that
-- already has the columns is a no-op.

ALTER TABLE runs ADD COLUMN stability_score REAL;
ALTER TABLE runs ADD COLUMN stability_label TEXT;
ALTER TABLE runs ADD COLUMN stability_sigma REAL;
