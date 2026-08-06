-- v012: versioned manifest schema.
-- Tracks which Manifest schema version was in effect when each
-- sidecar was written, enabling safe forward-compat reads.

CREATE TABLE IF NOT EXISTS manifest_versions (
    run_id TEXT NOT NULL,
    manifest_version INTEGER NOT NULL,
    written_at_unix INTEGER NOT NULL,
    PRIMARY KEY (run_id)
);
