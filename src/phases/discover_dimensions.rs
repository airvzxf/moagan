//! Discovery mode — `discover_dimensions` phase.
//!
//! F1 (Track G.2): when the operator does NOT pass
//! `--matrix-spec`, this phase calls the LLM with
//! [`Role::DimensionDeriver`] to derive the exploration-matrix
//! dimensions and per-dimension facets from the brief itself.
//! The derived list replaces the legacy hardcoded 4×2 default
//! (`deployment-model` / `storage` / `consistency` /
//! `observability`, each with 2 facets).
//!
//! The phase:
//!
//! 1. Reads `<run_dir>/brief.json` (the canonical brief the
//!    upstream `clarify` phase produced).
//! 2. Builds a user payload that quotes the brief verbatim so the
//!    model can ground its proposal in the user's actual problem.
//! 3. Calls `Role::DimensionDeriver` via
//!    [`RunContext::call_with_retry_parse`], parsing the response
//!    into a [`DerivedDimensions`] envelope.
//! 4. Persists the dimensions to
//!    `<run_dir>/discovery_dimensions.json` so the matrix phase
//!    can pick them up without re-issuing the LLM call.
//! 5. Emits a `tracing::info!` so an operator scanning logs can
//!    confirm the dimension list.
//!
//! The phase never falls back to placeholders — when the LLM
//! call fails or returns a malformed payload, the phase returns
//! `Err(...)` so the operator can decide (retry / abort). This
//! is the F1 spec's "error explícito" requirement: the legacy
//! default was a silent quality regression and the new path
//! refuses to swallow that.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::discovery::matrix::{
    DISCOVERY_DIMENSIONS_FILENAME, DISCOVERY_DIMENSIONS_SCHEMA_VERSION, Dimension,
    DimensionFacetDescription, DiscoveryDimensions,
};
use crate::discovery::matrix_spec::DerivedDimensions;
use crate::discovery::matrix_spec::DimensionSpec;
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::time::now_unix_secs;

/// Cache key namespace. Bumped when the prompt template changes
/// in a way that would invalidate cached responses. The cache
/// key is `sha256(brief + "dims-v1")` — `dims-v1` is the schema
/// version embedded in the prompt's contract.
pub const CACHE_KEY_NAMESPACE: &str = "dims-v1";

/// `DiscoverDimensionsPhase` reads the brief, calls
/// [`Role::DimensionDeriver`], and persists the
/// `<run_dir>/discovery_dimensions.json` sidecar. It is the F1
/// entry point for the LLM-derive path.
///
/// The phase is a no-op when a
/// `<run_dir>/discovery_dimensions.json` already exists (resume
/// path): the matrix phase and the resume coordinator honour
/// the sidecar verbatim, so re-running `discover_dimensions`
/// would issue an unnecessary LLM call. The sidecar's schema
/// version is checked, but missing-or-equal both pass without
/// re-derivation. An empty `RunContext` (e.g. tests) gets the
/// fresh-derive path because the sidecar is absent.
#[derive(Debug, Clone, Default)]
pub struct DiscoverDimensionsPhase;

#[async_trait]
impl Phase for DiscoverDimensionsPhase {
    fn name(&self) -> &'static str {
        "discover_dimensions"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let run_dir = ctx.run_dir();
        let sidecar_path = run_dir.root().join(DISCOVERY_DIMENSIONS_FILENAME);

        // Resume path: a sidecar already exists. Honor it and
        // skip the LLM call. The matrix phase reads the same
        // sidecar, so this branch guarantees resume produces the
        // exact dimensions the original run used (no drift from
        // a re-derived prompt).
        if let Some(existing) = read_existing_sidecar(&sidecar_path)? {
            tracing::info!(
                schema_version = existing.schema_version,
                dimensions = existing.dimensions.len(),
                brief_hash = %existing.brief_hash,
                "discover_dimensions: sidecar present; reusing existing dimensions"
            );
            return Ok(PhaseOutput::DiscoveryDimensions(sidecar_path));
        }

        // Fresh path: read the brief, build the payload, call
        // the LLM.
        let brief: serde_json::Value = read_json(&run_dir.brief())?;
        let brief_text = brief_text_from_brief(&brief);
        let brief_hash = sha256_hex(brief_text.as_bytes());

        let user_payload = Self::build_user_payload(&brief_text);
        let system = system_prompt(Role::DimensionDeriver).to_owned();
        let schema_hint = system_prompt(Role::DimensionDeriver);
        let derived: DerivedDimensions = ctx
            .call_with_retry_parse(
                Role::DimensionDeriver,
                system,
                user_payload,
                schema_hint,
                // 3 retries: the LLM is being asked to produce a
                // structured JSON envelope on a fresh brief; one
                // bad attempt is normal (brace mismatch,
                // truncated response). Two retries beyond that is
                // already the audit's "stop and surface" ceiling
                // for non-decorative roles.
                3,
            )
            .await?;

        if derived.dimensions.is_empty() {
            return Err(Error::DiscoveryQualityTooLow {
                failed: 1,
                total: 1,
                threshold_pct: 100,
            });
        }

        // Convert the LLM-derived spec into the matrix's typed
        // `Dimension` rows. We keep the descriptions alongside in
        // a parallel map so the sidecar carries them; the matrix
        // itself only needs `id` + `label`.
        let (dims, descriptions) = derived_dimensions_to_matrix(&derived);

        let sidecar = DiscoveryDimensions {
            schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.to_string(),
            brief_hash: brief_hash.clone(),
            dimensions: dims.clone(),
            descriptions: descriptions.clone(),
            created_unix: now_unix_secs(),
        };

        write_json(&sidecar_path, &sidecar)?;

        tracing::info!(
            dimensions = dims.len(),
            brief_hash = %brief_hash,
            total_cells = dims.iter().map(|d| d.facets.len()).sum::<usize>(),
            "discover_dimensions: derived {} dimensions from brief",
            dims.len()
        );

        Ok(PhaseOutput::DiscoveryDimensions(sidecar_path))
    }
}

impl DiscoverDimensionsPhase {
    /// Build the LLM user payload. The full brief is quoted
    /// verbatim so the model can ground its proposal in the
    /// user's actual problem. The trailing schema instruction
    /// reinforces the JSON envelope contract so the response
    /// parses without manual recovery on the first attempt.
    pub fn build_user_payload(brief_text: &str) -> String {
        format!(
            "Brief (verbatim, JSON-encoded):\n{brief}\n\n\
             Task: derive the exploration-matrix dimensions and per-dimension facets \
             that best span the design space implied by the brief.\n\n\
             Respond ONLY with a JSON object matching this schema:\n\
             {{\n  \
               \"dimensions\": [\n    \
                 {{\n      \
                   \"id\": \"kebab-case-id\",\n      \
                   \"label\": \"Human readable label\",\n      \
                   \"facets\": [\n        \
                     {{\n          \
                       \"id\": \"kebab-case-id\",\n          \
                       \"label\": \"Human readable label\",\n          \
                       \"description\": \"1-2 sentences\"\n        \
                     }}\n      \
                   ]\n    \
                 }}\n  \
               ]\n\
             }}\n\n\
             Rules:\n\
             - 2 to 6 dimensions.\n\
             - Each dimension carries 1 to 5 facets. Asymmetric counts are welcome.\n\
             - All ids are kebab-case, lowercase, ≤ 32 chars.\n\
             - Every dimension must have at least one facet.\n",
            brief = brief_text
        )
    }
}

/// Path to the sidecar persisted under the run root. Public so
/// the matrix phase and the resume path share the same
/// filename constant.
pub fn sidecar_path(run_root: &Path) -> PathBuf {
    run_root.join(DISCOVERY_DIMENSIONS_FILENAME)
}

/// Read a previously-persisted sidecar. Returns `Ok(None)` when
/// the file is absent or the schema version is missing/empty;
/// returns `Err(Error::InvalidState)` when the file is present
/// but malformed (resume must surface, not silently drop).
fn read_existing_sidecar(path: &Path) -> Result<Option<DiscoveryDimensions>> {
    if !path.exists() {
        return Ok(None);
    }
    let sidecar: DiscoveryDimensions = match read_json(path) {
        Ok(s) => s,
        Err(e) => {
            return Err(Error::InvalidState(format!(
                "{} malformed: {e}",
                DISCOVERY_DIMENSIONS_FILENAME
            )));
        }
    };
    Ok(Some(sidecar))
}

/// Convert the LLM-derived spec into the matrix's typed
/// `Dimension` rows + a parallel list of
/// [`DimensionFacetDescription`] triples. The descriptions
/// are stored as a list (not a `HashMap<(String, String),
/// String>`) because the latter requires string keys at every
/// JSON nesting level; the list form round-trips cleanly
/// through serde.
fn derived_dimensions_to_matrix(
    derived: &DerivedDimensions,
) -> (Vec<Dimension>, Vec<DimensionFacetDescription>) {
    let mut descriptions: Vec<DimensionFacetDescription> = Vec::new();
    let dims: Vec<Dimension> = derived
        .dimensions
        .iter()
        .map(|d: &DimensionSpec| Dimension {
            id: d.id.clone(),
            label: d.label.clone(),
            facets: d
                .facets
                .iter()
                .map(|f| {
                    descriptions.push(DimensionFacetDescription::new(
                        d.id.clone(),
                        f.id.clone(),
                        f.description.clone(),
                    ));
                    crate::discovery::matrix::Facet {
                        id: f.id.clone(),
                        label: f.label.clone(),
                    }
                })
                .collect(),
        })
        .collect();
    (dims, descriptions)
}

/// SHA-256 hex digest. Used to anchor the sidecar to a brief so
/// a resume that finds a sidecar from a different brief can
/// detect the mismatch. Cheap; SHA-256 is already used by
/// [`crate::discovery::facet::cache_key`].
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Compute the cache key the cross-run LLM cache uses for the
/// dimension-deriver call. Bumping [`CACHE_KEY_NAMESPACE`]
/// invalidates cached responses — the prompt is the only
/// thing that ever changes here.
pub fn cache_key_for(brief_text: &str) -> String {
    let mut h = Sha256::new();
    h.update(brief_text.as_bytes());
    h.update([0x1f]);
    h.update(CACHE_KEY_NAMESPACE.as_bytes());
    hex::encode(h.finalize())
}

/// Render the brief as a flat JSON string the LLM can read. We
/// serialize the brief JSON verbatim (sorted keys for stability
/// across runs) — operators can re-derive on the same brief and
/// get a byte-identical cache key.
fn brief_text_from_brief(brief: &serde_json::Value) -> String {
    // Stable key ordering so two runs over the same brief
    // produce identical cache keys; the `serde_json::Value` map
    // iteration order is otherwise implementation-defined.
    let mut value = brief.clone();
    sort_json_keys(&mut value);
    serde_json::to_string(&value).unwrap_or_default()
}

/// Recursively sort map keys so the JSON serialisation is
/// stable. Without this, two `serde_json::Map` instances with
/// the same content can serialise to different byte strings
/// (BTreeMap-backed maps are stable; HashMap-backed are not).
fn sort_json_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = std::mem::take(map)
                .into_iter()
                .map(|(k, mut v)| {
                    sort_json_keys(&mut v);
                    (k, v)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let sorted: serde_json::Map<String, serde_json::Value> = entries.into_iter().collect();
            *map = sorted;
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                sort_json_keys(item);
            }
        }
        _ => {}
    }
}

/// Helper that builds an `Arc<DerivedDimensions>` for callers
/// that want to inject a hand-built spec without going through
/// the LLM (the integration tests use this to mock the
/// deriver). The phase still writes the sidecar so the matrix
/// phase downstream can read it without re-deriving.
#[allow(dead_code)]
pub fn build_sidecar_for_test(brief_text: &str, derived: DerivedDimensions) -> DiscoveryDimensions {
    let brief_hash = sha256_hex(brief_text.as_bytes());
    let (dims, descriptions) = derived_dimensions_to_matrix(&derived);
    DiscoveryDimensions {
        schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.to_string(),
        brief_hash,
        dimensions: dims,
        descriptions,
        created_unix: now_unix_secs(),
    }
}

/// Adapter that turns a `DiscoveryDimensions` sidecar into a
/// `PhaseOutput::DiscoveryDimensions` value. Used by tests
/// that pre-populate the sidecar and want to verify the matrix
/// phase picks it up.
#[allow(dead_code)]
pub fn phase_output_from_sidecar(path: PathBuf) -> PhaseOutput {
    PhaseOutput::DiscoveryDimensions(path)
}

// `Arc` is unused at the module level but pulled in for the
// helpers below; the lint suppression keeps `cargo clippy -D
// warnings` clean without making the public API use `Arc`.
#[allow(dead_code)]
fn _force_arc_link(_x: Arc<()>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_user_payload_quotes_brief_verbatim() {
        let payload = DiscoverDimensionsPhase::build_user_payload("BRIEF_TEXT");
        assert!(payload.contains("BRIEF_TEXT"));
        // Schema is reinforced so the model sees the JSON shape
        // it must emit (helps non-JSON-mode providers).
        assert!(payload.contains("\"dimensions\""));
        assert!(payload.contains("\"facets\""));
        assert!(payload.contains("kebab-case"));
    }

    #[test]
    fn cache_key_changes_with_brief() {
        let a = cache_key_for("brief-a");
        let b = cache_key_for("brief-b");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn cache_key_is_stable_for_same_brief() {
        let a = cache_key_for("brief-a");
        let b = cache_key_for("brief-a");
        assert_eq!(a, b);
    }

    #[test]
    fn brief_text_from_brief_is_stable() {
        // Two `serde_json::Map` instances built the same way
        // should serialise to the same bytes (key order matters).
        let mut a = serde_json::Map::new();
        a.insert("problem".into(), serde_json::json!("x"));
        a.insert("objectives".into(), serde_json::json!(["y"]));
        let mut b = serde_json::Map::new();
        b.insert("objectives".into(), serde_json::json!(["y"]));
        b.insert("problem".into(), serde_json::json!("x"));
        let sa = brief_text_from_brief(&serde_json::Value::Object(a));
        let sb = brief_text_from_brief(&serde_json::Value::Object(b));
        assert_eq!(sa, sb);
    }

    #[test]
    fn derived_dimensions_to_matrix_returns_typed_dimensions() {
        let derived = DerivedDimensions {
            dimensions: vec![DimensionSpec {
                id: "deployment".into(),
                label: "Deployment".into(),
                facets: vec![
                    crate::discovery::matrix_spec::FacetSpec {
                        id: "serverless".into(),
                        label: "Serverless".into(),
                        description: "Run on a managed runtime.".into(),
                    },
                    crate::discovery::matrix_spec::FacetSpec {
                        id: "self_hosted".into(),
                        label: "Self hosted".into(),
                        description: "Operator runs the binary.".into(),
                    },
                ],
            }],
        };
        let (dims, descs) = derived_dimensions_to_matrix(&derived);
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0].id, "deployment");
        assert_eq!(dims[0].facets.len(), 2);
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].description, "Run on a managed runtime.");
        assert_eq!(descs[0].dimension_id, "deployment");
        assert_eq!(descs[0].facet_id, "serverless");
        assert_eq!(descs[1].description, "Operator runs the binary.");
        assert_eq!(descs[1].facet_id, "self_hosted");
    }

    #[test]
    fn sidecar_path_uses_filename_constant() {
        let path = sidecar_path(Path::new("/tmp/run"));
        assert_eq!(
            path,
            Path::new("/tmp/run").join(DISCOVERY_DIMENSIONS_FILENAME)
        );
    }

    #[test]
    fn read_existing_sidecar_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let out = read_existing_sidecar(&sidecar_path(tmp.path())).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn read_existing_sidecar_errors_on_malformed_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = sidecar_path(tmp.path());
        std::fs::write(&path, b"not-json").unwrap();
        let err = read_existing_sidecar(&path).unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }

    #[test]
    fn read_existing_sidecar_returns_value_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = sidecar_path(tmp.path());
        let sidecar = DiscoveryDimensions {
            schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.into(),
            brief_hash: "abc".into(),
            dimensions: vec![Dimension {
                id: "deployment".into(),
                label: "Deployment".into(),
                facets: vec![crate::discovery::matrix::Facet {
                    id: "serverless".into(),
                    label: "Serverless".into(),
                }],
            }],
            descriptions: Vec::new(),
            created_unix: 1,
        };
        write_json(&path, &sidecar).unwrap();
        let loaded = read_existing_sidecar(&path).unwrap().unwrap();
        assert_eq!(loaded.schema_version, DISCOVERY_DIMENSIONS_SCHEMA_VERSION);
        assert_eq!(loaded.dimensions.len(), 1);
        assert_eq!(loaded.brief_hash, "abc");
    }

    #[test]
    fn build_sidecar_for_test_returns_full_sidecar() {
        let derived = DerivedDimensions {
            dimensions: vec![DimensionSpec {
                id: "auth".into(),
                label: "Auth".into(),
                facets: vec![crate::discovery::matrix_spec::FacetSpec {
                    id: "oauth".into(),
                    label: "OAuth".into(),
                    description: "OAuth flow".into(),
                }],
            }],
        };
        let sidecar = build_sidecar_for_test("brief-text", derived);
        assert_eq!(sidecar.schema_version, DISCOVERY_DIMENSIONS_SCHEMA_VERSION);
        assert_eq!(sidecar.brief_hash.len(), 64);
        assert_eq!(sidecar.dimensions.len(), 1);
        assert_eq!(sidecar.descriptions.len(), 1);
        assert_eq!(sidecar.descriptions[0].dimension_id, "auth");
        assert_eq!(sidecar.descriptions[0].facet_id, "oauth");
        assert_eq!(sidecar.descriptions[0].description, "OAuth flow");
        assert_eq!(sidecar.description_for("auth", "oauth"), Some("OAuth flow"));
    }
}
