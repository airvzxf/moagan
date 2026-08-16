-- v019_process_lease_last_heartbeat.sql
-- Closes D.1.5 of T01-06: the typed `ProcessLease` API
-- (src/storage/lease.rs::acquire_process_lock /
-- heartbeat_process_lock / release_process_lock) needs a separate
-- `last_heartbeat_unix` column so the struct can carry both
-- `acquired_at_unix` (set once on acquire) and `last_heartbeat_unix`
-- (refreshed on every heartbeat) without overloading either of the
-- two pre-existing timestamp columns.
--
-- The pre-existing columns:
--   acquired_at_unix INTEGER NOT NULL  -- "row was acquired at"
--   expires_at_unix  INTEGER NOT NULL  -- "row expires at (now+ttl)"
--
-- Both columns stay in their original semantic role. The new column
-- is purely additive (no ALTER on existing data; the DEFAULT 0
-- covers rows written by the legacy Db::acquire_process_lock /
-- Db::renew_lease primitives, which never read it).
--
-- Migration is forward-only and idempotent via the
-- `column_exists` guard applied in src/storage/sqlite.rs (ALTER
-- TABLE ADD COLUMN cannot be expressed as `ADD COLUMN IF NOT
-- EXISTS` on SQLite versions before 3.35, so the probe-based
-- pattern used by v009 is reused here).

ALTER TABLE process_locks
    ADD COLUMN last_heartbeat_unix INTEGER NOT NULL DEFAULT 0;
