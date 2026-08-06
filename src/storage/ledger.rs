//! Artifact invalidate ledger (D.22.5 companion).
//!
//! Maps `run_id -> Set<artifact_path>` that the Adversary or
//! Refine loop has invalidated. Useful for downstream consumers
//! that want to know which artefacts are stale without walking
//! the filesystem.
//!
//! ## In-memory scope
//!
//! The ledger lives in process memory: a fresh process starts
//! with an empty ledger. Callers that need a durable record
//! mirror the invalidation into SQLite through
//! `crate::storage::sqlite::Db::record_stale_artifact` and
//! treat this in-memory map as a fast-path cache for the
//! within-run path (Refine -> Adversary handoff).
//!
//! ## Concurrency
//!
//! Backed by a `Mutex<HashMap<_, HashSet<_>>>`. The lock is
//! held for the duration of an `invalidate` / `list` call;
//! both are O(1) amortised, so contention is negligible
//! relative to the LLM round-trips that fire invalidations.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

/// In-memory ledger of artefact invalidations keyed by run id.
///
/// One instance is owned by the run context; phases call
/// [`InvalidateLedger::invalidate`] when an artefact is
/// invalidated (e.g. the Adversary rejects a proposal and the
/// downstream sketches need to be re-derived) and other
/// phases call [`InvalidateLedger::list`] to skip work on
/// known-stale artefacts.
#[derive(Debug, Default)]
pub struct InvalidateLedger {
    map: Mutex<HashMap<String, HashSet<PathBuf>>>,
}

impl InvalidateLedger {
    /// Build an empty ledger.
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Mark `path` as invalidated for `run_id`. Re-invalidating
    /// the same path is idempotent — the set is the
    /// deduplication structure.
    pub fn invalidate(&self, run_id: &str, path: PathBuf) {
        self.map
            .lock()
            .expect("invalidate ledger mutex poisoned")
            .entry(run_id.to_string())
            .or_default()
            .insert(path);
    }

    /// Snapshot of the invalidated paths for `run_id`. Returns
    /// an empty `Vec` when the run has no recorded
    /// invalidations.
    pub fn list(&self, run_id: &str) -> Vec<PathBuf> {
        self.map
            .lock()
            .expect("invalidate ledger mutex poisoned")
            .get(run_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// Drop every invalidation recorded for `run_id`. Returns
    /// the number of paths that were cleared.
    pub fn clear(&self, run_id: &str) -> usize {
        let mut map = self.map.lock().expect("invalidate ledger mutex poisoned");
        map.remove(run_id).map(|s| s.len()).unwrap_or(0)
    }

    /// Total number of `(run_id, path)` pairs across all runs.
    pub fn total_invalidated(&self) -> usize {
        self.map
            .lock()
            .expect("invalidate ledger mutex poisoned")
            .values()
            .map(|s| s.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn ledger_records_invalidation_per_run() {
        let ledger = InvalidateLedger::new();
        ledger.invalidate("run-A", p("/tmp/run-A/sketch.json"));
        ledger.invalidate("run-A", p("/tmp/run-A/proposal.json"));
        let paths = ledger.list("run-A");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&p("/tmp/run-A/sketch.json")));
        assert!(paths.contains(&p("/tmp/run-A/proposal.json")));
        assert_eq!(ledger.total_invalidated(), 2);
    }

    #[test]
    fn ledger_lists_paths_for_run() {
        let ledger = InvalidateLedger::new();
        // Insert paths under two distinct run ids; the lookup
        // for run-B must only return run-B's paths.
        ledger.invalidate("run-A", p("/tmp/A/1.json"));
        ledger.invalidate("run-A", p("/tmp/A/2.json"));
        ledger.invalidate("run-B", p("/tmp/B/1.json"));
        let run_a = ledger.list("run-A");
        assert_eq!(run_a.len(), 2);
        for path in &run_a {
            assert!(
                path.starts_with("/tmp/A/"),
                "run-A list leaked a foreign path: {path:?}"
            );
        }
        let run_b = ledger.list("run-B");
        assert_eq!(run_b.len(), 1);
        assert_eq!(run_b[0], p("/tmp/B/1.json"));
        // An unknown run id returns the empty snapshot.
        assert!(ledger.list("run-unknown").is_empty());
    }

    #[test]
    fn ledger_isolates_runs() {
        let ledger = InvalidateLedger::new();
        ledger.invalidate("run-1", p("/1/a.json"));
        ledger.invalidate("run-2", p("/2/a.json"));
        ledger.invalidate("run-2", p("/2/b.json"));
        // Removing run-1 must not affect run-2.
        let cleared = ledger.clear("run-1");
        assert_eq!(cleared, 1);
        assert!(ledger.list("run-1").is_empty());
        assert_eq!(ledger.list("run-2").len(), 2);
        // Clearing a missing run id is a 0-count no-op.
        assert_eq!(ledger.clear("run-ghost"), 0);
    }

    #[test]
    fn ledger_dedupes_repeated_invalidations() {
        let ledger = InvalidateLedger::new();
        let path = p("/tmp/run/single.json");
        for _ in 0..3 {
            ledger.invalidate("run", path.clone());
        }
        assert_eq!(ledger.list("run").len(), 1);
        assert_eq!(ledger.total_invalidated(), 1);
    }
}
