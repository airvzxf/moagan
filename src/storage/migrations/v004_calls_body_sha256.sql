ALTER TABLE calls ADD COLUMN body_sha256 TEXT;

CREATE INDEX IF NOT EXISTS idx_calls_body_sha256 ON calls(run_id, body_sha256);
