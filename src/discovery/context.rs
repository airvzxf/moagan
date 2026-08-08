//! D.13.5: composite discovery context injected into the
//! extract/integrate phases.
//!
//! `DiscoveryContext` is the typed bundle every downstream
//! discovery phase reads when it needs to know which
//! sketches / contradictions / facets the upstream passes
//! produced, plus the hashes that anchor the run to a stable
//! brief + matrix pair, plus the tagger similarity threshold the
//! cluster pass applied.
//!
//! The struct is intentionally minimal: it is *not* the source
//! of truth for any of these artefacts (those live in
//! `sketches/`, `contradictions/`, `facets/`), only a
//! reproducible pointer to them. Resume loads it back from
//! `discovery_context.json` so a re-run can verify the upstream
//! surface stayed consistent.
//!
//! The on-disk layout is `<run_dir>/discovery_context.json` (one
//! file per run). Missing artefacts yield empty vectors; missing
//! brief / matrix files yield empty hashes — the helpers never
//! abort on a partial state so a partially-persisted run can
//! still build a context sidecar.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fs_layout::RunDir;
use crate::phases::util::{read_json, write_json};

use super::id::{ContradictionId, FacetId, SketchId};

/// D.13.5: composite context injected into the discovery
/// extract/integrate phases. Bundles the ids of every artefact
/// the downstream phase needs to reference (sketches,
/// contradictions, facets) plus the hashes that anchor the run
/// to a stable brief/matrix pair, plus the tagger threshold the
/// upstream cluster pass used.
///
/// `Serialize`/`Deserialize` use the default `serde_json`
/// representation; the struct is `#[serde(default)]` so legacy
/// readers can decode a future expansion without losing data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryContext {
    /// Sketch ids that fed into the matrix (`sk_<NN>`).
    pub sketch_ids: Vec<SketchId>,
    /// Contradiction ids emitted by `discover_contradict`
    /// (`c_<NN>`). Empty when no contradictions were detected.
    pub contradiction_ids: Vec<ContradictionId>,
    /// Facet ids across every facet list. The format is
    /// `<category_id>:<facet_id>` so a single string can be
    /// matched across clusters without losing its cluster
    /// provenance.
    pub facet_ids: Vec<FacetId>,
    /// BLAKE3 hash of the brief (`<run_dir>/brief.json`),
    /// lowercase hex. Empty string when the brief file was
    /// missing at write time.
    pub brief_hash: String,
    /// BLAKE3 hash of the exploration matrix
    /// (`<run_dir>/exploration_matrix.json`), lowercase hex.
    /// Empty string when the matrix file was missing at write
    /// time.
    pub matrix_hash: String,
    /// Tagger similarity threshold (0..=1) used by
    /// `discover_tag`. Falls back to
    /// [`crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD`]
    /// (`0.6`) when no config value is supplied.
    pub tagger_threshold: f32,
}

/// File name of the sidecar persisted under the run root.
pub const DISCOVERY_CONTEXT_FILENAME: &str = "discovery_context.json";

impl DiscoveryContext {
    /// Build a `DiscoveryContext` by scanning the on-disk
    /// artefacts in `run_dir`. Reads `<run_dir>/brief.json`,
    /// `<run_dir>/exploration_matrix.json`, `<run_dir>/sketches/`,
    /// `<run_dir>/contradictions/`, and `<run_dir>/facets/` to
    /// populate the corresponding fields. Missing artefacts
    /// yield empty vectors / empty hashes; the function never
    /// aborts on a partial state.
    ///
    /// The `tagger_threshold` falls back to
    /// [`crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD`]
    /// when the caller does not pass an explicit value.
    pub fn build(run_dir: &RunDir<'_>) -> Self {
        Self::build_with_threshold(run_dir, None)
    }

    /// Like [`DiscoveryContext::build`] but with an explicit
    /// `tagger_threshold`. `None` falls back to the default.
    pub fn build_with_threshold(run_dir: &RunDir<'_>, tagger_threshold: Option<f32>) -> Self {
        let root = run_dir.root();
        let brief_hash = read_blake3_hex(&root.join("brief.json"));
        let matrix_hash = read_blake3_hex(&root.join("exploration_matrix.json"));
        let sketch_ids = scan_id_dir(&run_dir.sketches(), |stem| SketchId(stem.to_string()));
        let contradiction_ids = collect_contradiction_ids(&run_dir.contradictions());
        let facet_ids = collect_facet_ids(&run_dir.facets());
        let threshold = tagger_threshold
            .filter(|v| (0.0..=1.0).contains(v))
            .unwrap_or(crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD);
        Self {
            sketch_ids,
            contradiction_ids,
            facet_ids,
            brief_hash,
            matrix_hash,
            tagger_threshold: threshold,
        }
    }

    /// Path to the sidecar (`<run_dir>/discovery_context.json`).
    pub fn path(run_dir: &RunDir<'_>) -> PathBuf {
        run_dir.root().join(DISCOVERY_CONTEXT_FILENAME)
    }

    /// Persist the context to `<run_dir>/discovery_context.json`.
    /// Returns the path it was written to. Idempotent; a
    /// subsequent call overwrites the file with the latest
    /// snapshot.
    pub fn persist(&self, run_dir: &RunDir<'_>) -> Result<PathBuf> {
        let path = Self::path(run_dir);
        write_json(&path, self)?;
        Ok(path)
    }

    /// Load the context from `<run_dir>/discovery_context.json`.
    /// Returns `Ok(None)` when the sidecar is absent so a
    /// fresh run can carry on without one; returns
    /// `Err(Error::InvalidState)` when the file is present but
    /// malformed (corruption that resume must surface, not
    /// silently drop).
    pub fn load(run_dir: &RunDir<'_>) -> Result<Option<Self>> {
        let path = Self::path(run_dir);
        if !path.exists() {
            return Ok(None);
        }
        match read_json::<Self>(&path) {
            Ok(c) => Ok(Some(c)),
            Err(e) => Err(Error::InvalidState(format!(
                "discovery_context.json malformed: {e}"
            ))),
        }
    }
}

/// BLAKE3 a file's bytes (lowercase hex). Returns the empty
/// string when the file is absent or unreadable so the sidecar
/// stays populated even when an upstream artefact was not
/// persisted yet.
fn read_blake3_hex(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    blake3::hash(&bytes).to_hex().to_string()
}

/// Walk a directory of `*.json` files (skipping `.meta.json`
/// sidecars) and return one id per file using the provided
/// constructor. Skips non-`json` extensions and metadata sidecars
/// so the helper matches the same conventions the discovery
/// phases use elsewhere. Returns an empty `Vec` when the
/// directory is absent.
fn scan_id_dir<T, F>(dir: &Path, ctor: F) -> Vec<T>
where
    F: Fn(&str) -> T,
{
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<T> = Vec::new();
    let mut stems: Vec<String> = read
        .filter_map(|r| r.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                return None;
            }
            if p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(".meta.json"))
            {
                return None;
            }
            p.file_stem().and_then(|s| s.to_str()).map(String::from)
        })
        .collect();
    stems.sort();
    for stem in stems {
        out.push(ctor(&stem));
    }
    out
}

/// Walk every `*.json` file under the contradictions directory
/// and collect one [`ContradictionId`] per `id` field in each
/// decoded `Contradiction`. Contradictions live inside a single
/// `contradictions.json` file (a JSON array) rather than as
/// per-id files, so we have to parse the payload to recover the
/// ids. Files that fail to decode are silently skipped so a
/// partially-written run still produces a populated context.
fn collect_contradiction_ids(dir: &Path) -> Vec<ContradictionId> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && !p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with(".meta.json"))
        })
        .collect();
    paths.sort();
    let mut out: Vec<ContradictionId> = Vec::new();
    for p in paths {
        let Ok(records) = read_json::<Vec<crate::domain::Contradiction>>(&p) else {
            continue;
        };
        for c in records {
            if !c.id.is_empty() {
                out.push(ContradictionId(c.id));
            }
        }
    }
    out
}

/// Walk every `<facets>/<file>.json`, decode it as
/// `FacetList`, and emit one [`FacetId`] per inner `Facet`. The
/// format is `<category_id>:<facet_id>` so the downstream
/// phases can disambiguate facets with the same slug across
/// clusters. Facet files that fail to decode are silently
/// skipped (the cluster pass may have emitted a malformed one
/// and we do not want the context sidecar to fail the whole
/// phase).
fn collect_facet_ids(dir: &Path) -> Vec<FacetId> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<FacetId> = Vec::new();
    let mut paths: Vec<PathBuf> = read
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some("json")
                && !p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with(".meta.json"))
        })
        .collect();
    paths.sort();
    for p in paths {
        let Ok(list) = read_json::<crate::domain::FacetList>(&p) else {
            continue;
        };
        for facet in &list.facets {
            out.push(FacetId(format!("{}:{}", list.category_id, facet.id)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_moagan_home;

    /// Schema version persisted alongside the sidecar so
    /// future readers can detect a breaking change without
    /// diffing the field list.
    pub(super) const SCHEMA_VERSION: &str = "v1";

    fn round_trip_value() -> DiscoveryContext {
        DiscoveryContext {
            sketch_ids: vec![SketchId::new("sk_001"), SketchId::new("sk_002")],
            contradiction_ids: vec![ContradictionId::new("c_01")],
            facet_ids: vec![
                FacetId::new("cat_01:data-flows"),
                FacetId::new("cat_01:error-handling"),
                FacetId::new("cat_02:trade-offs"),
            ],
            brief_hash: "a".repeat(64),
            matrix_hash: "b".repeat(64),
            tagger_threshold: 0.42,
        }
    }

    #[test]
    fn discovery_context_round_trips_through_json() {
        let original = round_trip_value();
        let json = serde_json::to_string(&original).expect("serialise");
        // Bare-string transparency for the id lists (Vec<String>-ish).
        assert!(json.contains("\"sk_001\""));
        assert!(json.contains("\"cat_01:data-flows\""));
        let restored: DiscoveryContext = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(restored, original);
    }

    #[test]
    fn discovery_context_default_is_empty() {
        let c = DiscoveryContext::default();
        assert!(c.sketch_ids.is_empty());
        assert!(c.contradiction_ids.is_empty());
        assert!(c.facet_ids.is_empty());
        assert_eq!(c.brief_hash, "");
        assert_eq!(c.matrix_hash, "");
        assert!((c.tagger_threshold - 0.0).abs() < 1e-6);
    }

    #[test]
    fn discovery_context_persist_load_round_trip() {
        with_moagan_home("discovery_context_persist_load", |home| {
            unsafe {
                std::env::set_var("MOAGAN_HOME", home);
            }
            let home_arc =
                std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().expect("resolve home"));
            let run_id = crate::ids::RunId::new();
            let run_dir = home_arc.run_dir(run_id);
            run_dir.ensure().expect("ensure run dir");

            let ctx = round_trip_value();
            let path = ctx.persist(&run_dir).expect("persist");
            assert!(path.exists(), "sidecar must exist after persist");

            let loaded = DiscoveryContext::load(&run_dir)
                .expect("load ok")
                .expect("sidecar present");
            assert_eq!(loaded, ctx);
        });
    }

    #[test]
    fn discovery_context_load_returns_none_when_absent() {
        with_moagan_home("discovery_context_load_absent", |home| {
            unsafe {
                std::env::set_var("MOAGAN_HOME", home);
            }
            let home_arc =
                std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().expect("resolve home"));
            let run_dir = home_arc.run_dir(crate::ids::RunId::new());
            run_dir.ensure().expect("ensure run dir");
            let loaded = DiscoveryContext::load(&run_dir).expect("load ok");
            assert!(loaded.is_none(), "missing sidecar must yield None");
        });
    }

    #[test]
    fn discovery_context_build_scans_run_dir() {
        with_moagan_home("discovery_context_build", |home| {
            unsafe {
                std::env::set_var("MOAGAN_HOME", home);
            }
            let home_arc =
                std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().expect("resolve home"));
            let run_id = crate::ids::RunId::new();
            let run_dir = home_arc.run_dir(run_id);
            run_dir.ensure().expect("ensure run dir");
            // `RunDir::ensure` does not create `sketches/` (the
            // matrix phase owns that directory). Make it explicit
            // so the test can drop sketch artefacts into it.
            std::fs::create_dir_all(run_dir.sketches()).expect("sketches dir");

            // Drop a brief, a matrix, two sketches, a contradictions
            // bundle, and one facet list so `build` has data to
            // discover. The facet list contains two facets to
            // confirm the per-facet expansion.
            let brief = serde_json::json!({"topic": "auth", "summary": "test"});
            std::fs::write(run_dir.brief(), serde_json::to_vec(&brief).unwrap()).unwrap();
            let matrix = serde_json::json!({"cardinality": 80, "cells": 8});
            std::fs::write(
                run_dir.root().join("exploration_matrix.json"),
                serde_json::to_vec(&matrix).unwrap(),
            )
            .unwrap();
            std::fs::write(
                run_dir.sketches().join("sk_001.json"),
                serde_json::to_vec(&serde_json::json!({"id": "sk_001"})).unwrap(),
            )
            .unwrap();
            std::fs::write(
                run_dir.sketches().join("sk_002.json"),
                serde_json::to_vec(&serde_json::json!({"id": "sk_002"})).unwrap(),
            )
            .unwrap();
            let contra = serde_json::json!([{"id": "c_01", "severity": "high"}]);
            std::fs::write(
                run_dir.contradictions().join("contradictions.json"),
                serde_json::to_vec(&contra).unwrap(),
            )
            .unwrap();
            let facet_list = serde_json::json!({
                "category_id": "cat_01",
                "cluster_id": "cluster_01",
                "facets": [
                    {"id": "data-flows", "description": "", "required": true},
                    {"id": "error-handling", "description": "", "required": false},
                ],
                "cache_key": "",
                "created_unix": 0,
                "schema_version": "v1",
            });
            std::fs::write(
                run_dir.facets().join("cat_01.json"),
                serde_json::to_vec(&facet_list).unwrap(),
            )
            .unwrap();

            let ctx = DiscoveryContext::build_with_threshold(&run_dir, Some(0.55));
            assert_eq!(
                ctx.sketch_ids,
                vec![SketchId::new("sk_001"), SketchId::new("sk_002")]
            );
            assert_eq!(ctx.contradiction_ids, vec![ContradictionId::new("c_01")]);
            assert_eq!(
                ctx.facet_ids,
                vec![
                    FacetId::new("cat_01:data-flows"),
                    FacetId::new("cat_01:error-handling"),
                ]
            );
            assert_eq!(ctx.brief_hash.len(), 64);
            assert_eq!(ctx.matrix_hash.len(), 64);
            assert!((ctx.tagger_threshold - 0.55).abs() < 1e-6);
        });
    }

    #[test]
    fn discovery_context_build_handles_missing_artefacts() {
        with_moagan_home("discovery_context_missing", |home| {
            unsafe {
                std::env::set_var("MOAGAN_HOME", home);
            }
            let home_arc =
                std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().expect("resolve home"));
            let run_dir = home_arc.run_dir(crate::ids::RunId::new());
            run_dir.ensure().expect("ensure run dir");
            let ctx = DiscoveryContext::build(&run_dir);
            assert!(ctx.sketch_ids.is_empty());
            assert!(ctx.contradiction_ids.is_empty());
            assert!(ctx.facet_ids.is_empty());
            assert_eq!(ctx.brief_hash, "");
            assert_eq!(ctx.matrix_hash, "");
            assert!(
                (ctx.tagger_threshold
                    - crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD)
                    .abs()
                    < 1e-6
            );
        });
    }

    #[test]
    fn discovery_context_build_falls_back_when_threshold_out_of_range() {
        with_moagan_home("discovery_context_threshold", |home| {
            unsafe {
                std::env::set_var("MOAGAN_HOME", home);
            }
            let home_arc =
                std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().expect("resolve home"));
            let run_dir = home_arc.run_dir(crate::ids::RunId::new());
            run_dir.ensure().expect("ensure run dir");
            let ctx = DiscoveryContext::build_with_threshold(&run_dir, Some(2.5));
            assert!(
                (ctx.tagger_threshold
                    - crate::discovery::tagger_threshold::DEFAULT_TAGGER_THRESHOLD)
                    .abs()
                    < 1e-6
            );
        });
    }

    #[test]
    fn discovery_context_path_is_run_root() {
        with_moagan_home("discovery_context_path", |home| {
            unsafe {
                std::env::set_var("MOAGAN_HOME", home);
            }
            let home_arc =
                std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().expect("resolve home"));
            let run_dir = home_arc.run_dir(crate::ids::RunId::new());
            let path = DiscoveryContext::path(&run_dir);
            assert!(path.ends_with(DISCOVERY_CONTEXT_FILENAME));
            assert_eq!(path.parent(), Some(run_dir.root()));
        });
    }

    #[test]
    fn schema_version_constant_is_stable() {
        // Documented here so a future bump forces a deliberate
        // edit instead of an accidental drift.
        assert_eq!(SCHEMA_VERSION, "v1");
    }
}
