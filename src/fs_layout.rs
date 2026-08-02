//! Filesystem layout for moagan runs.
//!
//! The run directory is the canonical source of truth for a run; SQLite
//! is an index. See T01-06 §1.1 ("the file wins, SQLite indexes").
//!
//! Default home: `${MOAGAN_HOME:-~/.local/share/moagan}`. Override at
//! runtime with the `MOAGAN_HOME` env or `--runs-dir` CLI flag.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// Stable, well-known paths for a run, exposed both as
/// run-relative strings and as absolute paths (D.12.16).
///
/// `relative` survives the run being moved or archived
/// (`brief.json` is always relative to the run root). `absolute`
/// is the resolved-on-disk path for the current `MoaganHome`,
/// which is what the dashboard / inspect surface consume to fetch
/// the actual files. The two maps share the same keys so a caller
/// can pull either form by the same logical id.
///
/// Keys cover the seven spec-stabilised artefacts:
/// - `brief`: canonical brief produced by intake/clarify
/// - `final`: directory holding `portfolio.md` and per-proposal
///   exports
/// - `manifest`: run-level parameterisation + state
/// - `ranking`: ranked proposals JSON
/// - `calls`: per-call telemetry (JSONL gzipped)
/// - `phases`: per-phase events (JSONL gzipped)
/// - `warnings`: warnings log (plain JSONL)
/// - `checkpoints`: human-checkpoint log (plain JSONL)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunPaths {
    /// Run-relative paths. Survives a move / archive.
    pub relative: std::collections::BTreeMap<String, String>,
    /// Absolute paths. Resolved against `MoaganHome`.
    pub absolute: std::collections::BTreeMap<String, PathBuf>,
}

impl RunPaths {
    /// Resolve the standard set of run paths for `run_id` under
    /// `home`. Returns a `RunPaths` with both maps populated.
    /// Idempotent: does not touch the filesystem.
    pub fn resolve(home: &MoaganHome, run_id: RunId) -> Self {
        let run_dir = home.run_dir(run_id);
        let root = run_dir.root().to_path_buf();
        let entries: [(&str, &str); 8] = [
            ("brief", "brief.json"),
            ("final", "final"),
            ("manifest", "manifest.json"),
            ("ranking", "rankings/ranking.json"),
            ("calls", "telemetry/calls.jsonl.gz"),
            ("phases", "telemetry/phases.jsonl.gz"),
            ("warnings", "telemetry/warnings.jsonl"),
            ("checkpoints", "telemetry/checkpoints.jsonl"),
        ];
        let mut relative = std::collections::BTreeMap::new();
        let mut absolute = std::collections::BTreeMap::new();
        for (key, sub) in entries {
            relative.insert(key.to_string(), sub.to_string());
            absolute.insert(key.to_string(), root.join(sub));
        }
        Self { relative, absolute }
    }

    /// Look up an absolute path by key. Returns `None` if the
    /// key is not in the catalog.
    pub fn absolute(&self, key: &str) -> Option<&PathBuf> {
        self.absolute.get(key)
    }

    /// Look up a run-relative path by key. Returns `None` if the
    /// key is not in the catalog.
    pub fn relative_str(&self, key: &str) -> Option<&str> {
        self.relative.get(key).map(String::as_str)
    }

    /// Number of catalog entries. Always 8 by construction.
    pub fn len(&self) -> usize {
        self.relative.len()
    }

    /// True when no entries are present. Should never happen for
    /// a `RunPaths` returned by `resolve`; the method exists so
    /// callers can `is_empty()` defensively after deserialising.
    pub fn is_empty(&self) -> bool {
        self.relative.is_empty()
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

    // -- Phase M (D.12.16) — RunPaths::resolve() -------------------------

    /// `resolve` returns both maps populated for the standard
    /// eight keys (D.12.16).
    #[test]
    fn run_paths_resolve_returns_both_maps() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let id = RunId::new();
        let paths = RunPaths::resolve(&h, id);
        assert_eq!(paths.relative.len(), 8);
        assert_eq!(paths.absolute.len(), 8);
        assert_eq!(paths.len(), 8);
        assert!(!paths.is_empty());
    }

    /// Every documented key is present in both maps.
    #[test]
    fn run_paths_resolve_contains_all_documented_keys() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let paths = RunPaths::resolve(&h, RunId::new());
        for key in [
            "brief",
            "final",
            "manifest",
            "ranking",
            "calls",
            "phases",
            "warnings",
            "checkpoints",
        ] {
            assert!(
                paths.relative.contains_key(key),
                "missing relative key: {key}"
            );
            assert!(
                paths.absolute.contains_key(key),
                "missing absolute key: {key}"
            );
        }
    }

    /// Relative paths are pure suffixes (no leading slash,
    /// never absolute).
    #[test]
    fn run_paths_relative_are_run_relative() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let paths = RunPaths::resolve(&h, RunId::new());
        for (key, rel) in &paths.relative {
            assert!(!rel.starts_with('/'), "{key}: relative starts with /");
            assert!(!rel.contains(".."), "{key}: relative contains ..");
        }
    }

    /// Absolute paths point at the run dir + the relative suffix.
    /// After `run_dir.ensure()`, the directory branches exist
    /// (brief/manifest/ranking paths are files we create on
    /// demand, but the parent dirs exist).
    #[test]
    fn run_paths_absolute_resolve_to_existing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let id = RunId::new();
        let rd = h.run_dir(id);
        rd.ensure().unwrap();
        let paths = RunPaths::resolve(&h, id);

        // The directory keys must point at directories that exist
        // after `ensure()`.
        for key in ["final", "calls", "phases", "warnings", "checkpoints"] {
            let p = paths
                .absolute(key)
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(p.parent().unwrap().is_dir(), "{key} parent missing: {p:?}");
        }
        // File keys must point inside the run dir, even though
        // the file itself has not been written yet.
        for key in ["brief", "manifest", "ranking"] {
            let p = paths
                .absolute(key)
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(
                p.starts_with(rd.root()),
                "{key}: absolute {p:?} not under run root {:?}",
                rd.root()
            );
        }
    }

    /// RunPaths round-trips through serde so it can live inside
    /// `Manifest.lineage_paths` (M.5) without bespoke plumbing.
    #[test]
    fn run_paths_round_trips_json() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let h = MoaganHome::resolve().unwrap();
        let paths = RunPaths::resolve(&h, RunId::new());
        let j = serde_json::to_string(&paths).unwrap();
        let back: RunPaths = serde_json::from_str(&j).unwrap();
        assert_eq!(paths, back);
    }
}
