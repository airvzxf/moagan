//! `ExplorationMatrix` — the discovery fan-out grid.
//!
//! Per V4 §6.4 and proposal-02-rust.md §9.1, the discovery matrix is
//! `dims × facets_per_dim × cells_per_facet`. Each cell produces one
//! sketch via the `discover_matrix` prompt. The matrix differs from
//! the angle-rotated sketch fan-out of the standard pipeline because
//! it samples the *space* systematically rather than rotating
//! through a fixed list of personas.
//!
//! F1 (Track G.2) refactor: the matrix no longer hardcodes its
//! dimensions. The new constructor
//! [`ExplorationMatrix::new`] takes the dimensions verbatim —
//! whether the operator supplied them via `--matrix-spec`, the
//! `discover_dimensions` LLM-derive phase derived them from the
//! brief, or a legacy code path built them programmatically.
//! [`ExplorationMatrix::load_or_derive`] is the resume-side helper:
//! load a previously-persisted `discovery_dimensions.json` if
//! present, otherwise build a new matrix from the supplied spec
//! (or empty fallback when the caller has no spec).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::phases::util::read_json;

use super::matrix_spec::MatrixSpec;

/// Per-provider sampling temperature profile (PR-D1, V4 §6.4
/// evolution).
///
/// A `TemperatureProfile` is the explicit knob an operator uses to
/// override the role-default sampling temperature for one provider's
/// matrix fan-out. The matrix phase reads `(temperatures,
/// replicas_per_temperature)` and fires `temperatures ×
/// replicas_per_temperature` LLM calls per `(cell, replica)` pair for
/// the named provider; every other provider in the same run keeps the
/// [`Self::default()`] single-shot contract so a profile that is not
/// configured for a provider produces a byte-identical request set to
/// the v0.5 behaviour.
///
/// The default (`[1.0] × 1`) is deliberate: the matrix phase's
/// pre-PR-D1 loop was `cells × sketches_per_cell`, one call per
/// `(cell, sketch_index)` pair at the role-default temperature
/// (`Role::Sketch` = `1.0`). Keeping `temperatures = vec![1.0]` and
/// `replicas_per_temperature = 1` reproduces that exact fan-out so
/// operators who never set a profile see no behavioural change.
///
/// Validation rules (enforced at the CLI spec-parser level so a
/// `Vec<f32>` parsed from TOML or clap never carries garbage):
///
/// * `temperatures` must be non-empty.
/// * Every temperature must be in `0.0..=2.0` (the documented LLM
///   sampling band — providers outside the band reject the request).
/// * `replicas_per_temperature` must be `>= 1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemperatureProfile {
    /// Sampling temperatures the matrix phase should iterate per
    /// `(cell, replica)` pair. Default `vec![1.0]` — one call at
    /// the role default.
    pub temperatures: Vec<f32>,
    /// How many LLM calls to fire per `(cell, temperature)` pair.
    /// Default `1` — same fan-out as v0.5.
    pub replicas_per_temperature: usize,
}

impl Default for TemperatureProfile {
    fn default() -> Self {
        Self {
            temperatures: vec![1.0],
            replicas_per_temperature: 1,
        }
    }
}

impl TemperatureProfile {
    /// Total iterations per cell = `temperatures.len() *
    /// replicas_per_temperature`. The matrix phase uses this to size
    /// the inner loop and to surface the per-provider expansion to
    /// `ExplorationMatrix::cardinality`.
    pub fn total(&self) -> usize {
        self.temperatures.len() * self.replicas_per_temperature.max(1)
    }
}

/// One dimension in the exploration matrix. A dimension is a
/// high-level axis of variation the user wants to explore (e.g.
/// "deployment model", "storage layer", "auth strategy").
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Dimension {
    /// Stable id (kebab-case).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Facets (the values the dimension can take).
    pub facets: Vec<Facet>,
}

/// One facet (value) of a dimension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Facet {
    /// Stable id (kebab-case).
    pub id: String,
    /// Human-readable label.
    pub label: String,
}

/// One cell in the matrix — a `(dimension × facet)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MatrixCell {
    /// Dimension id.
    pub dimension_id: String,
    /// Facet id.
    pub facet_id: String,
    /// Composite label for the prompt.
    pub label: String,
}

/// Sidecar filename under the run root that holds the LLM-derived
/// matrix dimensions. F1 / Track G.2 (`discover_dimensions`):
/// written by the new phase, read by the matrix phase and the
/// resume path.
pub const DISCOVERY_DIMENSIONS_FILENAME: &str = "discovery_dimensions.json";

/// Schema version stored alongside the dimensions sidecar so a
/// future bump is detectable without diffing field lists. Bump
/// when the wire format changes in a non-backward-compatible way.
pub const DISCOVERY_DIMENSIONS_SCHEMA_VERSION: &str = "dims-v1";

/// On-disk shape of `<run_dir>/discovery_dimensions.json`. The
/// matrix persists the brief-deriver's payload verbatim (minus
/// the schema metadata) so the resume path can recover the same
/// dimensions without re-issuing the LLM call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryDimensions {
    /// Schema version for forward-compat detection.
    pub schema_version: String,
    /// BLAKE3 hash of the brief that drove the derivation (so a
    /// resume can confirm the cached dimensions still match the
    /// current brief).
    pub brief_hash: String,
    /// Dimensions + facets the LLM derived.
    pub dimensions: Vec<Dimension>,
    /// Optional descriptions map keyed by `(dimension_id,
    /// facet_id)`. Populated when the LLM supplied descriptions
    /// (the dimension-deriver prompt asks for them); an empty map
    /// is acceptable. Serialised as a list of
    /// `[dimension_id, facet_id, description]` triples so a
    /// tuple-keyed `HashMap` round-trips through JSON (Rust's
    /// default tuple-key serde representation is a nested
    /// object that requires string keys at every level).
    #[serde(default)]
    pub descriptions: Vec<DimensionFacetDescription>,
    /// Unix timestamp when the sidecar was written.
    pub created_unix: i64,
}

/// One `(dimension_id, facet_id, description)` triple. The
/// wire-format for [`DiscoveryDimensions::descriptions`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DimensionFacetDescription {
    /// Dimension id.
    pub dimension_id: String,
    /// Facet id.
    pub facet_id: String,
    /// Free-text description the LLM supplied.
    pub description: String,
}

impl DimensionFacetDescription {
    /// Build a new triple.
    pub fn new(
        dimension_id: impl Into<String>,
        facet_id: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            dimension_id: dimension_id.into(),
            facet_id: facet_id.into(),
            description: description.into(),
        }
    }
}

impl DiscoveryDimensions {
    /// Look up the description for a `(dimension_id, facet_id)`
    /// pair. Returns `None` when the LLM did not supply one.
    pub fn description_for(&self, dim: &str, facet: &str) -> Option<&str> {
        self.descriptions
            .iter()
            .find(|d| d.dimension_id == dim && d.facet_id == facet)
            .map(|d| d.description.as_str())
    }
}

/// The full exploration matrix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplorationMatrix {
    /// Dimensions in declaration order.
    pub dimensions: Vec<Dimension>,
    /// Number of sketches to generate per cell. Default 10
    /// (4 dims × 2 facets × 10 = 80 sketches, the user's chosen
    /// floor). F1 surfaces this as `sketches_per_cell` so the
    /// matrix fan-out is decoupled from a hardcoded `cardinality`
    /// target; F2 renames the CLI flag accordingly.
    pub sketches_per_cell: usize,
    /// Per-provider sampling-temperature profiles keyed by the
    /// provider's MODEL name (e.g. `"MiniMax-M3"`, `"deepseek-v4-flash"`,
    /// `"mimo-v2.5"` — the same string stored on the `Request` /
    /// `ProviderConfig`). A provider not in this map uses
    /// [`Self::default_profile`]. Default empty so the matrix is
    /// bit-identical to the v0.5 behaviour when no profile is
    /// configured (PR-D1).
    #[serde(default)]
    pub temperature_profiles: HashMap<String, TemperatureProfile>,
    /// Default profile applied to providers absent from
    /// [`Self::temperature_profiles`]. Default
    /// [`TemperatureProfile::default()`] (`[1.0] × 1`) so the
    /// unconfigured case reproduces the v0.5 single-shot behaviour
    /// byte-for-byte.
    #[serde(default)]
    pub default_profile: TemperatureProfile,
}

impl ExplorationMatrix {
    /// Build a matrix from the supplied dimensions and a
    /// `sketches_per_cell` fan-out. The matrix owns the dimensions
    /// verbatim — no hardcoded defaults, no derivation. Callers
    /// that need to source the dimensions from an operator spec,
    /// the LLM deriver, or a sidecar reach this constructor via
    /// [`ExplorationMatrix::from_spec`],
    /// [`ExplorationMatrix::from_derived`], or
    /// [`ExplorationMatrix::load_or_derive`].
    pub fn new(dimensions: Vec<Dimension>, sketches_per_cell: usize) -> Self {
        let sketches_per_cell = sketches_per_cell.max(1);
        Self {
            dimensions,
            sketches_per_cell,
            temperature_profiles: HashMap::new(),
            default_profile: TemperatureProfile::default(),
        }
    }

    /// Build a matrix from an operator-supplied [`MatrixSpec`].
    /// The spec's dimensions are promoted verbatim; the
    /// `sketches_per_cell` parameter is the per-cell fan-out the
    /// CLI / TOML resolved (F2 will move the flag from
    /// `--cardinality` to `--sketches-per-cell`).
    pub fn from_spec(spec: MatrixSpec, sketches_per_cell: usize) -> Self {
        Self::new(spec.into_dimensions(), sketches_per_cell)
    }

    /// Build a matrix from LLM-derived dimensions. The phase
    /// already validated the brief-deriver's payload; this
    /// constructor just wraps the `Vec<Dimension>` with the
    /// operator-chosen fan-out.
    pub fn from_derived(dimensions: Vec<Dimension>, sketches_per_cell: usize) -> Self {
        Self::new(dimensions, sketches_per_cell)
    }

    /// Build a matrix from an existing
    /// [`DiscoveryDimensions`] sidecar (the resume path) plus the
    /// supplied fan-out. The constructor takes ownership so the
    /// caller doesn't have to clone the dimensions just to wrap
    /// them.
    pub fn from_sidecar(sidecar: DiscoveryDimensions, sketches_per_cell: usize) -> Self {
        Self::new(sidecar.dimensions, sketches_per_cell)
    }

    /// Load the dimensions sidecar from `<run_dir>/discovery_dimensions.json`
    /// and build the matrix around it. Returns `Ok(None)` when the
    /// sidecar is absent (a fresh run); returns
    /// `Err(Error::InvalidState)` when the sidecar is present but
    /// malformed (resume must surface, not silently drop).
    pub fn load_or_derive(run_dir: &Path, sketches_per_cell: usize) -> Result<Option<Self>> {
        let path = run_dir.join(DISCOVERY_DIMENSIONS_FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let sidecar: DiscoveryDimensions = match read_json(&path) {
            Ok(s) => s,
            Err(e) => {
                return Err(Error::InvalidState(format!(
                    "{} malformed: {e}",
                    DISCOVERY_DIMENSIONS_FILENAME
                )));
            }
        };
        Ok(Some(Self::from_sidecar(sidecar, sketches_per_cell)))
    }

    /// Resolve the temperature profile for a given provider model
    /// name. Falls back to [`Self::default_profile`] when the provider
    /// is not in [`Self::temperature_profiles`]. The lookup is
    /// case-sensitive on the model name string — operators should
    /// pass the exact `ProviderConfig::model` value (e.g.
    /// `"MiniMax-M3"`) when configuring the map.
    pub fn profile_for(&self, provider_model: &str) -> &TemperatureProfile {
        self.temperature_profiles
            .get(provider_model)
            .unwrap_or(&self.default_profile)
    }

    /// Total number of sketches the matrix will request against a
    /// single provider. The formula is `cells() * sketches_per_cell *
    /// profile.total()` for one provider. The matrix phase iterates
    /// per-provider, so callers that fan out across multiple
    /// providers should sum this across the active set; for the
    /// common single-provider case the value matches the v0.5
    /// `cells() * sketches_per_cell` (because the default profile is
    /// `[1.0] × 1`, which contributes a factor of 1).
    ///
    /// Note: this method returns the per-provider expansion factor —
    /// it does NOT add profiles across providers because the matrix
    /// does not own the provider list (the coordinator / CLI does).
    /// The discovery phase applies `profile_for(provider).total()`
    /// itself.
    pub fn cardinality(&self) -> usize {
        self.cells() * self.sketches_per_cell
    }

    /// Number of cells (one per `dimension × facet` pair). With
    /// F1's asymmetric facets the sum is per-dimension facet
    /// counts (NOT a Cartesian product — the dimension-deriver
    /// phase picks facet counts per dimension).
    pub fn cells(&self) -> usize {
        // Empty matrix has zero cells (no fan-out). A non-empty
        // matrix with a zero-facet dimension still counts that
        // dimension as one cell so the fan-out never silently
        // disappears.
        if self.dimensions.is_empty() {
            0
        } else {
            self.dimensions.iter().map(|d| d.facets.len().max(1)).sum()
        }
    }

    /// Iterate over every cell in declaration order.
    pub fn iter_cells(&self) -> impl Iterator<Item = MatrixCell> + '_ {
        self.dimensions.iter().flat_map(|d| {
            d.facets.iter().map(move |f| MatrixCell {
                dimension_id: d.id.clone(),
                facet_id: f.id.clone(),
                label: format!("{} / {}", d.label, f.label),
            })
        })
    }

    /// Look up a dimension by id.
    pub fn dimension(&self, id: &str) -> Option<&Dimension> {
        self.dimensions.iter().find(|d| d.id == id)
    }

    /// Tally of dimension → facet count, useful for logs.
    pub fn tally(&self) -> BTreeMap<String, usize> {
        self.dimensions
            .iter()
            .map(|d| (d.id.clone(), d.facets.len()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims_2_3() -> Vec<Dimension> {
        vec![
            Dimension {
                id: "deployment-model".into(),
                label: "Deployment model".into(),
                facets: vec![
                    Facet {
                        id: "serverless".into(),
                        label: "serverless".into(),
                    },
                    Facet {
                        id: "self-hosted".into(),
                        label: "self-hosted".into(),
                    },
                ],
            },
            Dimension {
                id: "storage".into(),
                label: "Storage strategy".into(),
                facets: vec![
                    Facet {
                        id: "sql".into(),
                        label: "SQL".into(),
                    },
                    Facet {
                        id: "kv".into(),
                        label: "embedded key-value".into(),
                    },
                    Facet {
                        id: "blob".into(),
                        label: "blob".into(),
                    },
                ],
            },
        ]
    }

    #[test]
    fn new_uses_supplied_dimensions() {
        let m = ExplorationMatrix::new(dims_2_3(), 10);
        assert_eq!(m.dimensions.len(), 2);
        assert_eq!(m.sketches_per_cell, 10);
        assert_eq!(m.cells(), 5);
        assert_eq!(m.cardinality(), 50);
    }

    #[test]
    fn new_clamps_sketches_per_cell_to_one() {
        // Zero or below is treated as one so the matrix never
        // silently produces zero work.
        let m = ExplorationMatrix::new(dims_2_3(), 0);
        assert_eq!(m.sketches_per_cell, 1);
        assert_eq!(m.cardinality(), 5);
    }

    #[test]
    fn from_spec_promotes_to_matrix_shape() {
        let spec =
            MatrixSpec::parse_one("deployment=serverless,self-hosted;storage=sql,kv,blob").unwrap();
        let m = ExplorationMatrix::from_spec(spec, 12);
        assert_eq!(m.dimensions.len(), 2);
        assert_eq!(m.cells(), 5);
        assert_eq!(m.sketches_per_cell, 12);
        assert_eq!(m.cardinality(), 60);
    }

    #[test]
    fn from_derived_passes_dimensions_through() {
        let dims = vec![Dimension {
            id: "auth".into(),
            label: "Auth strategy".into(),
            facets: vec![
                Facet {
                    id: "oauth".into(),
                    label: "OAuth".into(),
                },
                Facet {
                    id: "api-key".into(),
                    label: "API key".into(),
                },
            ],
        }];
        let m = ExplorationMatrix::from_derived(dims, 20);
        assert_eq!(m.dimensions.len(), 1);
        assert_eq!(m.cells(), 2);
        assert_eq!(m.sketches_per_cell, 20);
        assert_eq!(m.cardinality(), 40);
    }

    #[test]
    fn iter_cells_yields_asymmetric_grid() {
        // 2 + 3 = 5 cells, in declaration order, no Cartesian
        // duplication.
        let m = ExplorationMatrix::new(dims_2_3(), 1);
        let cells: Vec<_> = m.iter_cells().collect();
        assert_eq!(cells.len(), 5);
        let ids: Vec<(String, String)> = cells
            .iter()
            .map(|c| (c.dimension_id.clone(), c.facet_id.clone()))
            .collect();
        assert!(ids.contains(&("deployment-model".into(), "serverless".into())));
        assert!(ids.contains(&("deployment-model".into(), "self-hosted".into())));
        assert!(ids.contains(&("storage".into(), "sql".into())));
        assert!(ids.contains(&("storage".into(), "kv".into())));
        assert!(ids.contains(&("storage".into(), "blob".into())));
    }

    #[test]
    fn dimension_lookup_returns_match() {
        let m = ExplorationMatrix::new(dims_2_3(), 1);
        assert_eq!(m.dimension("storage").unwrap().label, "Storage strategy");
        assert!(m.dimension("nope").is_none());
    }

    #[test]
    fn tally_counts_facets_per_dimension() {
        let m = ExplorationMatrix::new(dims_2_3(), 1);
        let t = m.tally();
        assert_eq!(t.len(), 2);
        assert_eq!(t["deployment-model"], 2);
        assert_eq!(t["storage"], 3);
    }

    #[test]
    fn matrix_cell_serializes() {
        let c = MatrixCell {
            dimension_id: "d".into(),
            facet_id: "f".into(),
            label: "d / f".into(),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"dimension_id\":\"d\""));
        assert!(j.contains("\"facet_id\":\"f\""));
    }

    #[test]
    fn cardinality_round_trips_through_json() {
        let m = ExplorationMatrix::new(dims_2_3(), 16);
        let j = serde_json::to_string(&m).unwrap();
        let back: ExplorationMatrix = serde_json::from_str(&j).unwrap();
        assert_eq!(back.cardinality(), 80);
        assert_eq!(back.cells(), 5);
        assert_eq!(back.sketches_per_cell, 16);
    }

    /// PR-D1: the default profile is bit-identical to the v0.5
    /// single-shot contract. `TemperatureProfile::default()` must
    /// produce exactly one call per `(cell, replica)` pair at the
    /// role-default temperature (`1.0` for `Role::Sketch`). Any
    /// drift here is the user-facing "magic switch" the audit
    /// rejected.
    #[test]
    fn temperature_profile_default_is_one_temp_one_replica() {
        let p = TemperatureProfile::default();
        assert_eq!(p.temperatures, vec![1.0]);
        assert_eq!(p.replicas_per_temperature, 1);
        assert_eq!(p.total(), 1);
    }

    /// PR-D1: total iterations = `len(temperatures) ×
    /// replicas_per_temperature`. The `[0.0, 0.3] × 3` example from
    /// the spec yields 6 — exactly the per-cell fan-out the
    /// discovery phase will request.
    #[test]
    fn temperature_profile_total_multiplies() {
        let p = TemperatureProfile {
            temperatures: vec![0.0, 0.3],
            replicas_per_temperature: 3,
        };
        assert_eq!(p.total(), 6);
    }

    /// PR-D1: `profile_for` returns the explicit profile when the
    /// provider model is present, otherwise falls back to
    /// `default_profile`. The matrix's default profile is what
    /// unconfigured providers inherit.
    #[test]
    fn exploration_matrix_profile_for_returns_explicit_then_default() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.3, 0.7, 1.0],
                replicas_per_temperature: 4,
            },
        );
        let default_profile = TemperatureProfile {
            temperatures: vec![0.5],
            replicas_per_temperature: 2,
        };
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        m.default_profile = default_profile;
        let explicit = m.profile_for("MiniMax-M3");
        assert_eq!(explicit.temperatures, vec![0.0, 0.3, 0.7, 1.0]);
        assert_eq!(explicit.replicas_per_temperature, 4);
        let fallback = m.profile_for("deepseek-v4-flash");
        assert_eq!(fallback.temperatures, vec![0.5]);
        assert_eq!(fallback.replicas_per_temperature, 2);
    }

    /// PR-D1: with the default profile (`[1.0] × 1`) and an empty
    /// per-provider map, `cardinality()` matches the no-profile
    /// case — the `total()` factor is `1`, so the formula is
    /// unchanged.
    #[test]
    fn exploration_matrix_cardinality_unchanged_when_no_profiles() {
        let m = ExplorationMatrix::new(dims_2_3(), 10);
        assert_eq!(m.profile_for("anything").total(), 1);
        assert_eq!(m.cardinality(), 50);
    }

    /// PR-D1: a profile-configured matrix round-trips through
    /// JSON. The persisted `exploration_matrix.json` artefact must
    /// carry the per-provider map so a resumed run picks it up
    /// without re-deriving from the CLI flags.
    #[test]
    fn exploration_matrix_round_trips_with_profiles() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.7],
                replicas_per_temperature: 2,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        m.default_profile = TemperatureProfile {
            temperatures: vec![0.5],
            replicas_per_temperature: 1,
        };
        let j = serde_json::to_string(&m).unwrap();
        let back: ExplorationMatrix = serde_json::from_str(&j).unwrap();
        let restored = back.profile_for("MiniMax-M3");
        assert_eq!(restored.temperatures, vec![0.0, 0.7]);
        assert_eq!(restored.replicas_per_temperature, 2);
        assert_eq!(back.default_profile.temperatures, vec![0.5]);
    }

    /// F1: an asymmetric matrix (3 dims with 1/2/3 facets) sums
    /// to 6 cells and rounds to the supplied fan-out for the
    /// cardinality calculation. The previous Cartesian-product
    /// default (dims × facets) is gone.
    #[test]
    fn asymmetric_dimensions_sum_to_six_cells() {
        let dims = vec![
            Dimension {
                id: "auth".into(),
                label: "Auth".into(),
                facets: vec![Facet {
                    id: "oauth".into(),
                    label: "OAuth".into(),
                }],
            },
            Dimension {
                id: "storage".into(),
                label: "Storage".into(),
                facets: vec![
                    Facet {
                        id: "sql".into(),
                        label: "SQL".into(),
                    },
                    Facet {
                        id: "kv".into(),
                        label: "KV".into(),
                    },
                ],
            },
            Dimension {
                id: "scaling".into(),
                label: "Scaling".into(),
                facets: vec![
                    Facet {
                        id: "vertical".into(),
                        label: "Vertical".into(),
                    },
                    Facet {
                        id: "horizontal".into(),
                        label: "Horizontal".into(),
                    },
                    Facet {
                        id: "auto".into(),
                        label: "Auto".into(),
                    },
                ],
            },
        ];
        let m = ExplorationMatrix::new(dims, 10);
        assert_eq!(m.cells(), 6);
        assert_eq!(m.cardinality(), 60);
    }

    /// F1: `cells()` is zero on an empty matrix so a misconfigured
    /// spec (operator passed `--dimensions 0` for example) never
    /// silently produces work.
    #[test]
    fn empty_matrix_has_zero_cells() {
        let m = ExplorationMatrix::new(Vec::new(), 10);
        assert_eq!(m.cells(), 0);
        assert_eq!(m.cardinality(), 0);
        assert_eq!(m.iter_cells().count(), 0);
    }

    /// F1: `load_or_derive` returns `Ok(None)` when the sidecar
    /// is absent — a fresh run gets a clean `None` rather than an
    /// error.
    #[test]
    fn load_or_derive_returns_none_when_sidecar_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = ExplorationMatrix::load_or_derive(dir.path(), 10).expect("load_or_derive ok");
        assert!(m.is_none());
    }

    /// F1: `load_or_derive` reads the sidecar and rebuilds the
    /// matrix around it. The dimensions come back verbatim.
    #[test]
    fn load_or_derive_reads_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dims = dims_2_3();
        let sidecar = DiscoveryDimensions {
            schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.into(),
            brief_hash: "abc".into(),
            dimensions: dims.clone(),
            descriptions: Vec::new(),
            created_unix: 1,
        };
        let path = dir.path().join(DISCOVERY_DIMENSIONS_FILENAME);
        std::fs::write(&path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        let m = ExplorationMatrix::load_or_derive(dir.path(), 5)
            .expect("load ok")
            .expect("sidecar present");
        assert_eq!(m.cells(), 5);
        assert_eq!(m.sketches_per_cell, 5);
        assert_eq!(m.cardinality(), 25);
        assert_eq!(m.dimensions[0].id, dims[0].id);
    }

    /// F1: `load_or_derive` surfaces a malformed sidecar as
    /// `Error::InvalidState` so a resume does not silently drop
    /// the cached dimensions.
    #[test]
    fn load_or_derive_errors_on_malformed_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(DISCOVERY_DIMENSIONS_FILENAME);
        std::fs::write(&path, b"{not json").unwrap();
        let err = ExplorationMatrix::load_or_derive(dir.path(), 5).unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }

    /// F1: `DiscoveryDimensions` round-trips through JSON so a
    /// resumed run can read it back byte-for-byte.
    #[test]
    fn discovery_dimensions_round_trip() {
        let descs = vec![DimensionFacetDescription::new(
            "deployment-model",
            "serverless",
            "Run on a managed runtime.",
        )];
        let sidecar = DiscoveryDimensions {
            schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.into(),
            brief_hash: "deadbeef".into(),
            dimensions: dims_2_3(),
            descriptions: descs,
            created_unix: 123,
        };
        let j = serde_json::to_string(&sidecar).unwrap();
        let back: DiscoveryDimensions = serde_json::from_str(&j).unwrap();
        assert_eq!(back, sidecar);
    }
}
