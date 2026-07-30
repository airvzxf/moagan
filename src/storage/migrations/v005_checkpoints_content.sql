-- v005_checkpoints_content.sql
-- Adds content columns to the `checkpoints` table so the table can
-- mirror the HumanCheckpoint JSON sidecar verbatim. The v001 schema
-- only stored the lifecycle (`resolved` / `note` / `created_unix`)
-- which was enough for v0.1 but the v0.2 Phase D / sub-fase #6
-- requirement is to make the checkpoints searchable without
-- parsing every `checkpoints/h_<uuid>.json` sidecar.
--
-- Columns added:
--   ckp_id            -- stable id assigned by `Checkpoint::new` (h_<uuid7>).
--                        Used as the second part of the natural key
--                        (run_id, ckp_id) so re-runs of the same
--                        checkpoint over the same run don't conflict.
--   question          -- the verbatim question shown to the user.
--   response          -- the raw response captured from stdin (the
--                        `<skipped:non_interactive>` marker when
--                        `interactive=false`).
--   accepted_default  -- 0/1 boolean, true when the user accepted
--                        the default by hitting enter on a yes/no.
--   at_unix           -- unix seconds at capture time (mirrors
--                        HumanCheckpoint.at_unix).
--
-- The v001 lifecycle columns (resolved, note, created_unix,
-- resolved_unix) stay; they are useful for the future dashboard
-- ("which checkpoints are still pending?").

ALTER TABLE checkpoints ADD COLUMN ckp_id TEXT;
ALTER TABLE checkpoints ADD COLUMN question TEXT;
ALTER TABLE checkpoints ADD COLUMN response TEXT;
ALTER TABLE checkpoints ADD COLUMN accepted_default INTEGER NOT NULL DEFAULT 0;
ALTER TABLE checkpoints ADD COLUMN at_unix INTEGER;

-- New natural key: (run_id, ckp_id). The old (run_id, seq) primary
-- key was never used externally (no `seq` is ever populated), so
-- we replace it with a ckp_id-based one. SQLite allows PK changes
-- only via table recreation; we use the standard pattern.
PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS checkpoints_new (
    run_id          TEXT NOT NULL,
    ckp_id          TEXT NOT NULL,
    kind            TEXT NOT NULL,
    question        TEXT,
    response        TEXT,
    accepted_default INTEGER NOT NULL DEFAULT 0,
    at_unix         INTEGER,
    seq             INTEGER NOT NULL DEFAULT 0,
    resolved        INTEGER NOT NULL DEFAULT 0,
    note            TEXT,
    created_unix    INTEGER NOT NULL DEFAULT 0,
    resolved_unix   INTEGER,
    PRIMARY KEY (run_id, ckp_id),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

INSERT INTO checkpoints_new
    (run_id, ckp_id, kind, seq, resolved, created_unix)
SELECT run_id, 'legacy_' || seq, kind, seq, resolved, created_unix
FROM checkpoints;

DROP TABLE checkpoints;
ALTER TABLE checkpoints_new RENAME TO checkpoints;

CREATE INDEX IF NOT EXISTS idx_checkpoints_kind ON checkpoints(run_id, kind);
CREATE INDEX IF NOT EXISTS idx_checkpoints_at_unix ON checkpoints(at_unix);

PRAGMA foreign_keys = ON;
