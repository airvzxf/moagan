-- v013: closing tables for catalog D.5.1 (per-run state, discovery
-- dedup, plan state). The three tables cover the remaining
-- process-shared state that v008 deferred (outbox_events,
-- redact_audit, manifest_events, process_locks, provider_rollups).
-- v013 adds:
--
--   run_state(run_id PRIMARY KEY, state, updated_unix)
--     * Per-run state machine. One row per run, updated in-place.
--     * `state` is a free-form text label (e.g. "queued",
--       "running", "paused", "completed", "failed"). The schema
--       does not enumerate the vocabulary because the phases
--       already maintain a typed State enum (Rust side). Adding
--       a CHECK constraint would only paper over the Rust-side
--       contract.
--
--   discovery_dedup(run_id, fingerprint PRIMARY KEY)
--     * Discovery phase cache: per-run, per-fingerprint dedup
--       so two consecutive Discovery runs against the same
--       brief hash produce the same personae/angles without
--       re-issuing the LLM call. Fingerprint = BLAKE3 of the
--       cluster labels + cluster summaries (matches the
--       `FacetCacheKey` in src/discovery/facet_cache.rs).
--
--   plan_state(run_id, phase, state PRIMARY KEY)
--     * Per-run, per-phase plan state. Mirrors the in-memory
--       `PlanPhaseState` enum in src/phases/plan.rs so a
--       stale-run recovery path can read the last known state
--       from SQLite on boot and resume without losing the
--       dependency chain. (run_id, phase) is unique.
--
-- The migration is additive and idempotent (CREATE TABLE IF
-- NOT EXISTS, no ALTER TABLE). Pre-v013 databases skip the
-- `run_state` / `discovery_dedup` / `plan_state` writes (older
-- code paths return defaults), so the bump is safe to roll
-- forward at any time.
--
-- Out of scope for this PR: facet_cache (filesystem cache in
-- src/discovery/facet_cache.rs already covers the use case),
-- plan_state STAGE columns (added when Plan gains a stage
-- field), llm_cache (deferred to H3 follow-up alongside the
-- redis-style inverted index), dql (deferred to a dedicated
-- dql migration).

CREATE TABLE IF NOT EXISTS run_state (
    run_id       TEXT PRIMARY KEY,
    state        TEXT NOT NULL,
    updated_unix INTEGER NOT NULL,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE TABLE IF NOT EXISTS discovery_dedup (
    run_id      TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    PRIMARY KEY (run_id, fingerprint)
);

CREATE TABLE IF NOT EXISTS plan_state (
    run_id TEXT NOT NULL,
    phase  TEXT NOT NULL,
    state  TEXT NOT NULL,
    PRIMARY KEY (run_id, phase)
);
