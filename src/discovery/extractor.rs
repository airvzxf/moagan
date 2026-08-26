//! Discovery extractor — per-facet markdown from a cluster.
//!
//! Pure helpers; the phase lives in `src/phases/discover_extract.rs`.

use crate::domain::FacetExtraction;

/// Render a markdown body from a single `FacetExtraction`. The body
/// is intended as the LLM-generated section; the helper just adds
/// formatting around it.
pub fn render_body(ext: &FacetExtraction) -> String {
    tracing::debug!(
        facet_id = %ext.facet_id,
        body_len = ext.body.len(),
        sources = ext.sources.len(),
        "extractor: render_body"
    );
    let mut s = format!("## {}\n\n", ext.facet_id);
    s.push_str(ext.body.trim());
    s.push('\n');
    s
}

/// Coalesce a list of per-facet `FacetExtraction` into one markdown
/// document. Required facets come first; optional facets follow in
/// the order they were supplied.
pub fn join_markdown(extractions: &[FacetExtraction]) -> String {
    tracing::debug!(count = extractions.len(), "extractor: join_markdown");
    let mut buf = String::new();
    for ext in extractions {
        buf.push_str(&render_body(ext));
    }
    tracing::trace!(total_bytes = buf.len(), "extractor: join_markdown complete");
    buf
}

/// Total unique source sketch ids across the extractions.
pub fn unique_sources(extractions: &[FacetExtraction]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<String> = BTreeSet::new();
    for ext in extractions {
        for s in &ext.sources {
            set.insert(s.clone());
        }
    }
    let out: Vec<String> = set.into_iter().collect();
    tracing::debug!(
        extractions = extractions.len(),
        unique = out.len(),
        "extractor: unique_sources"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, body: &str, sources: &[&str]) -> FacetExtraction {
        FacetExtraction {
            facet_id: id.into(),
            category_id: "cat_01".into(),
            body: body.into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            schema_version: "v1".into(),
        }
    }

    #[test]
    fn render_body_adds_heading_and_trims() {
        let e = mk("data-flows", "  lines\n", &["sk_001"]);
        let r = render_body(&e);
        assert!(r.starts_with("## data-flows"));
        assert_eq!(r.trim(), "## data-flows\n\nlines");
    }

    #[test]
    fn join_markdown_preserves_order() {
        let exts = vec![mk("a", "alpha", &["s1"]), mk("b", "beta", &["s2"])];
        let s = join_markdown(&exts);
        let a_pos = s.find("## a").unwrap();
        let b_pos = s.find("## b").unwrap();
        assert!(a_pos < b_pos);
    }

    #[test]
    fn unique_sources_deduplicates() {
        let exts = vec![
            mk("a", "alpha", &["sk_001", "sk_002"]),
            mk("b", "beta", &["sk_002", "sk_003"]),
        ];
        let u = unique_sources(&exts);
        assert_eq!(u, vec!["sk_001", "sk_002", "sk_003"]);
    }

    #[test]
    fn unique_sources_empty_when_no_extractions() {
        let u = unique_sources(&[]);
        assert!(u.is_empty());
    }
}
