//! `MatrixSpec` — typed representation of an operator-supplied
//! exploration-matrix specification, plus the parser that turns
//! CLI/TOML/env input into the typed form.
//!
//! F1 (Track G.2): the spec replaces the legacy `--dimensions N +
//! --facets-per-dimension M` flag pair. Each spec is one
//! dimension with one or more facets; the operator can pass the
//! flag repeatedly to declare several dimensions, OR pass a
//! single `--matrix-spec` value with `;`-separated dimensions.
//! Both formats coexist (the user picked "repetible AND
//! consolidado con `;`" in the round-of-questions).
//!
//! Example CLI inputs:
//!
//! ```text
//! --matrix-spec 'deployment=serverless,self-hosted'
//! --matrix-spec 'storage=sql,kv'
//! ```
//!
//! or, in the consolidated format:
//!
//! ```text
//! --matrix-spec 'deployment=serverless,self-hosted;storage=sql,kv'
//! ```
//!
//! The parser accepts the consolidated form so a single flag can
//! declare an arbitrary number of dimensions; the repetitive form
//! keeps the multi-flag CLI ergonomic when an operator wants to
//! spread the spec across shell lines.
//!
//! Facets inside a dimension are **asymmetric** — one dimension
//! can carry 1 facet, another 3, another 5. The matrix's
//! `cells()` function is `sum(d.facets.len())` (NOT a Cartesian
//! product), so the spec intentionally supports arbitrary
//! per-dimension facet counts.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::discovery::matrix::{Dimension, Facet};
use crate::error::{Error, Result};

/// Single facet in a [`DimensionSpec`]. Mirrors the
/// [`crate::discovery::matrix::Facet`] wire shape (plus the
/// optional `description` carried by LLM-derived specs) so the
/// spec can be promoted to the matrix without translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FacetSpec {
    /// Stable id (kebab-case).
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Optional description. CLI-parsed specs leave this empty
    /// (the parser has no source text for a description); LLM
    /// derived specs always populate it so the sidecar can
    /// surface it to the integrator phase.
    #[serde(default)]
    pub description: String,
}

/// Single dimension in a [`MatrixSpec`]. Asymmetric: `facets` can
/// be any non-empty list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DimensionSpec {
    /// Stable dimension id (kebab-case).
    pub id: String,
    /// Human-readable dimension label.
    pub label: String,
    /// Facets (at least one).
    pub facets: Vec<FacetSpec>,
}

/// Operator-supplied exploration-matrix specification. The sum of
/// every `facets.len()` across `dimensions` is the matrix's
/// `cells()` count.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MatrixSpec {
    /// Dimensions in declaration order.
    pub dimensions: Vec<DimensionSpec>,
}

/// LLM-derived mirror of [`MatrixSpec`]. Used by
/// [`crate::llm::role::Role::DimensionDeriver`]'s JSON schema
/// validator so the wire-form contract is enforced at the role
/// boundary. Field shape matches `MatrixSpec` but the field is
/// named `dimensions` to match the prompt's vocabulary and to
/// give the validator a stable envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DerivedDimensions {
    /// Dimensions the LLM derived from the brief.
    pub dimensions: Vec<DimensionSpec>,
}

impl DerivedDimensions {
    /// Convert into a [`MatrixSpec`] (drop-in replacement for the
    /// CLI/TOML side). The phase calls this once the LLM's
    /// payload has been validated.
    pub fn into_matrix_spec(self) -> MatrixSpec {
        tracing::debug!(
            dimensions = self.dimensions.len(),
            "DerivedDimensions::into_matrix_spec"
        );
        MatrixSpec {
            dimensions: self.dimensions,
        }
    }
}

impl MatrixSpec {
    /// Parse a single CLI flag value. The flag can declare ONE
    /// dimension (`deployment=serverless,self-hosted`) or
    /// several (`deployment=serverless,self-hosted;storage=sql,kv`)
    /// using the consolidated form.
    ///
    /// Errors:
    /// - `Error::InvalidArgs` when the spec is malformed (empty,
    ///   missing `=`, missing facet list, empty facet id, …).
    /// - `Error::InvalidArgs` when a facet id is duplicated inside
    ///   one dimension.
    pub fn parse_one(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        tracing::debug!(raw_len = raw.len(), "MatrixSpec::parse_one");
        if raw.is_empty() {
            tracing::warn!("MatrixSpec::parse_one: empty input");
            return Err(Error::InvalidArgs("matrix-spec is empty".to_string()));
        }
        let mut out = Self::default();
        for chunk in raw.split(';') {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                tracing::warn!(raw = %raw, "MatrixSpec::parse_one: empty dimension segment");
                return Err(Error::InvalidArgs(format!(
                    "matrix-spec has an empty dimension segment in {raw:?}"
                )));
            }
            let dim = parse_dimension_segment(chunk)?;
            validate_unique_facet_ids(&dim, raw)?;
            out.dimensions.push(dim);
        }
        if out.dimensions.is_empty() {
            tracing::warn!(raw = %raw, "MatrixSpec::parse_one: zero dimensions");
            return Err(Error::InvalidArgs(format!(
                "matrix-spec {raw:?} produced zero dimensions"
            )));
        }
        tracing::debug!(
            dimensions = out.dimensions.len(),
            cells = out.cells(),
            "MatrixSpec::parse_one ok"
        );
        Ok(out)
    }

    /// Parse a list of CLI flag values. Each entry is either the
    /// repetitive form (`'deployment=serverless,self-hosted'`) or
    /// the consolidated form (`'a=x,y;b=p,q'`). Empty entries are
    /// silently dropped (the parser logs a warning at the call
    /// site) so a script can append an empty string without
    /// aborting the run.
    pub fn parse_all<I, S>(entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Self::default();
        let mut dropped = 0usize;
        for raw in entries {
            let raw = raw.as_ref();
            if raw.trim().is_empty() {
                dropped += 1;
                tracing::trace!("MatrixSpec::parse_all: dropping empty entry");
                continue;
            }
            let parsed = Self::parse_one(raw)?;
            for dim in parsed.dimensions {
                validate_unique_facet_ids(&dim, raw)?;
                out.dimensions.push(dim);
            }
        }
        if dropped > 0 {
            tracing::warn!(dropped, "MatrixSpec::parse_all: dropped empty entries");
        }
        if out.dimensions.is_empty() {
            tracing::warn!("MatrixSpec::parse_all: zero dimensions after parsing all entries");
            return Err(Error::InvalidArgs(
                "matrix-spec produced zero dimensions after parsing all entries".to_string(),
            ));
        }
        tracing::debug!(
            dimensions = out.dimensions.len(),
            cells = out.cells(),
            "MatrixSpec::parse_all ok"
        );
        Ok(out)
    }

    /// Total cells the matrix will fan out across (`sum(facets.len())`).
    /// Kept on the spec so the dispatcher can size the run without
    /// first building the [`crate::discovery::matrix::ExplorationMatrix`].
    pub fn cells(&self) -> usize {
        let v: usize = self.dimensions.iter().map(|d| d.facets.len()).sum();
        tracing::trace!(
            dimensions = self.dimensions.len(),
            cells = v,
            "MatrixSpec::cells"
        );
        v
    }

    /// Validate every dimension and facet id is non-empty +
    /// kebab-case. Returns the first error so a malformed flag
    /// surfaces quickly. The CLI parser also validates each
    /// segment at parse time; this helper is the belt-and-braces
    /// pass for any caller that builds a `MatrixSpec`
    /// programmatically.
    pub fn validate(&self) -> Result<()> {
        tracing::debug!(dimensions = self.dimensions.len(), "MatrixSpec::validate");
        if self.dimensions.is_empty() {
            return Err(Error::InvalidArgs(
                "MatrixSpec has zero dimensions".to_string(),
            ));
        }
        for dim in &self.dimensions {
            if dim.id.is_empty() {
                return Err(Error::InvalidArgs(
                    "MatrixSpec dimension has empty id".to_string(),
                ));
            }
            if !is_kebab_case(&dim.id) {
                return Err(Error::InvalidArgs(format!(
                    "MatrixSpec dimension id {:?} is not kebab-case",
                    dim.id
                )));
            }
            if dim.label.trim().is_empty() {
                return Err(Error::InvalidArgs(format!(
                    "MatrixSpec dimension {:?} has empty label",
                    dim.id
                )));
            }
            if dim.facets.is_empty() {
                return Err(Error::InvalidArgs(format!(
                    "MatrixSpec dimension {:?} has zero facets",
                    dim.id
                )));
            }
            validate_unique_facet_ids(dim, "<MatrixSpec>")?;
            for facet in &dim.facets {
                if !is_kebab_case(&facet.id) {
                    return Err(Error::InvalidArgs(format!(
                        "MatrixSpec facet id {:?} (in dimension {:?}) is not kebab-case",
                        facet.id, dim.id
                    )));
                }
                if facet.label.trim().is_empty() {
                    return Err(Error::InvalidArgs(format!(
                        "MatrixSpec facet {:?} (in dimension {:?}) has empty label",
                        facet.id, dim.id
                    )));
                }
            }
        }
        Ok(())
    }

    /// Convert into a `Vec<Dimension>` ready for
    /// [`crate::discovery::matrix::ExplorationMatrix::new`]. The
    /// conversion drops the per-facet `description` (the matrix
    /// only carries `id` + `label`) — the descriptions live on
    /// the [`crate::discovery::matrix_spec::DimensionSpec`] the
    /// caller persists alongside, or are surfaced via the
    /// dimension-deriver phase's sidecar.
    pub fn into_dimensions(self) -> Vec<Dimension> {
        let dims: Vec<Dimension> = self
            .dimensions
            .into_iter()
            .map(|d| Dimension {
                id: d.id,
                label: d.label,
                facets: d
                    .facets
                    .into_iter()
                    .map(|f| Facet {
                        id: f.id,
                        label: f.label,
                    })
                    .collect(),
            })
            .collect();
        tracing::trace!(dimensions = dims.len(), "MatrixSpec::into_dimensions");
        dims
    }

    /// Convert into `Vec<Dimension>` while preserving per-facet
    /// descriptions in a parallel map (`(dimension_id, facet_id)
    /// → description`). The map is useful for the
    /// `discover_dimensions` phase which validates the LLM's
    /// output and wants to keep the descriptions alongside the
    /// `Dimension`/`Facet` rows.
    pub fn into_dimensions_with_descriptions(
        self,
    ) -> (Vec<Dimension>, HashMap<(String, String), String>) {
        tracing::debug!(
            dimensions = self.dimensions.len(),
            "MatrixSpec::into_dimensions_with_descriptions"
        );
        let mut descs: HashMap<(String, String), String> = HashMap::new();
        let dims = self
            .dimensions
            .into_iter()
            .map(|d| {
                let dim_id = d.id.clone();
                let facets = d
                    .facets
                    .into_iter()
                    .map(|f| {
                        descs.insert((dim_id.clone(), f.id.clone()), f.description);
                        Facet {
                            id: f.id,
                            label: f.label,
                        }
                    })
                    .collect();
                Dimension {
                    id: dim_id,
                    label: d.label,
                    facets,
                }
            })
            .collect();
        (dims, descs)
    }
}

impl DimensionSpec {
    /// Build a `DimensionSpec` from a `Dimension` + per-facet
    /// descriptions map. The reverse of
    /// [`MatrixSpec::into_dimensions_with_descriptions`]. Used by
    /// the `discover_dimensions` phase to round-trip the
    /// LLM-derived `DerivedDimensions` back through the spec
    /// before persisting to the sidecar.
    pub fn from_dimension_with_descriptions(
        dim: &Dimension,
        descriptions: &HashMap<(String, String), String>,
    ) -> Self {
        tracing::trace!(
            dimension_id = %dim.id,
            facets = dim.facets.len(),
            "DimensionSpec::from_dimension_with_descriptions"
        );
        let facets = dim
            .facets
            .iter()
            .map(|f| FacetSpec {
                id: f.id.clone(),
                label: f.label.clone(),
                description: descriptions
                    .get(&(dim.id.clone(), f.id.clone()))
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        Self {
            id: dim.id.clone(),
            label: dim.label.clone(),
            facets,
        }
    }
}

/// Parse a single `id=facet1,facet2,...` segment into a
/// [`DimensionSpec`].
fn parse_dimension_segment(segment: &str) -> Result<DimensionSpec> {
    let (id_part, facets_part) = segment.split_once('=').ok_or_else(|| {
        tracing::warn!(segment = %segment, "parse_dimension_segment: missing '='");
        Error::InvalidArgs(format!(
            "matrix-spec segment {segment:?} is missing `=`; \
             expected `<dim-id>=<facet1>,<facet2>,..."
        ))
    })?;
    let id = id_part.trim();
    if id.is_empty() {
        return Err(Error::InvalidArgs(format!(
            "matrix-spec segment {segment:?} has an empty dimension id"
        )));
    }
    if !is_kebab_case(id) {
        tracing::warn!(id = %id, "parse_dimension_segment: dim id not kebab-case");
        return Err(Error::InvalidArgs(format!(
            "matrix-spec dimension id {:?} is not kebab-case",
            id
        )));
    }
    // Default the label to the dimension id with dashes replaced
    // by spaces and the first letter of each word capitalized —
    // operators can override it later by editing the persisted
    // sidecar if they care. Keeping the default makes the
    // repetitive CLI form ergonomic.
    let label = default_label_from_id(id);
    let mut facets = Vec::new();
    for raw_facet in facets_part.split(',') {
        let fid = raw_facet.trim();
        if fid.is_empty() {
            return Err(Error::InvalidArgs(format!(
                "matrix-spec segment {segment:?} has an empty facet id"
            )));
        }
        if !is_kebab_case(fid) {
            tracing::warn!(
                fid = %fid,
                segment = %segment,
                "parse_dimension_segment: facet id not kebab-case"
            );
            return Err(Error::InvalidArgs(format!(
                "matrix-spec facet id {:?} in segment {segment:?} is not kebab-case",
                fid
            )));
        }
        facets.push(FacetSpec {
            id: fid.to_string(),
            label: default_label_from_id(fid),
            description: String::new(),
        });
    }
    if facets.is_empty() {
        return Err(Error::InvalidArgs(format!(
            "matrix-spec segment {segment:?} has zero facets"
        )));
    }
    Ok(DimensionSpec {
        id: id.to_string(),
        label,
        facets,
    })
}

/// Reject duplicate facet ids inside a dimension — they would
/// collapse two matrix cells into one.
fn validate_unique_facet_ids(dim: &DimensionSpec, raw: &str) -> Result<()> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for facet in &dim.facets {
        if !seen.insert(facet.id.as_str()) {
            tracing::warn!(
                dim_id = %dim.id,
                facet_id = %facet.id,
                "validate_unique_facet_ids: duplicate"
            );
            return Err(Error::InvalidArgs(format!(
                "matrix-spec {raw:?} has duplicate facet id {:?} in dimension {:?}",
                facet.id, dim.id
            )));
        }
    }
    Ok(())
}

/// Cheap kebab-case check: lowercase letters / digits / `-`, no
/// leading or trailing dash, no `--`.
fn is_kebab_case(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }
    if s.contains("--") {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Build a human-readable default label from a kebab-case id.
fn default_label_from_id(id: &str) -> String {
    tracing::trace!(id, "default_label_from_id");
    let mut out = String::with_capacity(id.len());
    let mut at_word_start = true;
    for ch in id.chars() {
        if ch == '-' {
            out.push(' ');
            at_word_start = true;
            continue;
        }
        if at_word_start {
            for upper in ch.to_uppercase() {
                out.push(upper);
            }
            at_word_start = false;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_dimension_with_two_facets() {
        let spec = MatrixSpec::parse_one("deployment=serverless,self-hosted").unwrap();
        assert_eq!(spec.dimensions.len(), 1);
        assert_eq!(spec.dimensions[0].id, "deployment");
        assert_eq!(spec.dimensions[0].facets.len(), 2);
        assert_eq!(spec.dimensions[0].facets[0].id, "serverless");
        assert_eq!(spec.dimensions[0].facets[1].id, "self-hosted");
        assert_eq!(spec.cells(), 2);
    }

    #[test]
    fn parse_one_dimension_with_single_facet() {
        let spec = MatrixSpec::parse_one("observability=metrics").unwrap();
        assert_eq!(spec.dimensions.len(), 1);
        assert_eq!(spec.dimensions[0].facets.len(), 1);
        assert_eq!(spec.cells(), 1);
    }

    #[test]
    fn parse_one_consolidated_form_with_two_dimensions() {
        let spec =
            MatrixSpec::parse_one("deployment=serverless,self-hosted;storage=sql,kv,blob").unwrap();
        assert_eq!(spec.dimensions.len(), 2);
        assert_eq!(spec.dimensions[0].id, "deployment");
        assert_eq!(spec.dimensions[0].facets.len(), 2);
        assert_eq!(spec.dimensions[1].id, "storage");
        assert_eq!(spec.dimensions[1].facets.len(), 3);
        assert_eq!(spec.cells(), 5);
    }

    #[test]
    fn parse_all_repetible_form() {
        let spec = MatrixSpec::parse_all([
            "deployment=serverless,self-hosted",
            "storage=sql,kv,blob",
            "observability=metrics,logs,traces",
        ])
        .unwrap();
        assert_eq!(spec.dimensions.len(), 3);
        assert_eq!(spec.cells(), 8);
    }

    #[test]
    fn parse_all_mixed_form() {
        let spec = MatrixSpec::parse_all([
            "deployment=serverless,self-hosted;storage=sql,kv",
            "observability=metrics,logs",
        ])
        .unwrap();
        assert_eq!(spec.dimensions.len(), 3);
        assert_eq!(spec.cells(), 6);
    }

    #[test]
    fn parse_all_drops_empty_segments() {
        let spec = MatrixSpec::parse_all([
            "",
            "deployment=serverless,self-hosted",
            "   ",
            "storage=sql",
        ])
        .unwrap();
        assert_eq!(spec.dimensions.len(), 2);
        assert_eq!(spec.cells(), 3);
    }

    #[test]
    fn parse_all_rejects_empty_input() {
        let err = MatrixSpec::parse_all(["", "   "]).unwrap_err();
        assert!(err.to_string().contains("zero dimensions"));
    }

    #[test]
    fn parse_rejects_empty_string() {
        let err = MatrixSpec::parse_one("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn parse_rejects_missing_equals() {
        let err = MatrixSpec::parse_one("deployment").unwrap_err();
        assert!(err.to_string().contains("missing `=`"));
    }

    #[test]
    fn parse_rejects_empty_dimension_id() {
        let err = MatrixSpec::parse_one("=serverless").unwrap_err();
        assert!(err.to_string().contains("empty dimension id"));
    }

    #[test]
    fn parse_rejects_uppercase_dimension_id() {
        let err = MatrixSpec::parse_one("Deployment=serverless").unwrap_err();
        assert!(err.to_string().contains("not kebab-case"));
    }

    #[test]
    fn parse_rejects_empty_facet_id() {
        let err = MatrixSpec::parse_one("deployment=serverless,").unwrap_err();
        assert!(err.to_string().contains("empty facet id"));
    }

    #[test]
    fn parse_rejects_zero_facets() {
        // `deployment=` with no facet list trips the
        // "empty facet id" guard because the comma-split yields
        // a single empty entry. The parse rejects the spec
        // before reaching the zero-facets summary check.
        let err = MatrixSpec::parse_one("deployment=").unwrap_err();
        assert!(err.to_string().contains("empty facet id"));
    }

    #[test]
    fn parse_rejects_duplicate_facet_ids() {
        let err = MatrixSpec::parse_one("deployment=serverless,serverless").unwrap_err();
        assert!(err.to_string().contains("duplicate facet id"));
    }

    #[test]
    fn parse_rejects_double_dash_in_id() {
        let err = MatrixSpec::parse_one("deploy--ment=serverless").unwrap_err();
        assert!(err.to_string().contains("not kebab-case"));
    }

    #[test]
    fn parse_trims_whitespace_around_id_and_facets() {
        let spec = MatrixSpec::parse_one("  deployment = serverless , self-hosted  ").unwrap();
        assert_eq!(spec.dimensions[0].id, "deployment");
        assert_eq!(spec.dimensions[0].facets[0].id, "serverless");
        assert_eq!(spec.dimensions[0].facets[1].id, "self-hosted");
    }

    #[test]
    fn default_label_capitalises_words() {
        assert_eq!(default_label_from_id("deployment"), "Deployment");
        assert_eq!(default_label_from_id("self-hosted"), "Self Hosted");
        assert_eq!(default_label_from_id("sql"), "Sql");
    }

    #[test]
    fn validate_accepts_well_formed_spec() {
        let spec = MatrixSpec::parse_one("deployment=serverless,self-hosted").unwrap();
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_dimensions() {
        let spec = MatrixSpec::default();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("zero dimensions"));
    }

    #[test]
    fn validate_rejects_empty_label() {
        let mut spec = MatrixSpec::parse_one("deployment=serverless").unwrap();
        spec.dimensions[0].label = "   ".into();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("empty label"));
    }

    #[test]
    fn into_dimensions_returns_matrix_shape() {
        let spec =
            MatrixSpec::parse_one("deployment=serverless,self-hosted;storage=sql,kv,blob").unwrap();
        let dims = spec.into_dimensions();
        assert_eq!(dims.len(), 2);
        assert_eq!(dims[0].id, "deployment");
        assert_eq!(dims[0].facets.len(), 2);
        assert_eq!(dims[0].facets[0].id, "serverless");
        assert_eq!(dims[0].facets[0].label, "Serverless");
        assert_eq!(dims[1].id, "storage");
        assert_eq!(dims[1].facets.len(), 3);
        assert_eq!(dims[1].facets[2].id, "blob");
    }

    #[test]
    fn into_dimensions_with_descriptions_returns_parallel_map() {
        let spec = DerivedDimensions {
            dimensions: vec![DimensionSpec {
                id: "deployment".into(),
                label: "Deployment model".into(),
                facets: vec![FacetSpec {
                    id: "serverless".into(),
                    label: "Serverless".into(),
                    description: "Run on a managed runtime.".into(),
                }],
            }],
        }
        .into_matrix_spec();
        let (dims, descs) = spec.into_dimensions_with_descriptions();
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0].id, "deployment");
        assert_eq!(dims[0].facets[0].id, "serverless");
        assert_eq!(
            descs
                .get(&("deployment".into(), "serverless".into()))
                .unwrap(),
            "Run on a managed runtime."
        );
    }

    #[test]
    fn dimension_spec_round_trips_through_json() {
        let original = DimensionSpec {
            id: "deployment".into(),
            label: "Deployment model".into(),
            facets: vec![
                FacetSpec {
                    id: "serverless".into(),
                    label: "Serverless".into(),
                    description: String::new(),
                },
                FacetSpec {
                    id: "self_hosted".into(),
                    label: "Self hosted".into(),
                    description: String::new(),
                },
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: DimensionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn matrix_spec_round_trips_through_json() {
        let spec =
            MatrixSpec::parse_one("deployment=serverless,self-hosted;storage=sql,kv,blob").unwrap();
        let json = serde_json::to_string(&spec).unwrap();
        let back: MatrixSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn derived_dimensions_validates_empty_object() {
        // The role-level validator exercises this with `{}`.
        let derived: DerivedDimensions = serde_json::from_str("{}").unwrap();
        assert!(derived.dimensions.is_empty());
    }

    #[test]
    fn derived_dimensions_round_trips_through_json() {
        let derived = DerivedDimensions {
            dimensions: vec![DimensionSpec {
                id: "deployment".into(),
                label: "Deployment".into(),
                facets: vec![FacetSpec {
                    id: "serverless".into(),
                    label: "Serverless".into(),
                    description: String::new(),
                }],
            }],
        };
        let json = serde_json::to_string(&derived).unwrap();
        let back: DerivedDimensions = serde_json::from_str(&json).unwrap();
        assert_eq!(back, derived);
    }
}
