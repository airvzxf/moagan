//! Filesystem layout for moagan runs.
//!
//! The run directory is the canonical source of truth for a run; SQLite
//! is an index. See T01-06 §1.1 ("the file wins, SQLite indexes").
//!
//! Default home: `${MOAGAN_HOME:-~/.local/share/moagan}`. Override at
//! runtime with the `MOAGAN_HOME` env or `--runs-dir` CLI flag.

use std::path::{Path, PathBuf};

use crate::error::{Error, IoError, Result};
use crate::ids::RunId;

/// Resolved root directory for all moagan state.
#[derive(Debug, Clone)]
pub struct MoaganHome {
    root: PathBuf,
}

impl MoaganHome {
    /// Build a `MoaganHome` from an explicit root path.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve `${MOAGAN_HOME:-~/.local/share/moagan}`.
    pub fn resolve() -> Result<Self> {
        if let Ok(env) = std::env::var("MOAGAN_HOME")
            && !env.trim().is_empty()
        {
            return Ok(Self::at(PathBuf::from(env)));
        }
        if let Some(home) = std::env::var_os("HOME")
            && !home.is_empty()
        {
            return Ok(Self::at(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("moagan"),
            ));
        }
        Err(Error::Io(IoError::Raw(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve user home directory; set MOAGAN_HOME",
        ))))
    }

    /// Root path (e.g. `~/.local/share/moagan`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory containing every run: `<root>/.runs`.
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join(".runs")
    }

    /// Root-level meta database: `<root>/meta.sqlite`.
    pub fn meta_db_path(&self) -> PathBuf {
        self.root.join("meta.sqlite")
    }

    /// Directory for cross-run LLM cache: `<root>/cache/llm`.
    pub fn cross_run_cache_dir(&self) -> PathBuf {
        self.root.join("cache").join("llm")
    }

    /// Ensure the root layout exists. Idempotent.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.runs_dir())?;
        std::fs::create_dir_all(self.cross_run_cache_dir())?;
        Ok(())
    }

    /// Directory for a specific run.
    pub fn run_dir(&self, run_id: RunId) -> RunDir<'_> {
        RunDir {
            root: self.runs_dir().join(run_id.to_string()),
            _home: self,
        }
    }
}

/// Path namespace for a single run. All paths are lazy.
pub struct RunDir<'a> {
    root: PathBuf,
    _home: &'a MoaganHome,
}

impl std::fmt::Debug for RunDir<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunDir").field("root", &self.root).finish()
    }
}

impl Clone for RunDir<'_> {
    fn clone(&self) -> Self {
        // SAFETY: the underlying `MoaganHome` reference is held for the
        // lifetime of the process via `RunContext::new`. We extend the
        // lifetime of the reference to match the new RunDir's lifetime
        // (always shorter than 'static).
        #[allow(clippy::borrow_deref_ref)]
        let home: &'static MoaganHome = unsafe { &*(self._home as *const MoaganHome) };
        Self {
            root: self.root.clone(),
            _home: home,
        }
    }
}

impl RunDir<'_> {
    /// Root directory of the run.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `manifest.json` — run-level parameterisation + state.
    pub fn manifest(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// `brief.json` — canonical brief produced by intake/clarify.
    pub fn brief(&self) -> PathBuf {
        self.root.join("brief.json")
    }

    /// `sketches/` directory — short, opinionated hypotheses emitted
    /// by the `SketchPhase` (v0.2). Empty for `fast` mode.
    pub fn sketches(&self) -> PathBuf {
        self.root.join("sketches")
    }

    /// `proposals/` directory.
    pub fn proposals(&self) -> PathBuf {
        self.root.join("proposals")
    }

    /// `critiques/` directory.
    pub fn critiques(&self) -> PathBuf {
        self.root.join("critiques")
    }

    /// `revisions/` directory.
    pub fn revisions(&self) -> PathBuf {
        self.root.join("revisions")
    }

    /// `validation/` directory.
    pub fn validation(&self) -> PathBuf {
        self.root.join("validation")
    }

    /// `evaluations/` directory.
    pub fn evaluations(&self) -> PathBuf {
        self.root.join("evaluations")
    }

    /// `rankings/` directory.
    pub fn rankings(&self) -> PathBuf {
        self.root.join("rankings")
    }

    /// `final/` directory.
    pub fn final_dir(&self) -> PathBuf {
        self.root.join("final")
    }

    /// `logs/` directory.
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `telemetry/` directory.
    pub fn telemetry(&self) -> PathBuf {
        self.root.join("telemetry")
    }

    /// `telemetry/external_audit.jsonl` — append-only JSONL emitted
    /// by the `moagan audit proxy` sidecar. Each line carries a
    /// per-line CRC32 so a torn write is detectable. The extension
    /// is `.jsonl` (not `.jsonl.gz`) on purpose: the sidecar writes
    /// plain JSONL with one flush per line, which keeps a torn tail
    /// readable line-by-line without depending on a multi-member
    /// gzip stream being re-parseable.
    pub fn external_audit_path(&self) -> PathBuf {
        self.telemetry().join("external_audit.jsonl")
    }

    /// `telemetry/external_audit.verify.tsv` — output of `moagan audit
    /// verify` for this run.
    pub fn external_audit_verify_path(&self) -> PathBuf {
        self.telemetry().join("external_audit.verify.tsv")
    }

    /// `cache/` directory (intra-run LLM cache).
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// `checkpoints/` directory.
    pub fn checkpoints(&self) -> PathBuf {
        self.root.join("checkpoints")
    }

    /// Create every directory the run expects. Idempotent.
    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.root.clone(),
            self.proposals(),
            self.critiques(),
            self.revisions(),
            self.validation(),
            self.evaluations(),
            self.rankings(),
            self.final_dir(),
            self.logs(),
            self.telemetry(),
            self.cache(),
            self.checkpoints(),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_resolves_to_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        assert_eq!(h.root(), tmp.path());
        assert_eq!(h.runs_dir(), tmp.path().join(".runs"));
        assert_eq!(h.meta_db_path(), tmp.path().join("meta.sqlite"));
    }

    #[test]
    fn ensure_creates_layout() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        h.ensure().unwrap();
        assert!(h.runs_dir().is_dir());
        assert!(h.cross_run_cache_dir().is_dir());
    }

    #[test]
    fn run_dir_ensure_supports_external_audit() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        r.ensure().unwrap();
        let path = r.external_audit_path();
        assert!(path.ends_with("telemetry/external_audit.jsonl"));
        std::fs::write(&path, b"test").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn run_dir_external_audit_verify_path() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        let p = r.external_audit_verify_path();
        assert!(p.ends_with("telemetry/external_audit.verify.tsv"));
    }

    #[test]
    fn run_dir_paths() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let id = RunId::new();
        let r = h.run_dir(id);
        assert_eq!(r.root(), h.runs_dir().join(id.to_string()));
        assert!(r.manifest().ends_with("manifest.json"));
        assert!(r.proposals().ends_with("proposals"));
        assert!(r.final_dir().ends_with("final"));
        assert!(r.telemetry().ends_with("telemetry"));
    }

    #[test]
    fn run_dir_ensure_creates_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        r.ensure().unwrap();
        assert!(r.proposals().is_dir());
        assert!(r.rankings().is_dir());
        assert!(r.final_dir().is_dir());
        assert!(r.telemetry().is_dir());
    }
}
