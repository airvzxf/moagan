//! D.13.5: stable id newtypes shared across the discovery pipeline.
//!
//! [`SketchId`], [`ContradictionId`], and [`FacetId`] are thin
//! wrappers around `String`. They exist so the discovery helpers
//! can pass stable id types instead of leaking the underlying
//! string storage, mirroring the role of
//! [`crate::discovery::outlier::SketchId`] (kept as a re-export
//! for backward compatibility — `outlier.rs` predates this
//! module).
//!
//! Each newtype is `#[serde(transparent)]` so the JSON
//! representation is the bare string. Conversion from `&str` and
//! `String` is implemented for ergonomic construction; the
//! underlying string is treated as opaque (no format validation)
//! because the upstream phases already enforce the canonical
//! `sk_<NN>` / `c_<NN>` / `<category>:<facet>` shapes.

/// Newtype around `String` identifying a sketch in the discovery
/// pipeline. Equivalent to the original
/// [`crate::discovery::outlier::SketchId`] — re-exported from
/// `outlier.rs` for backward compatibility.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SketchId(pub String);

/// Newtype around `String` identifying a contradiction in the
/// discovery pipeline. Source of ids is the
/// `contradictions/contradictions.json` file (`c_<NN>`).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ContradictionId(pub String);

/// Newtype around `String` identifying a facet across every facet
/// list. Format: `<category_id>:<facet_id>` (the category id
/// disambiguates facets with the same slug across clusters).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct FacetId(pub String);

impl SketchId {
    /// Build a `SketchId` from a borrowed string. Does not
    /// validate the id shape; callers can pass any string (e.g.
    /// `"sk_0001"`).
    pub fn new(id: impl Into<String>) -> Self {
        let inner = id.into();
        tracing::trace!(id = %inner, "SketchId::new");
        Self(inner)
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ContradictionId {
    /// Build a `ContradictionId` from a borrowed string. Does not
    /// validate the id shape; callers can pass any string (e.g.
    /// `"c_001"`).
    pub fn new(id: impl Into<String>) -> Self {
        let inner = id.into();
        tracing::trace!(id = %inner, "ContradictionId::new");
        Self(inner)
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FacetId {
    /// Build a `FacetId` from a borrowed string. Does not
    /// validate the id shape; callers can pass any string (e.g.
    /// `"cat_01:data-flows"`).
    pub fn new(id: impl Into<String>) -> Self {
        let inner = id.into();
        tracing::trace!(id = %inner, "FacetId::new");
        Self(inner)
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SketchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for ContradictionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for FacetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SketchId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SketchId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for ContradictionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ContradictionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for FacetId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for FacetId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for SketchId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ContradictionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for FacetId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sketch_id_new_wraps_string() {
        let s = SketchId::new("sk_001");
        assert_eq!(s.as_str(), "sk_001");
        assert_eq!(s.to_string(), "sk_001");
    }

    #[test]
    fn sketch_id_from_str_and_string() {
        let a: SketchId = "sk_002".into();
        let b: SketchId = String::from("sk_002").into();
        assert_eq!(a, b);
    }

    #[test]
    fn contradiction_id_round_trip() {
        let a = ContradictionId::new("c_007");
        assert_eq!(a.as_str(), "c_007");
        let b: ContradictionId = String::from("c_007").into();
        assert_eq!(a, b);
    }

    #[test]
    fn facet_id_supports_category_prefix() {
        let a = FacetId::new("cat_01:data-flows");
        assert_eq!(a.as_str(), "cat_01:data-flows");
        let b: FacetId = "cat_01:data-flows".into();
        assert_eq!(a, b);
    }

    #[test]
    fn ids_are_transparent_in_json() {
        // The newtypes must serialise as bare strings so the
        // `discovery_context.json` shape stays flat (Vec<String>-ish).
        let v = serde_json::to_string(&SketchId::new("sk_42")).unwrap();
        assert_eq!(v, "\"sk_42\"");
        let v = serde_json::to_string(&ContradictionId::new("c_01")).unwrap();
        assert_eq!(v, "\"c_01\"");
        let v = serde_json::to_string(&FacetId::new("cat_01:data-flows")).unwrap();
        assert_eq!(v, "\"cat_01:data-flows\"");
    }
}
