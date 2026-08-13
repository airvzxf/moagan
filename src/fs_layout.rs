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

/// Canonicalize a caller-supplied path and reject escapes (D.29.1).
///
/// `candidate` is treated as either an absolute path or a path
/// relative to `root`. The helper canonicalises the joined path and
/// returns `Error::PathTraversal` if the result falls outside
/// `root`. Two attack surfaces are covered:
///
/// - `..` traversal inside a relative candidate
///   (`../../etc/passwd` resolves to `/etc/passwd` on Unix).
/// - Symlinks whose target sits outside `root` (canonicalisation
///   follows symlinks and exposes the true destination).
///
/// On non-Unix platforms the helper still rejects `..` via the
/// lexical check, but symlink-following is delegated to the
/// underlying `canonicalize` call.
///
/// # Errors
///
/// - [`Error::PathTraversal`] when the candidate escapes `root`
///   (either via `..` or via a symlink).
/// - [`Error::Io`] when `canonicalize` fails for any reason other
///   than the path not existing (a missing target file is OK —
///   the helper is meant to validate the *input* path, not the
///   target's existence).
pub fn safe_path(root: &Path, candidate: &Path) -> Result<PathBuf> {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    // Cheap lexical rejection of `..` even before canonicalisation,
    // so the helper surfaces a clean error on inputs that the OS
    // would also reject for an absent parent segment.
    for comp in joined.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return Err(Error::PathTraversal(format!(
                "{} contains `..`",
                candidate.display()
            )));
        }
    }
    let canonical_root = root.canonicalize().map_err(|e| {
        Error::Io(IoError::Raw(std::io::Error::new(
            e.kind(),
            format!(
                "safe_path: cannot canonicalize root {}: {e}",
                root.display()
            ),
        )))
    })?;
    // `canonicalize` may fail when the candidate does not yet exist
    // (e.g. a brand-new directory a CLI flag just told us to
    // create). Fall back to canonicalising the parent and re-joining
    // the trailing component so we still get the symlink check.
    let canonical_candidate = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let parent = joined.parent().unwrap_or(&joined);
            let tail = joined
                .file_name()
                .ok_or_else(|| {
                    Error::PathTraversal(format!(
                        "safe_path: candidate {} has no filename component",
                        candidate.display()
                    ))
                })?
                .to_owned();
            let canon_parent = parent.canonicalize().map_err(|e| {
                Error::Io(IoError::Raw(std::io::Error::new(
                    e.kind(),
                    format!(
                        "safe_path: cannot canonicalize parent of {}: {e}",
                        candidate.display()
                    ),
                )))
            })?;
            canon_parent.join(tail)
        }
    };
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(Error::PathTraversal(format!(
            "{} resolves to {} which is outside root {}",
            candidate.display(),
            canonical_candidate.display(),
            canonical_root.display()
        )));
    }
    Ok(canonical_candidate)
}

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

    /// Path of the auto-discovered `max_tokens` table. Mirrors the
    /// `<root>/api_keys.toml` convention so config-style files
    /// stay co-located. The file is read once at startup by
    /// [`crate::llm::probe_table::MaxTokensTable::from_home`] and
    /// re-written when the probe discovers a new value.
    pub fn max_tokens_auto_path(&self) -> PathBuf {
        self.root.join("max_tokens_auto.toml")
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
    use crate::TEST_MOAGAN_HOME_LOCK;

    #[test]
    fn home_resolves_to_data_dir() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
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

    // -- D.29.1 — `safe_path` helper ----------------------------------

    /// Absolute path that lives under `root` resolves to its
    /// canonical form. The helper must not block legitimate
    /// in-tree usage.
    #[cfg(unix)]
    #[test]
    fn safe_path_accepts_in_tree_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("inside/file.json");
        std::fs::create_dir_all(tmp.path().join("inside")).unwrap();
        std::fs::write(&nested, b"{}").unwrap();
        let resolved = safe_path(tmp.path(), &nested).unwrap();
        assert_eq!(resolved, nested.canonicalize().unwrap());
    }

    /// Relative path that stays under `root` resolves correctly.
    /// Mirrors the natural CLI usage `moagan run --brief
    /// briefs/foo.json` where the candidate is supplied relative
    /// to the working directory or home.
    #[cfg(unix)]
    #[test]
    fn safe_path_accepts_relative_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/inside.md"), b"# ok").unwrap();
        let resolved = safe_path(tmp.path(), Path::new("sub/inside.md")).unwrap();
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    /// `../../etc/passwd` as a relative candidate must be
    /// rejected by the lexical `..` check even before
    /// canonicalisation runs. The error is `Error::PathTraversal`.
    #[test]
    fn safe_path_rejects_dotdot_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let err = safe_path(tmp.path(), Path::new("../../etc/passwd")).unwrap_err();
        assert!(
            matches!(err, Error::PathTraversal(_)),
            "expected PathTraversal, got {err:?}"
        );
    }

    /// Absolute `/etc/passwd` outside the root is rejected by
    /// the canonicalisation + `starts_with` check.
    #[cfg(unix)]
    #[test]
    fn safe_path_rejects_absolute_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let err = safe_path(tmp.path(), Path::new("/etc/passwd")).unwrap_err();
        assert!(
            matches!(err, Error::PathTraversal(_)),
            "expected PathTraversal, got {err:?}"
        );
    }

    /// `~/sensitive` style paths expand to the user's home and
    /// sit outside the configured root. The lexical check does
    /// not fire (no `..`) so the canonicalisation check is the
    /// line of defence.
    #[cfg(unix)]
    #[test]
    fn safe_path_rejects_tilde_escape() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a candidate that resolves outside `tmp` but does
        // not contain `..` segments. `..` would be caught by the
        // lexical check; this one exercises the
        // canonicalisation+`starts_with` branch.
        let sibling = tmp.path().parent().unwrap().join(format!(
            "moagan-tilde-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        let victim = sibling.join("secret.txt");
        std::fs::write(&victim, b"top secret").unwrap();

        let err = safe_path(tmp.path(), &victim).unwrap_err();
        assert!(
            matches!(err, Error::PathTraversal(_)),
            "expected PathTraversal, got {err:?}"
        );

        std::fs::remove_dir_all(&sibling).ok();
    }

    /// A symlink whose target sits outside `root` is rejected.
    /// `canonicalize` follows the link and exposes the
    /// out-of-tree destination, so the `starts_with` check fires.
    #[cfg(unix)]
    #[test]
    fn safe_path_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let victim_dir = tmp.path().parent().unwrap().join(format!(
            "moagan-symlink-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&victim_dir).unwrap();
        let victim = victim_dir.join("secret.txt");
        std::fs::write(&victim, b"x").unwrap();

        std::fs::create_dir_all(tmp.path().join("inside")).unwrap();
        std::os::unix::fs::symlink(&victim, tmp.path().join("inside/poison")).unwrap();

        let err = safe_path(tmp.path(), Path::new("inside/poison")).unwrap_err();
        assert!(
            matches!(err, Error::PathTraversal(_)),
            "expected PathTraversal, got {err:?}"
        );

        std::fs::remove_dir_all(&victim_dir).ok();
    }

    /// A symlink whose target stays inside `root` is accepted.
    #[cfg(unix)]
    #[test]
    fn safe_path_accepts_symlink_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("real/data.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("real/data.json"),
            tmp.path().join("data.json"),
        )
        .unwrap();

        let resolved = safe_path(tmp.path(), Path::new("data.json")).unwrap();
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    /// `Error::PathTraversal` maps to `ErrorCode::InvalidArgs`
    /// and `ExitCode::InvalidArgs` (exit 2) so CI scripts can
    /// branch on the conventional "bad input" exit code.
    #[test]
    fn path_traversal_error_maps_to_invalid_args() {
        use crate::ExitCode;
        use crate::error_code::ErrorCode;
        let err = Error::PathTraversal("../../etc/passwd".into());
        assert_eq!(err.code(), ErrorCode::InvalidArgs);
        assert_eq!(err.exit_code() as i32, ExitCode::InvalidArgs as i32);
    }
}
