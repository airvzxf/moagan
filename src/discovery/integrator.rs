//! Discovery integrator — join per-facet extracts into a category doc.
//!
//! Pure helpers; the phase lives in `src/phases/discover_integrate.rs`.

use crate::domain::{CategoryDoc, FacetExtraction};

/// Build a `CategoryDoc` from a list of per-facet `FacetExtraction`.
///
/// `clusters_max_members` is the population of the largest cluster
/// in the run; `density` is the cluster's own member count divided
/// by that maximum (so the largest cluster has density 1.0).
pub fn build_doc(
    category_id: &str,
    cluster_id: &str,
    member_count: usize,
    clusters_max_members: usize,
    sources: Vec<String>,
    body: String,
) -> CategoryDoc {
    let density = if clusters_max_members == 0 {
        0.0
    } else {
        member_count as f32 / clusters_max_members as f32
    };
    CategoryDoc {
        category_id: category_id.into(),
        cluster_id: cluster_id.into(),
        body,
        sources,
        density,
        schema_version: "v1".into(),
    }
}

/// Build the human-readable joining header for a category doc.
pub fn category_header(category_id: &str, label: &str) -> String {
    format!("# Category: {category_id} — {label}\n\n")
}

/// Coalesce extractions into a single markdown body. The integrator
/// phase replaces the LLM join with this local helper when the LLM
/// call fails.
pub fn local_join(category_id: &str, label: &str, extractions: &[FacetExtraction]) -> String {
    let mut buf = category_header(category_id, label);
    for ext in extractions {
        buf.push_str(&format!("## {}\n\n", ext.facet_id));
        buf.push_str(ext.body.trim());
        buf.push_str("\n\n");
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_normalises_to_largest_cluster() {
        let d = build_doc("cat_01", "cluster_01", 4, 8, vec![], "body".into());
        assert!((d.density - 0.5).abs() < 1e-6);
    }

    #[test]
    fn density_zero_when_max_is_zero() {
        let d = build_doc("cat_01", "cluster_01", 0, 0, vec![], "".into());
        assert_eq!(d.density, 0.0);
    }

    #[test]
    fn density_caps_at_one() {
        let d = build_doc("cat_01", "cluster_01", 8, 8, vec![], "".into());
        assert!((d.density - 1.0).abs() < 1e-6);
    }

    #[test]
    fn category_header_contains_label() {
        let h = category_header("cat_01", "auth strategies");
        assert!(h.contains("cat_01"));
        assert!(h.contains("auth strategies"));
    }

    #[test]
    fn local_join_emits_per_facet_headings() {
        let exts = vec![
            FacetExtraction {
                facet_id: "data-flows".into(),
                category_id: "cat_01".into(),
                body: "lines".into(),
                sources: vec![],
                schema_version: "v1".into(),
            },
            FacetExtraction {
                facet_id: "constraints".into(),
                category_id: "cat_01".into(),
                body: "hard ones".into(),
                sources: vec![],
                schema_version: "v1".into(),
            },
        ];
        let s = local_join("cat_01", "auth", &exts);
        assert!(s.contains("# Category: cat_01 — auth"));
        assert!(s.contains("## data-flows"));
        assert!(s.contains("## constraints"));
    }
}
