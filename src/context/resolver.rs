//! Classify and resolve a `--context` reference.
//!
//! A "context reference" is one of:
//!
//! - `ContextRef::RunId(RunId)` — a UUID v7 of a previous run.
//! - `ContextRef::FilePath(PathBuf)` — a single `.md` file on disk.
//! - `ContextRef::DirPath(PathBuf)` — a directory walked for `.md`s.
//!
//! `resolve_classify` is the entry point: it probes an existing path
//! first, then parses a UUID when no path exists. This keeps a file or
//! directory whose literal name is UUID-shaped addressable as a path.
//!
//! Both variants must resolve to something that exists on disk; an
//! unknown UUID or a missing path is a hard `Error::InvalidArgs`.
//! The reason for "hard fail" is that `--context` is a user
//! flag — silently falling back to an empty context would be a
//! subtle way to drop the user's input.

use std::path::{Path, PathBuf};

use crate::fs_layout::safe_path;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;

/// The shape of a `--context` argument, after classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRef {
    /// A UUID v7 that resolves to `<home>/.runs/<id>/`.
    RunId(RunId),
    /// A single `.md` file on disk.
    FilePath(PathBuf),
    /// A directory walked for `.md`s.
    DirPath(PathBuf),
}

impl ContextRef {
    /// Stable lowercase label for telemetry and the SQLite
    /// `run_context_refs.context_type` column.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunId(_) => "run_id",
            Self::FilePath(_) => "path",
            Self::DirPath(_) => "dir",
        }
    }

    /// The path or id as a string, for the `run_context_refs.source_path`
    /// column. For `RunId` we use the UUID text; for paths we use the
    /// path as-is.
    pub fn source(&self) -> String {
        match self {
            Self::RunId(r) => r.to_string(),
            Self::FilePath(p) | Self::DirPath(p) => p.display().to_string(),
        }
    }
}

/// Classify `input` as a `RunId`, file, or directory. Pure: no
/// filesystem probe. The UUID parse is the cheap check; if it
/// fails the caller should treat the input as a path candidate.
pub fn classify_no_io(input: &str) -> Result<ContextRef> {
    if let Ok(uuid) = uuid::Uuid::parse_str(input) {
        return Ok(ContextRef::RunId(RunId::from_uuid(uuid)));
    }
    Err(Error::InvalidArgs(format!(
        "context {input:?} is neither a valid UUID nor a path"
    )))
}

/// Try to classify `input` as an existing filesystem path first, then
/// as a UUID. Returns `Error::InvalidArgs` for "neither a known UUID
/// nor an existing path on disk".
///
/// The function deliberately does NOT check that the UUID resolves
/// to an existing run dir — that's the caller's responsibility
/// (`resolve`). The reason: `resolve_classify` is meant to be cheap
/// and side-effect-free so `run` can persist the classification
/// before deciding whether to load the actual contents.
pub fn resolve_classify(input: &str, home: &MoaganHome) -> Result<ContextRef> {
    if let Some(path_ref) = resolve_path(Path::new(input)) {
        return Ok(path_ref);
    }
    if let Ok(uuid) = uuid::Uuid::parse_str(input) {
        return Ok(ContextRef::RunId(RunId::from_uuid(uuid)));
    }
    let run_dir = home.runs_dir().join(input).display().to_string();
    Err(Error::InvalidArgs(format!(
        "context {input:?} is neither a valid run id (looked for {run_dir}) nor an existing path"
    )))
}

/// Pure path probe. Returns `Some` only when the path exists and
/// classifies as file or directory; `None` otherwise. Symlinks are
/// followed by `metadata()` so a symlink to a file is a
/// `FilePath` and a symlink to a directory is a `DirPath`.
///
/// D.29.1: rejects `..` traversal or symlink escapes via
/// [`safe_path`]. The parent directory is the natural root: the
/// candidate must live next to whatever the operator named.
fn resolve_path(path: &Path) -> Option<ContextRef> {
    let root = path.parent().unwrap_or(Path::new("/"));
    let safe = safe_path(root, path).ok()?;
    let meta = std::fs::metadata(&safe).ok()?;
    if meta.is_file() {
        Some(ContextRef::FilePath(safe))
    } else if meta.is_dir() {
        Some(ContextRef::DirPath(safe))
    } else {
        None
    }
}

/// Resolve a `--context` argument into a `ContextRef` and, when it
/// is a `RunId`, validate that the run dir exists under
/// `<home>/.runs/<id>/`. Errors out with `Error::InvalidArgs` for
/// any other failure (no such UUID, missing path, etc.).
pub fn resolve(home: &MoaganHome, raw: &str) -> Result<ContextRef> {
    let r = resolve_classify(raw, home)?;
    if let ContextRef::RunId(id) = &r {
        let run_dir = home.run_dir(*id);
        if !run_dir.root().exists() {
            return Err(Error::InvalidArgs(format!(
                "context run id {id} not found under {}",
                home.runs_dir().display()
            )));
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid UUID v7 classifies as `RunId`, no IO needed.
    #[test]
    fn resolve_classify_uuid_v7() {
        crate::test_support::with_moagan_home("resolve_classify_uuid_v7", |_home| {
            let home = MoaganHome::resolve().unwrap();
            let id = RunId::new();
            let r = resolve_classify(&id.to_string(), &home).unwrap();
            assert_eq!(r, ContextRef::RunId(id));
            assert_eq!(r.kind(), "run_id");
            assert_eq!(r.source(), id.to_string());
        });
    }

    /// A path to an existing `.md` file classifies as `FilePath`.
    #[test]
    fn resolve_classify_path_md() {
        crate::test_support::with_moagan_home("resolve_classify_path_md", |home| {
            let path = home.join("notes.md");
            std::fs::write(&path, "# notes").unwrap();
            let home_arc = MoaganHome::resolve().unwrap();
            let r = resolve_classify(path.to_str().unwrap(), &home_arc).unwrap();
            assert_eq!(r.kind(), "path");
            match r {
                ContextRef::FilePath(p) => assert_eq!(p, path),
                other => panic!("expected FilePath, got {other:?}"),
            }
        });
    }

    /// A path to an existing directory classifies as `DirPath`.
    #[test]
    fn resolve_classify_path_dir() {
        crate::test_support::with_moagan_home("resolve_classify_path_dir", |home| {
            let dir = home.join("ctx");
            std::fs::create_dir_all(&dir).unwrap();
            let home_arc = MoaganHome::resolve().unwrap();
            let r = resolve_classify(dir.to_str().unwrap(), &home_arc).unwrap();
            assert_eq!(r.kind(), "dir");
            match r {
                ContextRef::DirPath(p) => assert_eq!(p, dir),
                other => panic!("expected DirPath, got {other:?}"),
            }
        });
    }

    /// A string that's neither a UUID nor an existing path is a
    /// hard `Error::InvalidArgs` — `--context` is a user flag and
    /// silently dropping it would be worse than failing.
    #[test]
    fn resolve_classify_unknown_errors() {
        crate::test_support::with_moagan_home("resolve_classify_unknown_errors", |home| {
            let missing = home.join("ghost").display().to_string();
            let home_arc = MoaganHome::resolve().unwrap();
            let err = resolve_classify(&missing, &home_arc).unwrap_err();
            assert!(matches!(err, Error::InvalidArgs(_)), "got: {err}");
            let msg = err.to_string();
            assert!(msg.contains("context"), "msg: {msg}");
        });
    }

    /// `resolve` validates that a UUID-shaped context actually points
    /// at an existing run dir; a random UUID with no run dir is an
    /// `Error::InvalidArgs`.
    #[test]
    fn resolve_missing_run_dir_errors() {
        crate::test_support::with_moagan_home("resolve_missing_run_dir_errors", |_home| {
            let home = MoaganHome::resolve().unwrap();
            let fake = uuid::Uuid::now_v7().to_string();
            let err = resolve(&home, &fake).unwrap_err();
            assert!(matches!(err, Error::InvalidArgs(_)), "got: {err}");
        });
    }

    /// `resolve` accepts an existing run dir.
    #[test]
    fn resolve_existing_run_dir_ok() {
        crate::test_support::with_moagan_home("resolve_existing_run_dir_ok", |_home| {
            let home = MoaganHome::resolve().unwrap();
            home.ensure().unwrap();
            let id = RunId::new();
            std::fs::create_dir_all(home.runs_dir().join(id.to_string())).unwrap();
            let r = resolve(&home, &id.to_string()).unwrap();
            assert_eq!(r, ContextRef::RunId(id));
        });
    }

    /// `classify_no_io` does no filesystem probe.
    #[test]
    fn classify_no_io_does_not_touch_filesystem() {
        // No tmpdir set up; the function only parses.
        let r = classify_no_io("definitely-not-a-uuid").unwrap_err();
        assert!(matches!(r, Error::InvalidArgs(_)));
    }

    /// `kind()` and `source()` round-trip the label and the path/id.
    #[test]
    fn kind_and_source_for_each_variant() {
        let id = RunId::new();
        let r = ContextRef::RunId(id);
        assert_eq!(r.kind(), "run_id");
        assert_eq!(r.source(), id.to_string());
        let r = ContextRef::FilePath(PathBuf::from("/tmp/notes.md"));
        assert_eq!(r.kind(), "path");
        assert_eq!(r.source(), "/tmp/notes.md");
        let r = ContextRef::DirPath(PathBuf::from("/tmp/dir"));
        assert_eq!(r.kind(), "dir");
        assert_eq!(r.source(), "/tmp/dir");
    }
}
