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

    /// Directory for the cross-run facet cache: `<root>/cache/facets`.
    /// Used by `src/discovery/facet_cache.rs` so a re-run with the
    /// same `(brief, category_id)` skips the LLM call (V4 §6.8 +
    /// catalog decision D.13.13).
    pub fn cross_run_facet_cache_dir(&self) -> PathBuf {
        self.root.join("cache").join("facets")
    }

    /// Ensure the root layout exists. Idempotent.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.runs_dir())?;
        std::fs::create_dir_all(self.cross_run_cache_dir())?;
        std::fs::create_dir_all(self.cross_run_facet_cache_dir())?;
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

    /// `overrides.json` — optional rerun matrix override sidecar.
    pub fn overrides_json_path(&self) -> PathBuf {
        self.root.join("overrides.json")
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

    /// `telemetry/external_audit.jsonl.gz` — append-only JSONL emitted
    /// by the `moagan audit proxy` sidecar. Every line is stored as a
    /// complete gzip member and carries a per-line CRC32.
    pub fn external_audit_path(&self) -> PathBuf {
        self.telemetry().join("external_audit.jsonl.gz")
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

    /// `tags/` directory. (Discovery mode, V4 §6.5.)
    pub fn tags(&self) -> PathBuf {
        self.root.join("tags")
    }

    /// `clusters/` directory. (Discovery mode, V4 §6.6.)
    pub fn clusters(&self) -> PathBuf {
        self.root.join("clusters")
    }

    /// `facets/` directory. (Discovery mode, V4 §6.8.)
    pub fn facets(&self) -> PathBuf {
        self.root.join("facets")
    }

    /// `extractions/` directory. (Discovery mode, V4 §6.9.)
    pub fn extractions(&self) -> PathBuf {
        self.root.join("extractions")
    }

    /// `drafts/` directory. (Discovery mode, V4 §6.10.)
    pub fn drafts(&self) -> PathBuf {
        self.root.join("drafts")
    }

    /// `contradictions/` directory. (Discovery mode, V4 §6.7.)
    pub fn contradictions(&self) -> PathBuf {
        self.root.join("contradictions")
    }

    /// `synthesized/` directory. Phase D (V4 §5.13) — one
    /// `s_<NN>.json` per cluster that triggered synthesis.
    pub fn synthesized(&self) -> PathBuf {
        self.root.join("synthesized")
    }

    /// `cluster_proposals/` directory. Phase D — one
    /// `cp_<NN>.json` per proposal cluster.
    pub fn cluster_proposals_dir(&self) -> PathBuf {
        self.root.join("cluster_proposals")
    }

    /// `adversaries/` directory. Phase D — one
    /// `p_<id>.json` per proposal that triggered the adversarial
    /// judge pass.
    pub fn adversaries(&self) -> PathBuf {
        self.root.join("adversaries")
    }

    /// `problem_graph.json` — Phase G (v0.3). Holds the DAG produced
    /// by `DecomposePhase`; the file always exists after a deep run
    /// (trivial or not). `ensure` does not pre-create it because the
    /// phase is the only writer.
    pub fn problem_graph(&self) -> PathBuf {
        self.root.join("problem_graph.json")
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
            self.tags(),
            self.clusters(),
            self.facets(),
            self.extractions(),
            self.drafts(),
            self.contradictions(),
            self.synthesized(),
            self.cluster_proposals_dir(),
            self.adversaries(),
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
        assert!(path.ends_with("telemetry/external_audit.jsonl.gz"));
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

    /// Discovery mode adds tags/, clusters/, facets/, extractions/,
    /// drafts/, contradictions/. The `ensure` path must create them
    /// so the discovery phases never have to mkdir themselves.
    #[test]
    fn run_dir_ensure_creates_discovery_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        r.ensure().unwrap();
        assert!(r.tags().is_dir());
        assert!(r.clusters().is_dir());
        assert!(r.facets().is_dir());
        assert!(r.extractions().is_dir());
        assert!(r.drafts().is_dir());
        assert!(r.contradictions().is_dir());
    }

    /// Discovery path helpers return the right subdirectory name.
    #[test]
    fn discovery_path_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        assert!(r.tags().ends_with("tags"));
        assert!(r.clusters().ends_with("clusters"));
        assert!(r.facets().ends_with("facets"));
        assert!(r.extractions().ends_with("extractions"));
        assert!(r.drafts().ends_with("drafts"));
        assert!(r.contradictions().ends_with("contradictions"));
    }

    /// Phase D adds a `synthesized/` directory for intra-cluster
    /// synthesis output (V4 §5.13). `ensure()` must create it so the
    /// synthesize phase never has to mkdir itself.
    #[test]
    fn run_dir_ensure_creates_synthesized_dir() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        r.ensure().unwrap();
        assert!(r.synthesized().is_dir());
    }

    /// The `synthesized/` path helper returns the right subdirectory
    /// name (no surprises during debugging).
    #[test]
    fn synthesized_path_helper() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        assert!(r.synthesized().ends_with("synthesized"));
    }

    /// Phase D also adds `cluster_proposals/` and `adversaries/`.
    #[test]
    fn run_dir_ensure_creates_phase_d_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        r.ensure().unwrap();
        assert!(r.cluster_proposals_dir().is_dir());
        assert!(r.adversaries().is_dir());
    }

    /// The `cluster_proposals/` and `adversaries/` path helpers
    /// return the right subdirectory names.
    #[test]
    fn phase_d_path_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let r = h.run_dir(RunId::new());
        assert!(r.cluster_proposals_dir().ends_with("cluster_proposals"));
        assert!(r.adversaries().ends_with("adversaries"));
    }
}
