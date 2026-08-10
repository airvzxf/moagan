//! `ExplorationMatrix` — the discovery fan-out grid.
//!
//! Per V4 §6.4 and proposal-02-rust.md §9.1, the discovery matrix is
//! `dims × facets_per_dim × cells_per_facet`. Each cell produces one
//! sketch via the `discover_matrix` prompt. The matrix differs from
//! the angle-rotated sketch fan-out of the standard pipeline because
//! it samples the *space* systematically rather than rotating
//! through a fixed list of personas.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

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

/// The full exploration matrix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplorationMatrix {
    /// Dimensions in declaration order.
    pub dimensions: Vec<Dimension>,
    /// Number of sketches to generate per cell. Default 10
    /// (4 dims × 2 facets × 10 = 80 sketches, the user's chosen
    /// floor).
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
    /// Build a matrix with the default dimensions and facets when no
    /// explicit one is supplied. The defaults target the four most
    /// universally-applicable axes: deployment model, storage
    /// strategy, consistency model, and observability.
    ///
    /// Equivalent to `default_for_with_profiles(cardinality,
    /// HashMap::new(), TemperatureProfile::default())` — the v0.5
    /// single-shot contract is preserved bit-identically.
    pub fn default_for(cardinality: usize) -> Self {
        Self::default_for_with_profiles(cardinality, HashMap::new(), TemperatureProfile::default())
    }

    /// Variant of [`Self::default_for`] that accepts explicit
    /// per-provider temperature profiles. Use this when the CLI
    /// `--temperature-profile` flags (or the `[discovery]`
    /// `config.toml` block) supplied a non-empty map.
    pub fn default_for_with_profiles(
        cardinality: usize,
        temperature_profiles: HashMap<String, TemperatureProfile>,
        default_profile: TemperatureProfile,
    ) -> Self {
        let dims = vec![
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
                ],
            },
            Dimension {
                id: "consistency".into(),
                label: "Consistency model".into(),
                facets: vec![
                    Facet {
                        id: "strong".into(),
                        label: "strong".into(),
                    },
                    Facet {
                        id: "eventual".into(),
                        label: "eventual".into(),
                    },
                ],
            },
            Dimension {
                id: "observability".into(),
                label: "Observability".into(),
                facets: vec![
                    Facet {
                        id: "logs-only".into(),
                        label: "logs only".into(),
                    },
                    Facet {
                        id: "metrics-tracing".into(),
                        label: "metrics + tracing".into(),
                    },
                ],
            },
        ];
        let cells = dims.iter().map(|d| d.facets.len().max(1)).sum::<usize>();
        let per_cell = (cardinality / cells.max(1)).max(1);
        Self {
            dimensions: dims,
            sketches_per_cell: per_cell,
            temperature_profiles,
            default_profile,
        }
    }

    /// Build a matrix from explicit `dimensions` and `facets_per_dim`.
    /// The facets inside each dimension are auto-generated from the
    /// count (`f1`, `f2`, …, `fN`).
    ///
    /// Equivalent to `from_dimensions_with_profiles(num_dimensions,
    /// facets_per_dim, HashMap::new(),
    /// TemperatureProfile::default())` — preserves v0.5 behaviour.
    pub fn from_dimensions(num_dimensions: usize, facets_per_dim: usize) -> Self {
        Self::from_dimensions_with_profiles(
            num_dimensions,
            facets_per_dim,
            HashMap::new(),
            TemperatureProfile::default(),
        )
    }

    /// Variant of [`Self::from_dimensions`] that accepts explicit
    /// per-provider temperature profiles. Same shape as the
    /// no-profile constructor; the profile parameters are stored on
    /// the matrix so the discovery phase can iterate per-provider.
    pub fn from_dimensions_with_profiles(
        num_dimensions: usize,
        facets_per_dim: usize,
        temperature_profiles: HashMap<String, TemperatureProfile>,
        default_profile: TemperatureProfile,
    ) -> Self {
        let dims = (0..num_dimensions)
            .map(|i| Dimension {
                id: format!("dim-{i:02}"),
                label: format!("Dimension {i:02}"),
                facets: (0..facets_per_dim)
                    .map(|j| Facet {
                        id: format!("f{}", j + 1),
                        label: format!("F{}", j + 1),
                    })
                    .collect(),
            })
            .collect();
        let cells = num_dimensions * facets_per_dim.max(1);
        let per_cell = (80 / cells).max(1);
        Self {
            dimensions: dims,
            sketches_per_cell: per_cell,
            temperature_profiles,
            default_profile,
        }
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

    /// Number of cells (one per `dimension × facet` pair).
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

    #[test]
    fn default_for_80_sketches_yields_ten_per_cell() {
        let m = ExplorationMatrix::default_for(80);
        assert_eq!(
            m.cardinality(),
            80,
            "cells={} * per_cell={}",
            m.cells(),
            m.sketches_per_cell
        );
        assert_eq!(m.cells(), 8);
        assert_eq!(m.sketches_per_cell, 10);
    }

    #[test]
    fn default_for_160_doubles_per_cell() {
        let m = ExplorationMatrix::default_for(160);
        assert_eq!(m.cardinality(), 160);
        assert_eq!(m.sketches_per_cell, 20);
    }

    #[test]
    fn from_dimensions_creates_auto_ids() {
        let m = ExplorationMatrix::from_dimensions(3, 2);
        assert_eq!(m.dimensions.len(), 3);
        assert_eq!(m.dimensions[0].facets.len(), 2);
        assert_eq!(m.dimensions[0].facets[0].id, "f1");
        assert_eq!(m.dimensions[0].facets[1].id, "f2");
        assert_eq!(m.dimensions[1].id, "dim-01");
    }

    #[test]
    fn from_dimensions_with_zero_facets_uses_one_facet_min() {
        // The contract: at least one cell per dimension so the
        // matrix is never empty.
        let m = ExplorationMatrix::from_dimensions(4, 0);
        assert_eq!(m.cells(), 4);
    }

    #[test]
    fn iter_cells_yields_dense_grid() {
        let m = ExplorationMatrix::default_for(80);
        let cells: Vec<_> = m.iter_cells().collect();
        assert_eq!(cells.len(), 8);
        assert!(
            cells
                .iter()
                .any(|c| c.dimension_id == "deployment-model" && c.facet_id == "serverless")
        );
        assert!(
            cells
                .iter()
                .any(|c| c.dimension_id == "observability" && c.facet_id == "metrics-tracing")
        );
    }

    #[test]
    fn dimension_lookup_returns_match() {
        let m = ExplorationMatrix::default_for(80);
        assert_eq!(m.dimension("storage").unwrap().label, "Storage strategy");
        assert!(m.dimension("nope").is_none());
    }

    #[test]
    fn tally_counts_facets_per_dimension() {
        let m = ExplorationMatrix::default_for(80);
        let t = m.tally();
        assert_eq!(t.len(), 4);
        assert_eq!(t["deployment-model"], 2);
        assert_eq!(t["storage"], 2);
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
        let m = ExplorationMatrix::default_for(80);
        let j = serde_json::to_string(&m).unwrap();
        let back: ExplorationMatrix = serde_json::from_str(&j).unwrap();
        assert_eq!(back.cardinality(), 80);
    }

    // ---- PR-D1: TemperatureProfile + per-provider profile tests ----

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
        let m = ExplorationMatrix::default_for_with_profiles(80, profiles, default_profile);
        let explicit = m.profile_for("MiniMax-M3");
        assert_eq!(explicit.temperatures, vec![0.0, 0.3, 0.7, 1.0]);
        assert_eq!(explicit.replicas_per_temperature, 4);
        let fallback = m.profile_for("deepseek-v4-flash");
        assert_eq!(fallback.temperatures, vec![0.5]);
        assert_eq!(fallback.replicas_per_temperature, 2);
    }

    /// PR-D1: with the default profile (`[1.0] × 1`) and an empty
    /// per-provider map, `cardinality()` matches v0.5 — the
    /// `total()` factor is `1`, so the formula is unchanged.
    #[test]
    fn exploration_matrix_cardinality_unchanged_when_no_profiles() {
        let m = ExplorationMatrix::default_for(80);
        assert_eq!(m.profile_for("anything").total(), 1);
        assert_eq!(m.cardinality(), 80);
    }

    /// PR-D1: the matrix's profile constructors preserve v0.5
    /// `cardinality` when called without explicit profiles. Pin
    /// `default_for` and `from_dimensions` to the no-profile path so
    /// the audit's "bit-identical default" promise survives
    /// refactors.
    #[test]
    fn exploration_matrix_constructors_default_to_no_profiles() {
        let m1 = ExplorationMatrix::default_for(80);
        assert!(m1.temperature_profiles.is_empty());
        assert_eq!(m1.default_profile, TemperatureProfile::default());

        let m2 = ExplorationMatrix::from_dimensions(3, 2);
        assert!(m2.temperature_profiles.is_empty());
        assert_eq!(m2.default_profile, TemperatureProfile::default());
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
        let m = ExplorationMatrix::default_for_with_profiles(
            80,
            profiles,
            TemperatureProfile {
                temperatures: vec![0.5],
                replicas_per_temperature: 1,
            },
        );
        let j = serde_json::to_string(&m).unwrap();
        let back: ExplorationMatrix = serde_json::from_str(&j).unwrap();
        let restored = back.profile_for("MiniMax-M3");
        assert_eq!(restored.temperatures, vec![0.0, 0.7]);
        assert_eq!(restored.replicas_per_temperature, 2);
        assert_eq!(back.default_profile.temperatures, vec![0.5]);
    }
}
