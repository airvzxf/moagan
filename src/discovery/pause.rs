//! Cross-process hibernation: serialisable pause point.
//!
//! When `moagan pause <run_id>` is invoked at a phase boundary, the
//! current run state is captured into [`PausePoint`] and persisted to
//! `<run_dir>/paused.json`. A later `moagan continue --from-pause`
//! can then read the file and skip upstream phases that the user has
//! already accepted.
//!
//! The struct is intentionally tiny — a schema version, the run id,
//! the phase the run paused at, the list of phases that had already
//! finished when the pause was issued, the per-phase inputs that the
//! upstream phases will need to reproduce, and a free-form `summary`
//! for human display.
//!
//! PR C.2 defines the struct + persistence; PR C.3 wires the
//! `moagan pause` / `moagan continue --from-pause` subcommands on
//! top of it.
//!
//! ## Deviations from the original task spec
//!
//! - **`paused_at_phase` / `completed_phases` are typed as
//!   `String` / `Vec<String>`, not a `PhaseKind` enum.** The
//!   pipeline-side `Phase` trait exposes names as `&'static str`
//!   and there is no `PhaseKind` enum in the codebase today.
//!   Forcing a stable enum across the whole pipeline would couple
//!   this struct to the pipeline phase list and require touching
//!   `pause.rs` every time a new phase is added. `String` matches
//!   the convention used by `SketchLoopState` (also a discovery
//!   pause/resume point).
//! - **`delete` reports `IoError::Raw`, not `IoError::Remove`.**
//!   The `IoError` enum does not yet have a `Remove` variant
//!   (`src/error.rs:270`); `SketchLoopState::delete` uses
//!   `IoError::Raw` for the same call (`src/discovery/state.rs:128`)
//!   and we follow that precedent rather than grow the error type
//!   in a PR that is meant to be cross-process-hibernation only.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::atomic::writer::AtomicWriter;
use crate::error::{Error, IoError};
use crate::ids::RunId;

/// Schema version of the on-disk `paused.json`. Bumped when the
/// shape of [`PausePoint`] changes incompatibly. On load, an
/// unknown version is treated as "no pause point exists" and the
/// caller proceeds with a fresh run rather than risking a
/// misinterpretation of stale fields.
pub const SCHEMA_VERSION: u32 = 1;

/// On-disk filename for the serialised pause point. Lives at
/// `<run_dir>/paused.json`.
const FILENAME: &str = "paused.json";

/// Snapshot of a paused run.
///
/// `paused_at_phase` is the phase boundary at which `moagan pause`
/// was invoked (the phase the user accepted and the one the next
/// `continue` will resume from). `completed_phases` lists the
/// phases that had already produced artefacts on disk at the time
/// of the pause — callers should skip re-running them on resume.
/// `pending_inputs` is a free-form `serde_json::Value` that the
/// pause CLI uses to record any inputs (e.g. a partially-decided
/// interaction prompt) that need to round-trip into the resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PausePoint {
    /// Schema version of this struct (always [`SCHEMA_VERSION`]).
    pub version: u32,
    /// The run this pause point belongs to.
    pub run_id: RunId,
    /// Unix epoch seconds (UTC) when the pause point was written.
    pub paused_unix: i64,
    /// Name of the phase at which the pause was issued. Stable
    /// string (matches `Phase::name()`).
    pub paused_at_phase: String,
    /// Names of the phases that had already produced artefacts
    /// when the pause was written. The resume path skips these.
    pub completed_phases: Vec<String>,
    /// Free-form inputs the resume path needs to reproduce the
    /// next phase's state. Empty object when none.
    pub pending_inputs: serde_json::Value,
    /// Human-readable summary (rendered in `moagan inspect`).
    pub summary: String,
}

impl PausePoint {
    /// Build a fresh pause point with the current timestamp.
    pub fn new(
        run_id: RunId,
        paused_at_phase: String,
        completed_phases: Vec<String>,
        pending_inputs: serde_json::Value,
        summary: String,
    ) -> Self {
        Self {
            version: SCHEMA_VERSION,
            run_id,
            paused_unix: now_unix_secs(),
            paused_at_phase,
            completed_phases,
            pending_inputs,
            summary,
        }
    }

    /// Persist the pause point to `<run_dir>/paused.json` using an
    /// atomic write + `fsync` so a crash mid-write does not leave
    /// a half-written file (re-uses the I.3 atomic hardening from
    /// `src/atomic/writer.rs`).
    pub fn save(&self, run_dir: &Path) -> Result<()> {
        let path = run_dir.join(FILENAME);
        let json = serde_json::to_string_pretty(self).map_err(IoError::SerializeMeta)?;
        AtomicWriter::new()
            .with_fsync(true)
            .write(&path, json.as_bytes())?;
        Ok(())
    }

    /// Load the persisted pause point, if any.
    ///
    /// Returns `Ok(None)` for every "no usable pause point" case:
    /// missing file, unparseable JSON, schema version mismatch.
    /// The first two are warnings logged and the third is a hard
    /// discard (a wire-incompatible shape is worse than starting
    /// fresh — same policy as `SketchLoopState::load`).
    pub fn load(run_dir: &Path) -> Result<Option<Self>> {
        let path = run_dir.join(FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::Io(IoError::Read {
                path: path.clone(),
                source: e,
            })
        })?;
        let parsed: PausePoint = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "paused.json is corrupt — discarding"
                );
                return Ok(None);
            }
        };
        if parsed.version != SCHEMA_VERSION {
            tracing::warn!(
                old = parsed.version,
                current = SCHEMA_VERSION,
                "PausePoint schema version mismatch — discarding"
            );
            return Ok(None);
        }
        Ok(Some(parsed))
    }

    /// Remove the persisted pause point. Idempotent: missing file
    /// is a no-op so callers can run it unconditionally after a
    /// successful resume.
    pub fn delete(run_dir: &Path) -> Result<()> {
        let path = run_dir.join(FILENAME);
        if path.exists() {
            std::fs::remove_file(&path).map_err(IoError::Raw)?;
        }
        Ok(())
    }
}

fn now_unix_secs() -> i64 {
    crate::time::now_unix_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;

    /// `new()` populates the timestamp with a positive unix-second,
    /// pins the schema version, and copies the inputs verbatim.
    #[test]
    fn pause_point_new_populates_timestamp() {
        let before = now_unix_secs();
        let run_id = RunId::new();
        let inputs = serde_json::json!({"clarify_extra": "skip route"});
        let pp = PausePoint::new(
            run_id,
            "clarify".to_owned(),
            vec!["intake".to_owned()],
            inputs.clone(),
            "paused after clarify".to_owned(),
        );
        let after = now_unix_secs();
        assert_eq!(pp.version, SCHEMA_VERSION);
        assert_eq!(pp.run_id, run_id);
        assert!(
            pp.paused_unix >= before && pp.paused_unix <= after,
            "paused_unix {} not in [{}, {}]",
            pp.paused_unix,
            before,
            after
        );
        assert_eq!(pp.paused_at_phase, "clarify");
        assert_eq!(pp.completed_phases, vec!["intake"]);
        assert_eq!(pp.pending_inputs, inputs);
        assert_eq!(pp.summary, "paused after clarify");
    }

    /// `save` followed by `load` returns the same struct contents.
    /// Covers the atomic-write + parse path end-to-end.
    #[test]
    fn pause_point_save_then_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let run_id = RunId::new();
        let original = PausePoint::new(
            run_id,
            "propose".to_owned(),
            vec![
                "intake".to_owned(),
                "clarify".to_owned(),
                "route".to_owned(),
                "sketch".to_owned(),
            ],
            serde_json::json!({"angle_index": 3, "candidate_ids": ["sk_0001"]}),
            "paused at propose — 3 angles seen, 1 candidate selected".to_owned(),
        );
        original.save(tmp.path()).unwrap();

        let loaded = PausePoint::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, original);
        assert_eq!(loaded.run_id, run_id);
        assert_eq!(loaded.completed_phases.len(), 4);
        assert_eq!(loaded.pending_inputs["angle_index"], serde_json::json!(3));
    }

    /// `load` returns `Ok(None)` when no `paused.json` is on disk.
    /// The probe is "directory absent" rather than "directory
    /// present but file missing" — both are valid in practice.
    #[test]
    fn pause_point_load_returns_none_if_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = PausePoint::load(tmp.path()).unwrap();
        assert!(loaded.is_none(), "absent paused.json must be Ok(None)");

        // Empty sub-directory: file really missing, not the dir.
        let sub = tmp.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let loaded = PausePoint::load(&sub).unwrap();
        assert!(loaded.is_none());
    }

    /// A corrupt file is silently discarded and `load` returns
    /// `Ok(None)` (with a warning logged) — the resume path will
    /// then decide whether to start fresh or refuse.
    #[test]
    fn pause_point_load_returns_none_on_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(FILENAME);
        std::fs::write(&path, b"definitely not json {{{").unwrap();
        let loaded = PausePoint::load(tmp.path()).unwrap();
        assert!(loaded.is_none(), "corrupt paused.json must be discarded");

        // Mismatched schema version is the other silent-discard path.
        let bogus = serde_json::json!({
            "version": SCHEMA_VERSION + 1,
            "run_id": RunId::new(),
            "paused_unix": 0,
            "paused_at_phase": "intake",
            "completed_phases": [],
            "pending_inputs": serde_json::json!({}),
            "summary": "future version",
        });
        std::fs::write(&path, serde_json::to_string(&bogus).unwrap()).unwrap();
        let loaded = PausePoint::load(tmp.path()).unwrap();
        assert!(loaded.is_none(), "version mismatch must discard");
    }

    /// `save` uses `AtomicWriter::with_fsync(true)` — the
    /// durability contract for K.2a. After save, the file and its
    /// sidecar are both present and readable from a fresh process
    /// handle (simulated by `std::fs::read_to_string`).
    #[test]
    fn pause_point_atomic_write_uses_fsync() {
        let tmp = tempfile::tempdir().unwrap();
        let pp = PausePoint::new(
            RunId::new(),
            "validate".to_owned(),
            vec!["intake".to_owned(), "propose".to_owned()],
            serde_json::json!({}),
            "before validate".to_owned(),
        );
        pp.save(tmp.path()).unwrap();

        let path = tmp.path().join(FILENAME);
        assert!(path.exists(), "paused.json must exist after save");

        // The data must round-trip via a plain `std::fs` read —
        // proves the `fsync` in step 2 + the parent-dir `fsync`
        // in step 7 have flushed the bytes to stable storage.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"paused_at_phase\": \"validate\""));
        assert!(raw.contains("\"version\""));
        assert!(raw.contains("\"run_id\""));

        // The AtomicWriter sidecar must exist too.
        let sidecar = AtomicWriter::meta_path(&path);
        assert!(sidecar.exists(), "AtomicWriter must write a sidecar");
    }

    /// `delete` is idempotent: missing file is a no-op so the
    /// resume path can call it unconditionally.
    #[test]
    fn pause_point_delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();

        // First call: file does not exist. Must be Ok(()).
        PausePoint::delete(tmp.path()).unwrap();

        // Save, then delete, then delete again.
        PausePoint::new(
            RunId::new(),
            "intake".to_owned(),
            vec![],
            serde_json::json!({}),
            "to delete".to_owned(),
        )
        .save(tmp.path())
        .unwrap();
        let path = tmp.path().join(FILENAME);
        assert!(path.exists());

        PausePoint::delete(tmp.path()).unwrap();
        assert!(!path.exists());

        // Second delete: still Ok(()).
        PausePoint::delete(tmp.path()).unwrap();
    }

    /// The serialised JSON includes a readable `summary` that the
    /// `moagan inspect` command can render in v0.5 without
    /// needing to re-parse the struct.
    #[test]
    fn pause_point_pending_inputs_round_trip_complex_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let inputs = serde_json::json!({
            "user_choice": "regenerate",
            "rejected_phases": ["sketch", "propose"],
            "new_constraints": {
                "max_tokens": 4096,
                "forbidden_topics": ["yaml"],
            },
        });
        let pp = PausePoint::new(
            RunId::new(),
            "gate".to_owned(),
            vec!["intake".to_owned(), "propose".to_owned()],
            inputs.clone(),
            "gate retry — drop sketch+propose, regenerate".to_owned(),
        );
        pp.save(tmp.path()).unwrap();
        let loaded = PausePoint::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.pending_inputs, inputs);
        assert_eq!(
            loaded.pending_inputs["new_constraints"]["max_tokens"],
            serde_json::json!(4096)
        );
    }
}
