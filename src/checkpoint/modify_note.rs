//! Operator feedback sidecar (Phase D follow-up, F1).
//!
//! When a [`crate::checkpoint::Resolution::Modify`] is returned by any
//! checkpoint (clarify, the rank-phase sensitive trigger, the final
//! deliver checkpoint), the operator's free-form text is captured
//! here so the next-rank call (and any downstream LLM call that has
//! an associated prompt) can prepend the operator's correction
//! without re-asking.
//!
//! Layout
//! ------
//!
//! The sidecar lives at `<run_dir>/state/modify_note.json` with the
//! matching `<run_dir>/state/modify_note.json.meta.json` produced by
//! [`crate::atomic::writer::AtomicWriter`]. The data file is
//! pretty-printed JSON with the following shape:
//!
//! ```json
//! {
//!   "schema_version": "v1",
//!   "phase": "clarify",
//!   "text": "cap output at 10MB",
//!   "captured_at_unix": 1730000000
//! }
//! ```
//!
//! Pause/resume: a single sidecar is overwritten by the latest
//! note, so the most recent `Modify(text)` answer wins after a
//! resume. The load helper returns `None` when the file does not
//! exist (or is unreadable) — a missing sidecar is the common case
//! on a fresh run, not an error condition.
//!
//! Prompt format
//! -------------
//!
//! `prepend_to_prompt` wraps the loaded text in a tagged block so
//! downstream LLMs can distinguish the operator's correction from
//! the underlying brief:
//!
//! ```text
//! [operator_modify_note]
//! cap output at 10MB
//! [/operator_modify_note]
//!
//! <base prompt>
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::atomic::writer::AtomicWriter;
use crate::error::Error;

/// Current schema version. Bumped when the JSON shape changes in a
/// backward-incompatible way; older sidecars stay readable via the
/// same struct.
pub const SCHEMA_VERSION: &str = "v1";

/// JSON body of `<run_dir>/state/modify_note.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifyNote {
    /// Schema version; mirrors the matching [`ArtifactMeta`] field.
    pub schema_version: String,
    /// Phase that captured the note (e.g. `"clarify"`, `"rank"`,
    /// `"deliver"`). Surfaced in the prompt so the operator (and
    /// any audit reader) can trace the note back to its source
    /// checkpoint.
    pub phase: String,
    /// The verbatim operator text.
    pub text: String,
    /// Unix epoch seconds at which the note was sealed.
    pub captured_at_unix: i64,
}

impl ModifyNote {
    /// Build a fresh note. `captured_at_unix` is an explicit
    /// parameter so tests can pin the timestamp; production calls
    /// use [`crate::time::now_unix_secs`].
    pub fn new(phase: impl Into<String>, text: impl Into<String>, captured_at_unix: i64) -> Self {
        let phase_str: String = phase.into();
        tracing::trace!(
            phase = %phase_str,
            captured_at_unix,
            "checkpoint::modify_note::ModifyNote::new"
        );
        Self {
            schema_version: SCHEMA_VERSION.to_owned(),
            phase: phase_str,
            text: text.into(),
            captured_at_unix,
        }
    }
}

/// Compute the canonical sidecar path: `<run_dir>/state/modify_note.json`.
///
/// The `state/` sub-directory is created lazily by the
/// [`AtomicWriter`] when the first note is persisted, so callers
/// that just want to read the file (`load_modify_note`,
/// `prepend_to_prompt`) don't need to materialize the directory
/// first.
pub fn modify_note_path(run_dir: &Path) -> PathBuf {
    let path = run_dir.join("state").join("modify_note.json");
    tracing::trace!(path = %path.display(), "checkpoint::modify_note::modify_note_path");
    path
}

/// Persist `text` for `phase` to `<run_dir>/state/modify_note.json`.
///
/// Overwrites any previous note so the most recent operator answer
/// wins on resume. Uses [`AtomicWriter::new`] so the file is written
/// with the default `fsync_on_commit = true` (F1's I.3 durability
/// contract: a crash mid-write cannot leave a half-written note
/// that the next phase would half-consume).
pub fn persist_modify_note(run_dir: &Path, phase: &str, text: &str) -> Result<()> {
    tracing::debug!(
        phase,
        run_dir = %run_dir.display(),
        text_len = text.len(),
        "checkpoint::modify_note::persist_modify_note: enter"
    );
    let note = ModifyNote::new(phase, text, crate::time::now_unix_secs());
    let bytes = serde_json::to_vec_pretty(&note).map_err(Error::from)?;
    AtomicWriter::new().write(&modify_note_path(run_dir), &bytes)?;
    tracing::info!(
        phase,
        "checkpoint::modify_note::persist_modify_note: persisted"
    );
    Ok(())
}

/// Load the operator note text, if any.
///
/// Returns `None` when:
///
/// - the file does not exist (the typical fresh-run case),
/// - the file is unreadable (permission / I/O error),
/// - the body is not parseable as [`ModifyNote`].
///
/// Returns `Some(text)` verbatim otherwise. We intentionally squash
/// I/O and parse failures into `None` because the operator-note
/// stream is advisory: a malformed or missing sidecar must never
/// abort the pipeline. The audit trail lives on
/// `checkpoints/h_<NN>.json`; the sidecar is a convenience.
pub fn load_modify_note(run_dir: &Path) -> Option<String> {
    tracing::trace!(
        run_dir = %run_dir.display(),
        "checkpoint::modify_note::load_modify_note: enter"
    );
    let path = modify_note_path(run_dir);
    let bytes = std::fs::read(&path).ok()?;
    let note: ModifyNote = serde_json::from_slice(&bytes).ok()?;
    Some(note.text)
}

/// Prepend the operator note to `base`.
///
/// When no note exists (or the sidecar is missing / unreadable),
/// `base` is returned unchanged. Otherwise the note is wrapped in a
/// tagged block and the block is followed by a blank line and the
/// original prompt. The tag (`operator_modify_note`) is a stable
/// keyword a future prompt parser can grep for without any
/// structural coupling.
pub fn prepend_to_prompt(run_dir: &Path, base: &str) -> String {
    tracing::trace!(
        run_dir = %run_dir.display(),
        base_len = base.len(),
        "checkpoint::modify_note::prepend_to_prompt"
    );
    match load_modify_note(run_dir) {
        Some(text) => format!("[operator_modify_note]\n{text}\n[/operator_modify_note]\n\n{base}"),
        None => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modify_note_persists_to_sidecar_with_fsync() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        persist_modify_note(run_dir, "clarify", "cap output at 10MB").unwrap();

        // The data file exists and parses back.
        let data_path = modify_note_path(run_dir);
        assert!(data_path.exists(), "data file must exist");
        let raw = std::fs::read(&data_path).unwrap();
        let note: ModifyNote = serde_json::from_slice(&raw).unwrap();
        assert_eq!(note.schema_version, SCHEMA_VERSION);
        assert_eq!(note.phase, "clarify");
        assert_eq!(note.text, "cap output at 10MB");

        // The matching `.meta.json` sidecar (fsync + integrity
        // metadata) must also exist, mirroring the I.3 contract
        // every other artifact follows.
        let meta_path = AtomicWriter::meta_path(&data_path);
        assert!(
            meta_path.exists(),
            "meta sidecar must exist (AtomicWriter with fsync)"
        );
        let meta_raw = std::fs::read(&meta_path).unwrap();
        let _meta: crate::atomic::writer::ArtifactMeta = serde_json::from_slice(&meta_raw).unwrap();
    }

    #[test]
    fn modify_note_loads_from_sidecar_on_phase_start() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        assert!(
            load_modify_note(run_dir).is_none(),
            "missing sidecar must return None"
        );

        persist_modify_note(run_dir, "rank", "penalise length").unwrap();
        let loaded = load_modify_note(run_dir).expect("load after persist");
        assert_eq!(loaded, "penalise length");
    }

    #[test]
    fn modify_note_overwrites_previous_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        persist_modify_note(run_dir, "clarify", "first note").unwrap();
        persist_modify_note(run_dir, "rank", "second note").unwrap();
        let loaded = load_modify_note(run_dir).unwrap();
        assert_eq!(loaded, "second note");
    }

    #[test]
    fn load_returns_none_for_unreadable_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        // Write a body that will fail to parse so the helper
        // returns None instead of propagating the error.
        let path = modify_note_path(run_dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert!(load_modify_note(run_dir).is_none());
    }

    #[test]
    fn prepend_to_prompt_inserts_tagged_block() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        persist_modify_note(run_dir, "deliver", "include trade-offs").unwrap();
        let out = prepend_to_prompt(run_dir, "rank the proposals");
        assert!(out.contains("[operator_modify_note]"));
        assert!(out.contains("include trade-offs"));
        assert!(out.contains("[/operator_modify_note]"));
        assert!(
            out.ends_with("rank the proposals"),
            "base prompt must remain at the end; got:\n{out}"
        );
    }

    #[test]
    fn prepend_to_prompt_returns_base_unchanged_when_no_note() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        let out = prepend_to_prompt(run_dir, "rank the proposals");
        assert_eq!(out, "rank the proposals");
    }

    #[test]
    fn modify_note_path_resolves_under_state_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path();
        let p = modify_note_path(run_dir);
        assert!(p.ends_with("state/modify_note.json"));
    }

    /// F1 contract: the operator note must survive a simulated
    /// pause / resume. The "pause" is a process exit (we tear
    /// down every handle pointing at the run_dir) and the
    /// "resume" is a fresh process that opens the run_dir from
    /// scratch. The sidecar lives in
    /// `<run_dir>/state/modify_note.json`, which the
    /// `AtomicWriter` already fsync-d onto stable storage, so a
    /// new process that just calls `load_modify_note` sees the
    /// latest value verbatim.
    #[test]
    fn modify_note_survives_pause_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir_path = tmp.path().to_path_buf();

        // Pause phase: write a note from "session A". The
        // returned `PathBuf` is the durable surface we expect
        // to come back with the same bytes after the resume.
        persist_modify_note(&run_dir_path, "clarify", "include trade-offs").unwrap();
        let captured_path = modify_note_path(&run_dir_path);
        let captured_bytes = std::fs::read(&captured_path).unwrap();

        // Drop every handle we hold on the run_dir so the
        // resume path has to re-open from disk.
        drop(run_dir_path);

        // Resume phase: a fresh process would rebuild its
        // handles from disk and call `load_modify_note` with
        // the same `&Path`.
        let run_dir_path2 = tmp.path().to_path_buf();
        let resumed =
            load_modify_note(&run_dir_path2).expect("note must be readable after pause/resume");
        assert_eq!(resumed, "include trade-offs");

        // The bytes on disk are unchanged (no truncation, no
        // rename) so a second load yields the same string and
        // a direct file read surfaces the same payload.
        let raw = std::fs::read(&captured_path).unwrap();
        assert_eq!(raw, captured_bytes);
        let raw_str = std::str::from_utf8(&raw).expect("note is JSON, valid UTF-8");
        assert!(raw_str.contains("include trade-offs"));

        // Subsequent persists overwrite; the resume-side load
        // therefore sees the most recent note (the operator's
        // last answer wins).
        persist_modify_note(&run_dir_path2, "rank", "drop weak evidence").unwrap();
        let second = load_modify_note(&run_dir_path2).unwrap();
        assert_eq!(second, "drop weak evidence");
    }
}
