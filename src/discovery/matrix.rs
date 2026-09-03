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
//!
//! F2 (Track G.2): the matrix's `sketches_per_cell` is now an
//! explicit input knob (CLI flag `--sketches-per-cell`, env var
//! `MOAGAN_DISCOVERY_SKETCHES_PER_CELL`, or TOML
//! `[discovery_matrix].sketches_per_cell`), no longer derived
//! from the v0.5 `cardinality / cells` integer division. Default
//! 10 replaces the v0.5 cardinality floor of 80. The
//! operator-facing floor was lowered to 1 in v0.13.2; the
//! constructor's `.max(1)` clamp remains as defence-in-depth
//! against an internal caller passing `0`.

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

    /// F2 (B7): the per-cell fan-out counted over *distinct*
    /// temperatures, measured with `f32::to_bits()` so
    /// bit-identical duplicates collapse into one.
    ///
    /// [`Self::total`] is the number of LLM calls the loop
    /// actually fires — duplicates are preserved on purpose, one
    /// call per declared temperature. `unique_total` is the
    /// number of *distinct* `(temperature, replica)` points that
    /// fan-out explores, which is strictly smaller whenever
    /// `ExplorationMatrix::rewrite_temperatures_to_supported`
    /// snapped several declared temperatures onto the same
    /// upstream-supported value. The gap between the two is what
    /// the `RewriteEvent::dropped_count` signal reports per
    /// profile; the coordinator surfaces the summed version on
    /// its `discovery: loop initialised` line so an operator can
    /// see that the tracker is sized against the pre-collapse
    /// call count while the exploration is narrower.
    pub fn unique_total(&self) -> usize {
        let unique = self
            .temperatures
            .iter()
            .map(|t| t.to_bits())
            .collect::<std::collections::BTreeSet<u32>>()
            .len();
        unique * self.replicas_per_temperature.max(1)
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
        let d = dimension_id.into();
        let f = facet_id.into();
        let desc = description.into();
        tracing::trace!(
            dimension_id = %d,
            facet_id = %f,
            description_len = desc.len(),
            "DimensionFacetDescription::new"
        );
        Self {
            dimension_id: d,
            facet_id: f,
            description: desc,
        }
    }
}

impl DiscoveryDimensions {
    /// Look up the description for a `(dimension_id, facet_id)`
    /// pair. Returns `None` when the LLM did not supply one.
    pub fn description_for(&self, dim: &str, facet: &str) -> Option<&str> {
        let hit = self
            .descriptions
            .iter()
            .find(|d| d.dimension_id == dim && d.facet_id == facet)
            .map(|d| d.description.as_str());
        tracing::trace!(
            dimension_id = %dim,
            facet_id = %facet,
            hit = hit.is_some(),
            "DiscoveryDimensions::description_for"
        );
        hit
    }
}

/// One rewrite event emitted by
/// [`ExplorationMatrix::rewrite_temperatures_to_supported`] when at
/// least one declared temperature was snapped to the upstream's
/// supported set. The CLI dispatcher logs one `tracing::warn!` per
/// event so the operator's audit log faithfully records what the
/// runtime actually executed.
///
/// PR-7: the field shape mirrors the auto-probe warning style — a
/// slice of requested values, a slice of the values they were
/// clamped to, and the count of cells that were rewritten. The
/// `provider_model` field carries the same MODEL name string as
/// [`ExplorationMatrix::temperature_profiles`].
///
/// PR-04b-2 (N-1): the event now also carries the *cardinality*
/// of the declared profile and the *post-collapse* cardinality,
/// so the dispatcher can distinguish a profile that was rewritten
/// verbatim from one that collapsed after upstream clamping. The
/// classic case is a declared `[0.1, 0.12, 0.14, 0.5, 0.52, 0.9,
/// 0.91]` with an upstream that only accepts `[0.1, 0.5, 0.9]` —
/// the operator's audit log now reads
/// `original_count=7 unique_count=3 dropped_count=4`
/// alongside the existing `n_clamped` count, so a future grep on
/// `dropped_count > 0` surfaces the collapse.
#[derive(Debug, Clone)]
pub struct RewriteEvent {
    /// Provider MODEL name (the map key of
    /// [`ExplorationMatrix::temperature_profiles`]).
    pub provider_model: String,
    /// Temperatures the operator declared in the profile, in
    /// declaration order.
    pub requested: Vec<f32>,
    /// Temperatures the rewriter snapped them to (one per
    /// `requested[i]`). Same length as `requested`.
    pub clamped_to: Vec<f32>,
    /// Number of entries where `requested[i]` differs from
    /// `clamped_to[i]` by *more* than `1e-3_f32` (see
    /// [`ExplorationMatrix::rewrite_temperatures_to_supported`]
    /// for the threshold rationale). Strict `>` so a profile
    /// whose values are bit-identical after the rewrite (or
    /// differ by less than `1e-3`) reports `n_clamped == 0`.
    pub n_clamped: usize,
    /// PR-04b-2 (N-1): total number of declared temperatures in
    /// the operator's profile before the rewrite. Equal to
    /// `requested.len()`; surfaced as a first-class field so the
    /// dispatcher can log `original_count` without re-deriving
    /// it from `requested.len()`.
    pub original_count: usize,
    /// PR-04b-2 (N-1): number of *unique* temperatures in the
    /// post-rewrite profile, measured via `f32::to_bits()` to
    /// collapse bit-identical duplicates. Smaller than
    /// `original_count` when the rewrite clamped several
    /// declared values to the same upstream-supported value.
    pub unique_count: usize,
    /// PR-04b-2 (N-1): `original_count - unique_count`, clamped
    /// via `saturating_sub` so an out-of-band rewriter that
    /// expanded the profile reports `0` instead of panicking on
    /// a `usize` underflow. `> 0` is the canonical
    /// "profile collapsed after upstream clamping" signal.
    pub dropped_count: usize,
    /// PR-04b-2 (N-1): `profile.replicas_per_temperature *
    /// unique_count` — the per-cell fan-out the runtime will
    /// actually execute after the rewrite. Exposed so the
    /// dispatcher's audit log shows the runtime the operator
    /// will see, not the one they asked for.
    pub effective_fanout_per_cell: usize,
}

/// The full exploration matrix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExplorationMatrix {
    /// Dimensions in declaration order.
    pub dimensions: Vec<Dimension>,
    /// Number of sketches to generate per cell. Default 10
    /// (4 dims × 2 facets × 10 = 80 sketches, the user's chosen
    /// default). F1 surfaces this as `sketches_per_cell` so the
    /// matrix fan-out is decoupled from a hardcoded `cardinality`
    /// target; F2 renames the CLI flag accordingly. The
    /// operator-facing floor is `MIN_SKETCHES_PER_CELL = 1`
    /// (lowered from 10 in v0.13.2); this field stores whatever
    /// value the operator passed (or 10 by default).
    pub sketches_per_cell: usize,
    /// Per-provider sampling-temperature profiles keyed by the
    /// joined `section::model` string (the same key produced by
    /// `ProviderRegistry::registry_key(section, model)`,
    /// e.g. `"minimax::MiniMax-M3"`,
    /// `"deepseek::deepseek-v4-flash"`,
    /// `"opencode::mimo-v2.5"`). A `(section, model)` pair not
    /// in this map uses [`Self::default_profile`].
    ///
    /// Tanda 04e D-1 changed the persistence shape from bare
    /// MODEL names (PR-D1) to the joined `section::model` form so
    /// the coordinator can fan out across multiple providers in
    /// one run. A v0.14.x sidecar that carries bare-model keys
    /// (e.g. `temperature_profiles["MiniMax-M3"]`) is upgraded
    /// in-memory via [`Self::migrate_legacy_keys`] at read time —
    /// but only for the entry whose model matches the run's
    /// `--provider` model, because that is the only bare key
    /// whose section can be inferred unambiguously. The file is
    /// rewritten in-place on the next `write_json`
    /// call so a re-read sees the new shape. Default empty so the
    /// matrix is bit-identical to the v0.5 behaviour when no
    /// profile is configured (PR-D1).
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
        // Defence-in-depth: clamp `0` to `1` so an internal caller
        // passing `0` cannot silently produce an empty matrix.
        // The operator-facing floor in v0.13.2 is `1`
        // (`MIN_SKETCHES_PER_CELL`), so this only fires when an
        // internal site mis-sets the value to `0`.
        let sketches_per_cell = sketches_per_cell.max(1);
        tracing::debug!(
            dimensions = dimensions.len(),
            sketches_per_cell,
            "ExplorationMatrix::new"
        );
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
        tracing::debug!(
            spec_dims = spec.dimensions.len(),
            sketches_per_cell,
            "ExplorationMatrix::from_spec"
        );
        Self::new(spec.into_dimensions(), sketches_per_cell)
    }

    /// Build a matrix from LLM-derived dimensions. The phase
    /// already validated the brief-deriver's payload; this
    /// constructor just wraps the `Vec<Dimension>` with the
    /// operator-chosen fan-out.
    pub fn from_derived(dimensions: Vec<Dimension>, sketches_per_cell: usize) -> Self {
        tracing::debug!(
            dimensions = dimensions.len(),
            sketches_per_cell,
            "ExplorationMatrix::from_derived"
        );
        Self::new(dimensions, sketches_per_cell)
    }

    /// Build a matrix from an existing
    /// [`DiscoveryDimensions`] sidecar (the resume path) plus the
    /// supplied fan-out. The constructor takes ownership so the
    /// caller doesn't have to clone the dimensions just to wrap
    /// them.
    pub fn from_sidecar(sidecar: DiscoveryDimensions, sketches_per_cell: usize) -> Self {
        tracing::debug!(
            dimensions = sidecar.dimensions.len(),
            sketches_per_cell,
            "ExplorationMatrix::from_sidecar"
        );
        Self::new(sidecar.dimensions, sketches_per_cell)
    }

    /// Load the dimensions sidecar from `<run_dir>/discovery_dimensions.json`
    /// and build the matrix around it. Returns `Ok(None)` when the
    /// sidecar is absent (a fresh run); returns
    /// `Err(Error::InvalidState)` when the sidecar is present but
    /// malformed (resume must surface, not silently drop).
    pub fn load_or_derive(run_dir: &Path, sketches_per_cell: usize) -> Result<Option<Self>> {
        let path = run_dir.join(DISCOVERY_DIMENSIONS_FILENAME);
        tracing::debug!(
            path = %path.display(),
            sketches_per_cell,
            "ExplorationMatrix::load_or_derive"
        );
        if !path.exists() {
            tracing::trace!("load_or_derive: sidecar absent");
            return Ok(None);
        }
        let sidecar: DiscoveryDimensions = match read_json(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "load_or_derive: sidecar malformed"
                );
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
    ///
    /// Tanda 04e D-1: the legacy `temperature_profiles` map keys
    /// are now `section::model` join keys (e.g.
    /// `"minimax::MiniMax-M3"`). A bare-model lookup
    /// (`profile_for("MiniMax-M3")`) is preserved as a thin shim
    /// for the handful of callers that still need it (the
    /// unit tests and the `cli::discover_explain` cardinality
    /// table). New code MUST use [`Self::profile_for_pair`] —
    /// the bare-model lookup silently misses every multi-provider
    /// profile whose key is the joined form.
    pub fn profile_for(&self, provider_model: &str) -> &TemperatureProfile {
        let explicit = self.temperature_profiles.contains_key(provider_model);
        tracing::trace!(
            provider_model = %provider_model,
            explicit_profile = explicit,
            "ExplorationMatrix::profile_for"
        );
        self.temperature_profiles
            .get(provider_model)
            .unwrap_or(&self.default_profile)
    }

    /// Tanda 04e D-1: resolve the temperature profile for a
    /// specific `(section, model)` pair. The lookup key is the
    /// joined `ProviderRegistry::registry_key(section, model)`
    /// string, which matches how the CLI dispatcher and the
    /// coordinator persist the profile map. Returns
    /// [`Self::default_profile`] when the joined key is absent
    /// (the v0.5 unconfigured behaviour for any single
    /// provider).
    ///
    /// The companion helper [`Self::active_provider_profiles`]
    /// enumerates every `(section, model)` pair the coordinator
    /// should fan out across; [`Self::profile_for_pair`] is the
    /// per-iteration lookup the coordinator uses inside the
    /// multi-provider loop. The two helpers share the same
    /// joined-key contract so a key inserted by the CLI merge
    /// step is read back by the coordinator verbatim.
    pub fn profile_for_pair(&self, section: &str, model: &str) -> &TemperatureProfile {
        let key = crate::llm::ProviderRegistry::registry_key(section, model);
        let explicit = self.temperature_profiles.contains_key(&key);
        tracing::trace!(
            section = %section,
            model = %model,
            joined_key = %key,
            explicit_profile = explicit,
            "ExplorationMatrix::profile_for_pair"
        );
        self.temperature_profiles
            .get(&key)
            .unwrap_or(&self.default_profile)
    }

    /// Tanda 04e D-1: enumerate every `(section, model, profile)`
    /// triple the coordinator should fan out across. The list
    /// combines:
    ///
    /// 1. Every explicit entry in [`Self::temperature_profiles`],
    ///    with the joined key split on `"::"` into `(section,
    ///    model)`. Legacy bare-model keys (`"MiniMax-M3"` with no
    ///    `"::"`) are interpreted as `(default_section, model)`
    ///    so a v0.14.x sidecar transparently upgrades to the
    ///    new key form without losing data.
    /// 2. `(default_section, default_model, default_profile)`
    ///    appended so the unconfigured case collapses to one
    ///    provider × one profile (the v0.5 single-shot
    ///    contract).
    ///
    /// The list is deduplicated by `(section, model)` with the
    /// LATER occurrence winning — matches the CLI merge order
    /// where the last `--temperature-profile` flag overrides
    /// earlier ones. Returns at minimum
    /// `[(default_section, default_model, default_profile)]` so
    /// a fresh matrix with no explicit profiles produces the
    /// same fan-out as v0.5.
    ///
    /// The returned `TemperatureProfile` is the entry from
    /// [`Self::temperature_profiles`] (a clone), or
    /// [`Self::default_profile`] when the default fallback is
    /// used. The coordinator clones each entry to drive the
    /// inner `(temperatures × replicas)` loop, so the
    /// returned profile is detached from the matrix's borrow.
    pub fn active_provider_profiles(
        &self,
        default_section: &str,
        default_model: &str,
    ) -> Vec<(String, String, TemperatureProfile)> {
        use std::collections::BTreeMap;
        let mut by_pair: BTreeMap<(String, String), TemperatureProfile> = BTreeMap::new();
        for (key, profile) in self.temperature_profiles.iter() {
            let (section, model) = match key.split_once("::") {
                Some((sec, mdl)) => (sec.to_owned(), mdl.to_owned()),
                None => (default_section.to_owned(), key.clone()),
            };
            // Last-wins per (section, model) pair; matches the CLI
            // merge order ("last --temperature-profile flag wins").
            by_pair.insert((section, model), profile.clone());
        }
        // Always include the default pair so the unconfigured
        // case collapses to one provider. Inserting under the
        // same key only happens when the operator did not set a
        // profile for the default pair — the explicit entry
        // wins on conflict (the BTreeMap insert is a no-op when
        // the key is already present, so the existing
        // explicit-profile value is preserved).
        by_pair
            .entry((default_section.to_owned(), default_model.to_owned()))
            .or_insert_with(|| self.default_profile.clone());
        tracing::debug!(
            pair_count = by_pair.len(),
            default_section = %default_section,
            default_model = %default_model,
            "ExplorationMatrix::active_provider_profiles"
        );
        by_pair.into_iter().map(|(k, v)| (k.0, k.1, v)).collect()
    }

    /// Tanda 04e D-1: rewrite bare-model entries in
    /// [`Self::temperature_profiles`] whose key matches
    /// `default_model` (the v0.14.x persistence shape) into
    /// the joined `default_section::default_model` form. The
    /// rewrite is a one-shot, in-memory migration — the file is
    /// rewritten in-place on the next `write_json` call.
    ///
    /// Returns the number of keys that were re-keyed (0 when
    /// the matrix is already in the new shape). A bare-model
    /// entry whose model differs from `default_model` is left
    /// alone: those entries are outside the documented
    /// migration scope (they would be ambiguous — we do not
    /// know which section they belong to). Re-keying them under
    /// `default_section` would synthesise a `(section, model)`
    /// pair the registry was never asked to host: a v0.14.x
    /// sidecar carrying `temperature_profiles["MiniMax-M3"]`
    /// run under `--provider deepseek:deepseek-v4-flash` would
    /// migrate to `deepseek::MiniMax-M3` and then panic at
    /// dispatch time in `RunContext::provider_for`. Each such
    /// entry gets one `tracing::warn!` so the operator sees the
    /// key was recognised but deliberately not migrated.
    ///
    /// The migration is idempotent: running it twice leaves the
    /// matrix in the new shape without an infinite loop or
    /// duplicate entries.
    pub fn migrate_legacy_keys(&mut self, default_section: &str, default_model: &str) -> usize {
        let mut rewritten = 0usize;
        let mut to_insert: Vec<(String, TemperatureProfile)> = Vec::new();
        let mut to_remove: Vec<String> = Vec::new();
        let mut out_of_scope: Vec<String> = Vec::new();
        for (key, profile) in self.temperature_profiles.iter() {
            if key.contains("::") {
                // Already in the new shape.
                continue;
            }
            // The legacy entry is keyed by the model name alone.
            // Only the entry whose bare model equals the
            // configured default model can be attributed to
            // `default_section` — that is exactly what v0.14.x
            // wrote. Any other bare key belongs to an unknown
            // section, so it stays as-is.
            if key != default_model {
                out_of_scope.push(key.clone());
                continue;
            }
            let joined = crate::llm::ProviderRegistry::registry_key(default_section, key);
            if joined == *key {
                // Should be unreachable: the joined form for
                // (default_section, key) differs from `key` unless
                // `default_section == key` or `key.is_empty()`.
                // Defensive skip in case the registry's join
                // rule ever changes.
                continue;
            }
            if self.temperature_profiles.contains_key(&joined) {
                // The joined form is already present (e.g. a
                // newer run wrote it first). Drop the legacy
                // bare-model key so it does not double-fire on
                // `active_provider_profiles`.
                to_remove.push(key.clone());
                rewritten += 1;
                continue;
            }
            to_insert.push((joined, profile.clone()));
            to_remove.push(key.clone());
            rewritten += 1;
        }
        for (k, v) in to_insert {
            self.temperature_profiles.insert(k, v);
        }
        for k in to_remove {
            self.temperature_profiles.remove(&k);
        }
        for key in &out_of_scope {
            tracing::warn!(
                legacy_key = %key,
                default_section = %default_section,
                default_model = %default_model,
                "ExplorationMatrix::migrate_legacy_keys: bare-model key does not match \
                 the default model; left unmigrated (its section is unknown)"
            );
        }
        if rewritten > 0 {
            tracing::info!(
                rewritten,
                out_of_scope = out_of_scope.len(),
                default_section = %default_section,
                default_model = %default_model,
                "ExplorationMatrix::migrate_legacy_keys"
            );
        }
        rewritten
    }

    /// PR-7: rewrite every per-provider temperature profile so
    /// each declared temperature is snapped to the nearest value
    /// in the supplied `supported_sets` map. The map is keyed by
    /// the same provider MODEL name as
    /// [`Self::temperature_profiles`] (e.g. `"MiniMax-M3"`).
    /// Missing keys leave the corresponding profile untouched —
    /// the runtime gate in
    /// `crate::phases::phase::RunContext::dispatch_to_provider` is
    /// the safety net for any `(provider, model)` that doesn't
    /// appear here.
    ///
    /// `replicas_per_temperature` is left untouched (the rewrite
    /// only changes the temperature axis, not the replica count).
    /// Returns one [`RewriteEvent`] per profile that had at least
    /// one temperature snapped, so the CLI dispatcher can emit a
    /// `tracing::warn!` per event with the operator's audit log
    /// faithfully recording what the runtime actually executed.
    pub fn rewrite_temperatures_to_supported(
        &mut self,
        supported_sets: &std::collections::HashMap<String, Vec<f32>>,
    ) -> Vec<RewriteEvent> {
        tracing::debug!(
            supported_keys = supported_sets.len(),
            profile_keys = self.temperature_profiles.len(),
            "ExplorationMatrix::rewrite_temperatures_to_supported"
        );
        let mut events: Vec<RewriteEvent> = Vec::new();
        for (model, supported) in supported_sets {
            let Some(profile) = self.temperature_profiles.get_mut(model) else {
                tracing::trace!(model = %model, "rewrite: profile missing; skipping");
                continue;
            };
            let rewritten: Vec<f32> = profile
                .temperatures
                .iter()
                .map(|t| crate::llm::temperature_probe::nearest_in_set(supported, *t).unwrap_or(*t))
                .collect();
            // PR-04b-2 (N-2): the band-dead threshold is `1e-3_f32`
            // (not the pre-change `f32::EPSILON ≈ 1.19e-7`). The
            // wider band covers:
            //   - the Ryu-vs-Display rounding gap (`0.7` vs
            //     `0.70000004768` — same bits, different decimal
            //     representations),
            //   - 1-decimal operator rounding (`0.3` vs
            //     `0.30000001192`),
            //   - upstream clamping noise.
            // It does NOT swallow meaningful changes (`0.5 → 1.0`
            // is 0.5 away, well above the threshold).
            let n_clamped = profile
                .temperatures
                .iter()
                .zip(rewritten.iter())
                .filter(|(a, b)| (*a - *b).abs() > 1e-3_f32)
                .count();
            if n_clamped == 0 {
                tracing::trace!(model = %model, "rewrite: nothing to clamp");
                continue;
            }
            let requested = profile.temperatures.clone();
            profile.temperatures = rewritten.clone();
            // PR-04b-2 (N-1): collapse-visibility signals so the
            // operator can tell a profile rewrite from a profile
            // collapse. `to_bits()` deduplicates bit-identical
            // floats so the count is exact.
            let original_count = requested.len();
            let unique_count = rewritten
                .iter()
                .map(|t| t.to_bits())
                .collect::<std::collections::BTreeSet<u32>>()
                .len();
            let dropped_count = original_count.saturating_sub(unique_count);
            let effective_fanout_per_cell = profile
                .replicas_per_temperature
                .saturating_mul(unique_count);
            tracing::trace!(
                model = %model,
                n_clamped,
                original_count,
                unique_count,
                dropped_count,
                "rewrite: clamping temperatures"
            );
            events.push(RewriteEvent {
                provider_model: model.clone(),
                requested,
                clamped_to: rewritten,
                n_clamped,
                original_count,
                unique_count,
                dropped_count,
                effective_fanout_per_cell,
            });
        }
        tracing::debug!(events = events.len(), "rewrite complete");
        events
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
        let c = self.cells() * self.sketches_per_cell;
        tracing::trace!(
            cells = self.cells(),
            sketches_per_cell = self.sketches_per_cell,
            cardinality = c,
            "cardinality"
        );
        c
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
            tracing::trace!("cells: empty matrix");
            0
        } else {
            let v: usize = self.dimensions.iter().map(|d| d.facets.len().max(1)).sum();
            tracing::trace!(dimensions = self.dimensions.len(), cells = v, "cells");
            v
        }
    }

    /// Iterate over every cell in declaration order.
    pub fn iter_cells(&self) -> impl Iterator<Item = MatrixCell> + '_ {
        tracing::trace!("iter_cells invoked");
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
        let hit = self.dimensions.iter().find(|d| d.id == id);
        tracing::trace!(
            dimension_id = %id,
            hit = hit.is_some(),
            "dimension lookup"
        );
        hit
    }

    /// Tally of dimension → facet count, useful for logs.
    pub fn tally(&self) -> BTreeMap<String, usize> {
        let map: BTreeMap<String, usize> = self
            .dimensions
            .iter()
            .map(|d| (d.id.clone(), d.facets.len()))
            .collect();
        tracing::debug!(tally = ?map, "ExplorationMatrix::tally");
        map
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

    /// Tanda 04e D-1: a profile-configured matrix round-trips
    /// through JSON. The persisted `exploration_matrix.json`
    /// artefact must carry the per-`(section, model)` map so a
    /// resumed run picks it up without re-deriving from the CLI
    /// flags. The keys use the joined `section::model` form (the
    /// canonical wire shape since v0.14.3).
    #[test]
    fn exploration_matrix_round_trips_with_profiles() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "minimax::MiniMax-M3".to_owned(),
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
        let restored = back.profile_for_pair("minimax", "MiniMax-M3");
        assert_eq!(restored.temperatures, vec![0.0, 0.7]);
        assert_eq!(restored.replicas_per_temperature, 2);
        assert_eq!(back.default_profile.temperatures, vec![0.5]);
        // The legacy bare-model lookup is preserved as a thin shim
        // for the handful of pre-D-1 tests / callers; it now misses
        // the new joined key.
        let bare = back.profile_for("MiniMax-M3");
        assert_eq!(
            bare.replicas_per_temperature, 1,
            "bare-model lookup misses the joined key form; new code MUST use profile_for_pair"
        );
    }

    /// Tanda 04e D-1: `active_provider_profiles` on a matrix with
    /// no explicit profiles collapses to a single default pair with
    /// the `default_profile` (the v0.5 single-shot contract).
    #[test]
    fn active_provider_profiles_returns_default_pair_when_empty() {
        let m = ExplorationMatrix::new(dims_2_3(), 1);
        let active = m.active_provider_profiles("minimax", "MiniMax-M3");
        assert_eq!(active.len(), 1, "unconfigured case yields exactly one pair");
        let (section, model, profile) = &active[0];
        assert_eq!(section, "minimax");
        assert_eq!(model, "MiniMax-M3");
        assert_eq!(profile.temperatures, vec![1.0]);
        assert_eq!(profile.replicas_per_temperature, 1);
    }

    /// Tanda 04e D-1: `active_provider_profiles` enumerates every
    /// explicit `(section, model)` triple in addition to the
    /// default pair. With two explicit pairs and one default pair
    /// we get three triples total.
    #[test]
    fn active_provider_profiles_enumerates_multi_provider_set() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "minimax::MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.5],
                replicas_per_temperature: 2,
            },
        );
        profiles.insert(
            "opencode::mimo-v2.5".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.7],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        m.default_profile = TemperatureProfile {
            temperatures: vec![0.99],
            replicas_per_temperature: 1,
        };
        let active = m.active_provider_profiles("minimax", "MiniMax-M3");
        assert_eq!(
            active.len(),
            2,
            "explicit pairs cover both keys; default pair is suppressed"
        );
        let total_iterations: usize = active.iter().map(|(_, _, p)| p.total()).sum();
        assert_eq!(total_iterations, 5, "Σ profile.total() = 4 + 1");
        // The default fallback does NOT fire because every pair
        // has an explicit entry.
        for (_, _, profile) in &active {
            assert_ne!(
                profile.temperatures,
                vec![0.99],
                "default profile must not appear when all pairs have explicit entries"
            );
        }
    }

    /// Tanda 04e D-1: `active_provider_profiles` dedupes by
    /// `(section, model)` with last-wins semantics so the CLI
    /// merge order (`--temperature-profile` flags applied in
    /// declaration order) survives into the fan-out enumeration.
    #[test]
    fn active_provider_profiles_dedupes_last_wins() {
        let mut profiles = HashMap::new();
        // Two entries for the same `(section, model)` pair;
        // the LATER insert must win per the merge-order contract.
        profiles.insert(
            "minimax::MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0],
                replicas_per_temperature: 1,
            },
        );
        profiles.insert(
            "minimax::MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.5, 1.0],
                replicas_per_temperature: 4,
            },
        );
        let m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles,
            default_profile: TemperatureProfile::default(),
        };
        let active = m.active_provider_profiles("minimax", "MiniMax-M3");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].2.temperatures, vec![0.0, 0.5, 1.0]);
        assert_eq!(active[0].2.replicas_per_temperature, 4);
    }

    /// Tanda 04e D-1: `migrate_legacy_keys` re-keys a bare-model
    /// entry whose model matches the default model into the joined
    /// `<default_section>::<default_model>` form. The legacy entry
    /// disappears from the map (no double-fan-out risk).
    #[test]
    fn migrate_legacy_keys_rekeys_default_model() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.5],
                replicas_per_temperature: 2,
            },
        );
        let mut m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles,
            default_profile: TemperatureProfile::default(),
        };
        let rewritten = m.migrate_legacy_keys("minimax", "MiniMax-M3");
        assert_eq!(rewritten, 1);
        assert!(!m.temperature_profiles.contains_key("MiniMax-M3"));
        assert!(m.temperature_profiles.contains_key("minimax::MiniMax-M3"));
        let restored = m.profile_for_pair("minimax", "MiniMax-M3");
        assert_eq!(restored.temperatures, vec![0.0, 0.5]);
        assert_eq!(restored.replicas_per_temperature, 2);
    }

    /// Tanda 04e D-1: `migrate_legacy_keys` is a no-op on a
    /// matrix whose keys are already in the joined form. Returns
    /// 0 and leaves the map unchanged.
    #[test]
    fn migrate_legacy_keys_no_op_on_already_joined_keys() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "minimax::MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles.clone(),
            default_profile: TemperatureProfile::default(),
        };
        let rewritten = m.migrate_legacy_keys("minimax", "MiniMax-M3");
        assert_eq!(rewritten, 0);
        assert_eq!(m.temperature_profiles, profiles);
    }

    /// Tanda 04e D-1: `migrate_legacy_keys` is idempotent —
    /// running it twice leaves the matrix in the new shape
    /// without an infinite loop or duplicate entries.
    #[test]
    fn migrate_legacy_keys_is_idempotent() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles,
            default_profile: TemperatureProfile::default(),
        };
        assert_eq!(m.migrate_legacy_keys("minimax", "MiniMax-M3"), 1);
        assert_eq!(m.migrate_legacy_keys("minimax", "MiniMax-M3"), 0);
        assert_eq!(m.temperature_profiles.len(), 1);
        assert!(m.temperature_profiles.contains_key("minimax::MiniMax-M3"));
    }

    /// Tanda 04e D-1: `migrate_legacy_keys` drops a bare-model
    /// entry whose joined form already exists, so the explicit
    /// joined entry wins and the matrix does not double-fire the
    /// profile on `active_provider_profiles`.
    #[test]
    fn migrate_legacy_keys_drops_conflicting_bare_entry() {
        let mut profiles = HashMap::new();
        // Legacy bare-model entry.
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0],
                replicas_per_temperature: 1,
            },
        );
        // New joined-key entry that already exists.
        profiles.insert(
            "minimax::MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.5],
                replicas_per_temperature: 2,
            },
        );
        let mut m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles,
            default_profile: TemperatureProfile::default(),
        };
        let rewritten = m.migrate_legacy_keys("minimax", "MiniMax-M3");
        assert_eq!(rewritten, 1, "the bare entry is migrated-and-dropped");
        // The joined entry wins (temperatures = [0.5]).
        let restored = m.profile_for_pair("minimax", "MiniMax-M3");
        assert_eq!(restored.temperatures, vec![0.5]);
        assert_eq!(restored.replicas_per_temperature, 2);
        assert!(!m.temperature_profiles.contains_key("MiniMax-M3"));
    }

    /// F2 (B1): a bare-model entry whose model does NOT match
    /// `default_model` is left untouched. Migrating it under
    /// `default_section` would synthesise a `(section, model)`
    /// pair the registry was never built for — the exact
    /// over-migration that made a v0.14.x sidecar panic in
    /// `RunContext::provider_for` when the operator switched
    /// `--provider` to another section.
    #[test]
    fn migrate_legacy_keys_leaves_non_default_model_untouched() {
        let mut profiles = HashMap::new();
        // v0.14.x sidecar entry written by a MiniMax run.
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.5],
                replicas_per_temperature: 2,
            },
        );
        let mut m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles,
            default_profile: TemperatureProfile::default(),
        };
        // The operator re-runs with a different provider AND model.
        let rewritten = m.migrate_legacy_keys("deepseek", "deepseek-v4-flash");
        assert_eq!(rewritten, 0, "no key belongs to the new default pair");
        assert!(
            m.temperature_profiles.contains_key("MiniMax-M3"),
            "the legacy entry is preserved verbatim"
        );
        assert!(
            !m.temperature_profiles.contains_key("deepseek::MiniMax-M3"),
            "the legacy entry must NOT be re-keyed under the new section"
        );
    }

    /// F2 (B1): the migration is scoped per default pair, so a
    /// matrix holding several bare-model keys upgrades exactly
    /// one of them — the one the current run's `--provider`
    /// selects — and leaves the rest for a future run that
    /// selects them.
    #[test]
    fn migrate_legacy_keys_migrates_only_the_default_model_entry() {
        let mut profiles = HashMap::new();
        for model in ["MiniMax-M3", "deepseek-v4-flash", "mimo-v2.5"] {
            profiles.insert(
                model.to_owned(),
                TemperatureProfile {
                    temperatures: vec![0.5],
                    replicas_per_temperature: 1,
                },
            );
        }
        let mut m = ExplorationMatrix {
            dimensions: dims_2_3(),
            sketches_per_cell: 1,
            temperature_profiles: profiles,
            default_profile: TemperatureProfile::default(),
        };
        let rewritten = m.migrate_legacy_keys("deepseek", "deepseek-v4-flash");
        assert_eq!(rewritten, 1);
        assert!(
            m.temperature_profiles
                .contains_key("deepseek::deepseek-v4-flash")
        );
        assert!(!m.temperature_profiles.contains_key("deepseek-v4-flash"));
        assert!(m.temperature_profiles.contains_key("MiniMax-M3"));
        assert!(m.temperature_profiles.contains_key("mimo-v2.5"));
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

    // ===========================================================
    // PR-7: rewrite_temperatures_to_supported tests
    //
    // The rewriter snaps every declared temperature in
    // `temperature_profiles` to the nearest value in a
    // `provider_model -> supported_set` map. Missing keys leave
    // the corresponding profile untouched; replicas are preserved
    // across the rewrite; an empty map produces no events and
    // mutates no state.
    // ===========================================================

    /// PR-7: declared temperatures outside the supported set snap
    /// to the nearest supported value. The event carries the
    /// exact `requested` and `clamped_to` vectors plus the
    /// clamp count so the CLI dispatcher can log a faithful
    /// `tracing::warn!` per profile.
    ///
    /// NOTE: `n_clamped` is 6 here, not 7 — position 3
    /// (`1.0 → 1.0`) is a no-op because `1.0` is already in the
    /// supported set. The plan listed `n_clamped = 7` in this
    /// case; the correct value matches the implementation
    /// (counts only entries where the requested value differs
    /// from the clamped value by more than `1e-3_f32` — see the
    /// doc-comment on
    /// [`ExplorationMatrix::rewrite_temperatures_to_supported`]
    /// for the threshold rationale, PR-04b-2 N-2).
    #[test]
    fn rewrite_temperatures_to_supported_clamps_to_nearest() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9],
                replicas_per_temperature: 2,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        supported.insert("MiniMax-M3".to_owned(), vec![0.2, 1.0, 1.8]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.provider_model, "MiniMax-M3");
        assert_eq!(e.requested, vec![0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9]);
        assert_eq!(e.clamped_to, vec![0.2, 0.2, 1.0, 1.0, 1.0, 1.8, 1.8]);
        assert_eq!(e.n_clamped, 6);
        // In-place mutation: the profile on the matrix carries the
        // snapped temperatures.
        let p = m.profile_for("MiniMax-M3");
        assert_eq!(p.temperatures, vec![0.2, 0.2, 1.0, 1.0, 1.0, 1.8, 1.8]);
        assert_eq!(p.replicas_per_temperature, 2);
    }

    /// PR-7: when the declared profile is already a subset of
    /// the supported set, the rewriter emits no event and leaves
    /// the profile untouched. The runtime gate at dispatch then
    /// sees nothing to clamp.
    #[test]
    fn rewrite_temperatures_to_supported_keeps_when_all_match() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.2, 0.5, 1.0],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        supported.insert("MiniMax-M3".to_owned(), vec![0.2, 0.5, 1.0]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert!(events.is_empty(), "no event when nothing was clamped");
        // Profile is untouched.
        let p = m.profile_for("MiniMax-M3");
        assert_eq!(p.temperatures, vec![0.2, 0.5, 1.0]);
        assert_eq!(p.replicas_per_temperature, 1);
    }

    /// PR-7: when no supported set is supplied for any provider,
    /// the rewriter emits no events and mutates no state. The
    /// runtime gate at dispatch is the safety net for these
    /// `(provider, model)` pairs.
    #[test]
    fn rewrite_temperatures_to_supported_passes_through_when_no_supported_set() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.0, 0.5, 1.5],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert!(events.is_empty(), "no supported sets → no events");
        let p = m.profile_for("MiniMax-M3");
        assert_eq!(p.temperatures, vec![0.0, 0.5, 1.5]);
    }

    /// PR-7: the rewriter only touches the `temperatures` axis;
    /// `replicas_per_temperature` is preserved verbatim across
    /// the rewrite so the per-cell fan-out does not silently
    /// collapse or grow.
    #[test]
    fn rewrite_temperatures_to_supported_preserves_replicas() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.3, 0.7, 1.2, 1.9],
                replicas_per_temperature: 5,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        supported.insert("MiniMax-M3".to_owned(), vec![0.2, 1.0, 1.8]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].n_clamped, 4);
        let p = m.profile_for("MiniMax-M3");
        assert_eq!(p.replicas_per_temperature, 5);
        // Temperatures snapped but replicas preserved.
        assert_eq!(p.temperatures, vec![0.2, 1.0, 1.0, 1.8]);
        assert_eq!(p.total(), 4 * 5);
    }

    /// PR-7: a `supported_sets` key that doesn't have a matching
    /// entry in `temperature_profiles` is silently ignored — the
    /// rewriter iterates the matrix's profiles, not the input
    /// map's keys, so a stale or misnamed entry cannot crash the
    /// pipeline.
    #[test]
    fn rewrite_temperatures_to_supported_skips_unknown_keys() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.7],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        // "unknown-model" is in supported_sets but NOT in
        // temperature_profiles → silently ignored.
        supported.insert("unknown-model".to_owned(), vec![0.5]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert!(events.is_empty());
        let p = m.profile_for("MiniMax-M3");
        assert_eq!(p.temperatures, vec![0.7]);
    }

    /// PR-04b-2 (N-1): when the upstream collapses several
    /// declared temperatures to a smaller supported set, the
    /// `RewriteEvent` must surface the cardinality signals so the
    /// dispatcher can distinguish "rewrite verbatim" from
    /// "rewrite + collapse". The canonical case is
    /// `[0.1, 0.12, 0.14, 0.5, 0.52, 0.9, 0.91]` declared by the
    /// operator and `[0.1, 0.5, 0.9]` supported by the upstream
    /// → `original_count = 7`, `unique_count = 3`,
    /// `dropped_count = 4`.
    #[test]
    fn rewrite_event_exposes_collapse_signals() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.1, 0.12, 0.14, 0.5, 0.52, 0.9, 0.91],
                replicas_per_temperature: 2,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        supported.insert("MiniMax-M3".to_owned(), vec![0.1, 0.5, 0.9]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.original_count, 7);
        assert_eq!(e.unique_count, 3);
        assert_eq!(e.dropped_count, 4);
        // effective_fanout_per_cell = replicas_per_temperature *
        // unique_count = 2 * 3 = 6.
        assert_eq!(e.effective_fanout_per_cell, 6);
        // The clamped vector must carry the deduplicated set.
        assert_eq!(e.clamped_to, vec![0.1, 0.1, 0.1, 0.5, 0.5, 0.9, 0.9]);
    }

    /// PR-04b-2 (N-2): the band-dead threshold is `1e-3_f32`,
    /// not `f32::EPSILON ≈ 1.19e-7`. A rewriter that restores
    /// `f32::EPSILON` must NOT pass this test: a near-equal
    /// rewrite (`0.10000005_f32` vs `0.1_f32` — distance
    /// `~5e-8`) is below `1e-3_f32` and therefore a no-op, but
    /// above `f32::EPSILON` so it would have been counted as a
    /// clamp under the old threshold. The test pins the new
    /// threshold from both sides:
    ///
    /// - `requested = [0.10000005_f32]` vs `supported = [0.1]`:
    ///   distance `~5e-8`, below `1e-3` → `n_clamped = 0`.
    /// - `requested = [0.7 + 0.01_f32 = 0.71_f32]` vs
    ///   `supported = [0.7]`: distance `0.01`, above `1e-3` →
    ///   `n_clamped = 1`.
    ///
    /// If a future refactor narrows the threshold back to
    /// `f32::EPSILON`, the first case produces `n_clamped = 1`
    /// and the test fails.
    #[test]
    fn rewrite_clamps_near_equal_but_not_bit_identical() {
        // First case: distance ~5e-8 (below 1e-3) — must NOT
        // be counted as clamped under the new threshold.
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.10000005_f32],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        supported.insert("MiniMax-M3".to_owned(), vec![0.1_f32]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert_eq!(
            events.len(),
            0,
            "0.10000005 is within 1e-3 of 0.1 — must NOT trigger a RewriteEvent"
        );

        // Second case: distance 0.01 (above 1e-3) — MUST be
        // counted as clamped.
        let mut profiles = HashMap::new();
        profiles.insert(
            "MiniMax-M3".to_owned(),
            TemperatureProfile {
                temperatures: vec![0.71_f32],
                replicas_per_temperature: 1,
            },
        );
        let mut m = ExplorationMatrix::new(dims_2_3(), 1);
        m.temperature_profiles = profiles;
        let mut supported: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        supported.insert("MiniMax-M3".to_owned(), vec![0.7_f32]);
        let events = m.rewrite_temperatures_to_supported(&supported);
        assert_eq!(
            events.len(),
            1,
            "0.71 differs from 0.7 by 0.01 — MUST trigger a RewriteEvent"
        );
        assert_eq!(events[0].n_clamped, 1, "exactly one entry was clamped");
    }
}
