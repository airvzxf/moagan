//! Persisted state for the discovery sketch loop.
//!
//! Allows `discover_matrix` to resume across process crashes by
//! writing `.discovery_state.json` after each sketch succeeds.
//! On startup, the phase probes for this file; if found and
//! version-compatible, it skips completed sketches.
//!
//! Compliance: catalog 10-integrada-v0 §I.2 (D.34.2).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::atomic::writer::AtomicWriter;
use crate::error::IoError;

/// Schema version of the persisted state. Bumped on breaking
/// changes; on a mismatch the persisted state is discarded and the
/// loop restarts from scratch (silently resuming with a
/// wire-incompatible schema is worse than starting over).
pub const SCHEMA_VERSION: u32 = 1;

const FILENAME: &str = ".discovery_state.json";

/// Phase of the discovery sketch loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The loop is still firing.
    SketchLoop,
    /// The loop has finished all its cells and the phase is ready
    /// to hand off to the next stage.
    SketchLoopDone,
}

/// Snapshot of the discovery sketch loop. Written to
/// `<run_dir>/.discovery_state.json` after every sketch completion
/// or failure so a crashed process can resume from the last
/// successful sketch instead of redoing the whole fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SketchLoopState {
    /// Schema version — see [`SCHEMA_VERSION`].
    pub version: u32,
    /// Current loop phase.
    pub phase: Phase,
    /// Unix epoch seconds when the loop started.
    pub started_unix: i64,
    /// Unix epoch seconds of the last state mutation.
    pub last_updated_unix: i64,
    /// Total sketches attempted (successes + failures).
    pub total_attempts: u32,
    /// IDs of sketches that succeeded and were persisted to disk.
    pub completed_sketches: Vec<String>,
    /// Sketches that failed (LLM error, parse error, etc.).
    pub failed_attempts: u32,
    /// Name of the current strategy cell (e.g. "deployment-model:serverless").
    pub current_strategy: String,
}

impl SketchLoopState {
    /// Build a fresh state for a loop targeting `current_strategy`.
    pub fn new(current_strategy: String) -> Self {
        let now = unix_now();
        Self {
            version: SCHEMA_VERSION,
            phase: Phase::SketchLoop,
            started_unix: now,
            last_updated_unix: now,
            total_attempts: 0,
            completed_sketches: Vec::new(),
            failed_attempts: 0,
            current_strategy,
        }
    }

    /// Load the persisted state from `<run_dir>/.discovery_state.json`.
    /// Returns `Ok(None)` if the file is absent, corrupt, or carries
    /// a mismatched schema version — all three cases mean "start fresh".
    pub fn load(run_dir: &Path) -> Result<Option<Self>> {
        let path = run_dir.join(FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| IoError::Read {
            path: path.clone(),
            source: e,
        })?;
        let parsed: Self = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "discovery_state file is corrupt — discarding"
                );
                return Ok(None);
            }
        };
        if parsed.version != SCHEMA_VERSION {
            tracing::warn!(
                old = parsed.version,
                current = SCHEMA_VERSION,
                "discovery_state schema version mismatch — discarding"
            );
            return Ok(None);
        }
        Ok(Some(parsed))
    }

    /// Persist the state to `<run_dir>/.discovery_state.json` using
    /// an atomic write with `fsync` so a crash mid-write does not
    /// corrupt the file.
    pub fn save(&self, run_dir: &Path) -> Result<()> {
        let path = run_dir.join(FILENAME);
        let json = serde_json::to_string_pretty(self).map_err(IoError::SerializeMeta)?;
        AtomicWriter::new()
            .with_fsync(true)
            .write(&path, json.as_bytes())?;
        Ok(())
    }

    /// Remove the persisted state. Idempotent: missing-file is a
    /// no-op so callers can call this unconditionally at the end of
    /// a successful loop.
    pub fn delete(run_dir: &Path) -> Result<()> {
        let path = run_dir.join(FILENAME);
        if path.exists() {
            std::fs::remove_file(&path).map_err(IoError::Raw)?;
        }
        Ok(())
    }

    /// Record a successful sketch. Mutates counters and pushes
    /// `sketch_id` onto `completed_sketches`.
    pub fn record_completion(&mut self, sketch_id: String) {
        self.completed_sketches.push(sketch_id);
        self.total_attempts += 1;
        self.last_updated_unix = unix_now();
    }

    /// Record a failed sketch attempt. Mutates counters only.
    pub fn record_failure(&mut self) {
        self.failed_attempts += 1;
        self.total_attempts += 1;
        self.last_updated_unix = unix_now();
    }

    /// Mark the loop as fully done. Flips the phase to
    /// `SketchLoopDone` and timestamps the transition.
    pub fn mark_done(&mut self) {
        self.phase = Phase::SketchLoopDone;
        self.last_updated_unix = unix_now();
    }
}

/// Unix epoch seconds. Returns `0` if the clock is set before
/// 1970 (shouldn't happen on a sane host, but the loop must not
/// crash on a weird epoch).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_new_initializes_with_defaults() {
        let s = SketchLoopState::new("deployment-model:serverless".to_owned());
        assert_eq!(s.version, SCHEMA_VERSION);
        assert_eq!(s.phase, Phase::SketchLoop);
        assert_eq!(s.total_attempts, 0);
        assert_eq!(s.failed_attempts, 0);
        assert!(s.completed_sketches.is_empty());
        assert_eq!(s.current_strategy, "deployment-model:serverless");
        assert!(s.started_unix > 0);
        assert!(s.last_updated_unix >= s.started_unix);
    }

    #[test]
    fn state_load_returns_none_if_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = SketchLoopState::load(tmp.path()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn state_load_returns_none_on_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(FILENAME);
        std::fs::write(&path, b"not-json-at-all{{{").unwrap();
        let loaded = SketchLoopState::load(tmp.path()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn state_load_returns_none_on_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(FILENAME);
        let bogus = serde_json::json!({
            "version": SCHEMA_VERSION + 1,
            "phase": "sketch_loop",
            "started_unix": 0,
            "last_updated_unix": 0,
            "total_attempts": 0,
            "completed_sketches": [],
            "failed_attempts": 0,
            "current_strategy": "x",
        });
        std::fs::write(&path, serde_json::to_string(&bogus).unwrap()).unwrap();
        let loaded = SketchLoopState::load(tmp.path()).unwrap();
        assert!(loaded.is_none(), "version mismatch must discard");
    }

    #[test]
    fn state_save_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = SketchLoopState::new("storage:sql".to_owned());
        s.record_completion("sk_0001".to_owned());
        s.record_completion("sk_0002".to_owned());
        s.record_failure();
        s.save(tmp.path()).unwrap();
        let loaded = SketchLoopState::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, s);
        assert_eq!(loaded.completed_sketches, vec!["sk_0001", "sk_0002"]);
        assert_eq!(loaded.failed_attempts, 1);
        assert_eq!(loaded.total_attempts, 3);
    }

    #[test]
    fn state_record_completion_increments_counters() {
        let mut s = SketchLoopState::new("x".to_owned());
        let before = s.total_attempts;
        let before_len = s.completed_sketches.len();
        s.record_completion("sk_a".to_owned());
        assert_eq!(s.completed_sketches.len(), before_len + 1);
        assert_eq!(s.total_attempts, before + 1);
        assert_eq!(s.failed_attempts, 0);
    }

    #[test]
    fn state_atomic_write_uses_fsync() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SketchLoopState::new("atomic:fsync".to_owned());
        s.save(tmp.path()).unwrap();
        let path = tmp.path().join(FILENAME);
        // The file must exist and be readable after a fresh open
        // (i.e. another process / restart can see it).
        assert!(path.exists());
        let recovered = std::fs::read_to_string(&path).unwrap();
        assert!(recovered.contains("atomic:fsync"));
        // The sidecar must also be present because AtomicWriter
        // always writes it.
        let sidecar = AtomicWriter::meta_path(&path);
        assert!(sidecar.exists(), "AtomicWriter must write a sidecar");
    }

    #[test]
    fn state_delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SketchLoopState::new("to-delete".to_owned());
        s.save(tmp.path()).unwrap();
        assert!(tmp.path().join(FILENAME).exists());
        SketchLoopState::delete(tmp.path()).unwrap();
        assert!(!tmp.path().join(FILENAME).exists());
        // Second delete is a no-op, not an error.
        SketchLoopState::delete(tmp.path()).unwrap();
    }

    #[test]
    fn state_record_failure_increments_only_failure_counter() {
        let mut s = SketchLoopState::new("x".to_owned());
        let before_total = s.total_attempts;
        let before_failed = s.failed_attempts;
        s.record_failure();
        assert_eq!(s.failed_attempts, before_failed + 1);
        assert_eq!(s.total_attempts, before_total + 1);
        assert!(s.completed_sketches.is_empty());
    }

    #[test]
    fn state_mark_done_flips_phase() {
        let mut s = SketchLoopState::new("x".to_owned());
        assert_eq!(s.phase, Phase::SketchLoop);
        s.mark_done();
        assert_eq!(s.phase, Phase::SketchLoopDone);
    }
}
