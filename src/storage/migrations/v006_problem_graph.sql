-- v006_problem_graph.sql
-- Phase G (v0.3 «tercera etapa»): mirror the canonical
-- `problem_graph.json` sidecar into the SQLite index so a future
-- `moagan inspect` can answer "which deep runs decomposed into a
-- DAG and how many nodes did each one produce?" without reading
-- every run directory.
--
-- The table is intentionally narrow: only the four fields an
-- operator most often wants to filter on. The full DAG lives in
-- the JSON sidecar; this row is the index.
--
-- The v0.3 record_problem_graph() helper is best-effort: a legacy
-- database (PRAGMA user_version < 6) makes the method a no-op so
-- a pre-migration `moagan run` never errors on the new column.

CREATE TABLE IF NOT EXISTS problem_graphs (
    run_id          TEXT NOT NULL,
    brief_blake3    TEXT NOT NULL,
    should_decompose INTEGER NOT NULL DEFAULT 0,
    node_count      INTEGER NOT NULL DEFAULT 0,
    at_unix         INTEGER NOT NULL,
    PRIMARY KEY (run_id),
    FOREIGN KEY (run_id) REFERENCES runs(run_id)
);

CREATE INDEX IF NOT EXISTS idx_problem_graphs_decompose
    ON problem_graphs(should_decompose);
CREATE INDEX IF NOT EXISTS idx_problem_graphs_at_unix
    ON problem_graphs(at_unix);
