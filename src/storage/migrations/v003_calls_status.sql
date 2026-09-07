-- Moagan meta-database v003 — `calls.status` column.
-- Applied after v002. ADDITIVE only: no FK changes, no renames.
--
-- The schema carries a `status` column on `calls`
-- (`'ok','error','timeout','cancelled','truncated'`) but the
-- v0.1 implementation never wrote it: every call row was inferable
-- from `http_status` plus `error` but unqueryable by `status` alone.
-- This migration closes the gap so future dashboard queries
-- (`SELECT * FROM calls WHERE status='error'`) work without scanning
-- the JSONL stream.
--
-- Rows that pre-date the migration get `status='unknown'` so existing
-- rows are not retroactively rewritten (the spec      keeps the
-- filesystem authoritative and we treat the SQLite index the same).

ALTER TABLE calls ADD COLUMN status TEXT NOT NULL DEFAULT 'unknown';

CREATE INDEX IF NOT EXISTS idx_calls_status ON calls(run_id, status);