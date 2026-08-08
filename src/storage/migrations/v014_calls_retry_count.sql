-- Moagan meta-database v014 — `calls.retry_count` column.
-- Applied after v013. ADDITIVE only: no FK changes, no renames.
--
-- The retry helper in `src/phases/phase.rs::call_with_retry_parse`
-- (the canonical retry loop the pipeline drives every LLM call
-- through) was already tagged with `WarningEvent.attempt` so the
-- warnings stream could correlate each retry to the failure that
-- triggered it, but the `calls` table carried no record of the
-- attempt index — every retry looked like a fresh call. This
-- migration adds the `retry_count` column declared by the spec
-- (T01-06 §2.1, V4 §8.5) so the post-execution review can answer
-- "how many retries did this LLM call take?" by reading a single
-- SQL query instead of correlating warnings to call records.
--
-- Rows that pre-date the migration get `retry_count=0` so existing
-- runs are not retroactively rewritten (the spec §2.6 keeps the
-- filesystem authoritative and we treat the SQLite index the same
-- way).

ALTER TABLE calls ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_calls_retry_count ON calls(run_id, retry_count);