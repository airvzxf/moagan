-- v018: saturation_events table (catalog 10-integrada-v0 §D.23 + §D.27
-- cross-run aggregation, v0.8 telemetry push-side).
--
-- One row per `SaturationEvent` fired by the runtime when a provider
-- circuit breaker opens, a rate limiter budget is exhausted, or a
-- token-plan threshold is crossed. The table is the SQLite mirror of
-- the per-run `telemetry/saturation.jsonl` stream; the JSONL stream is
-- the canonical timeline (V4 §6.5-6.10), SQLite is the queryable index
-- for `moagan telemetry alerts list` (T01-06 §10.7 + add-on §D.27).
--
-- `run_id` is nullable so a pre-pipeline probe (e.g. a registry
-- discovery call) can still land an event that is not yet attached to
-- any run — mirrors the `redact_audit` design in v008.
--
-- `kind` is constrained to the three values the runtime emits:
--   - `token`      plan / budget threshold crossed
--   - `error`      provider circuit breaker opened (catalog §D.19.5)
--   - `rate_limit` token-bucket budget exhausted (catalog §D.19.6)
--
-- `threshold_pct` is the saturation percentage at which the event was
-- triggered (0.0–100.0). For `error` it is `100.0` (the breaker is
-- fully open); for `rate_limit` it is the bucket level at the time of
-- rejection; for `token` it is the plan consumption percentage.
--
-- `details` is free-form JSON for debugging context (e.g. observed
-- tokens, bucket capacity, breaker threshold). The runtime never
-- embeds raw payloads here.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS saturation_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id          TEXT,
    provider        TEXT NOT NULL,
    model           TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('token','error','rate_limit')),
    threshold_pct   REAL NOT NULL,
    observed_at_unix INTEGER NOT NULL,
    details         TEXT,
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_saturation_provider
    ON saturation_events(provider, observed_at_unix);
CREATE INDEX IF NOT EXISTS idx_saturation_kind
    ON saturation_events(kind, observed_at_unix);
CREATE INDEX IF NOT EXISTS idx_saturation_run
    ON saturation_events(run_id, observed_at_unix);
