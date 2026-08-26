//! Discovery integrator — join per-facet extracts into a category doc.
//!
//! Pure helpers; the phase lives in `src/phases/discover_integrate.rs`.

use std::collections::BTreeSet;

use crate::domain::{CategoryDoc, FacetExtraction};

/// Minimum coverage ratio the LLM-joined document must keep vs the
/// local join (catalog 10-integrada-v0 decision 42, V4 §6.10).
/// Below this the integrator reverts to the local join so a
/// pathological re-write cannot dilute the content. Set to `0.85`
/// (i.e. the LLM may compress up to ~15% of the original; the §6.10
/// "20%" rule is interpreted as "may not lose more than 15%",
/// matching the catalog's `coverage_ratio >= 0.85`).
pub const COVERAGE_RATIO_MIN: f32 = 0.85;

/// Minimum fraction of the original citations that must remain in
/// the refined document (catalog 10-integrada-v0 decision 42).
/// Below this the integrator reverts to the local join so citations
/// are not silently dropped.
pub const PRESERVED_CITATIONS_MIN: f32 = 0.9;

/// Compute the coverage ratio between two markdown bodies. The
/// formula is `len(refined) / len(original)` clamped to `[0, 1]`.
/// Whitespace is normalised before the comparison so a re-write that
/// only changes line breaks still counts as 100% coverage.
pub fn coverage_ratio(original: &str, refined: &str) -> f32 {
    let o = collapse_whitespace(original).chars().count();
    let r = collapse_whitespace(refined).chars().count();
    if o == 0 {
        tracing::trace!(
            refined_len = r,
            "integrator: coverage_ratio (empty original)"
        );
        return 1.0;
    }
    let ratio = r as f32 / o as f32;
    let clamped = ratio.clamp(0.0, 1.0);
    tracing::debug!(
        original = o,
        refined = r,
        ratio = clamped,
        "integrator: coverage_ratio"
    );
    clamped
}

/// Compute the fraction of citations in `original` that survive in
/// `refined`. A "citation" is a `sk_<id>` token — the same shape used
/// by the extractor to tag its sources.
pub fn preserved_citations_ratio(original: &str, refined: &str) -> f32 {
    let original_citations = citation_set(original);
    if original_citations.is_empty() {
        tracing::trace!("integrator: preserved_citations_ratio (empty original citations)");
        return 1.0;
    }
    let refined_citations = citation_set(refined);
    let preserved = original_citations.intersection(&refined_citations).count();
    let ratio = preserved as f32 / original_citations.len() as f32;
    tracing::debug!(
        original_citations = original_citations.len(),
        refined_citations = refined_citations.len(),
        preserved,
        ratio,
        "integrator: preserved_citations_ratio"
    );
    ratio
}

/// True if the refined document passes both safeguard thresholds
/// (`coverage_ratio >= COVERAGE_RATIO_MIN` and
/// `preserved_citations_ratio >= PRESERVED_CITATIONS_MIN`).
/// Returns the failing thresholds in the error message so the
/// warning has actionable detail.
pub fn meets_safeguards(original: &str, refined: &str) -> Result<(), String> {
    tracing::debug!(
        original_len = original.len(),
        refined_len = refined.len(),
        "integrator: meets_safeguards"
    );
    let cov = coverage_ratio(original, refined);
    let cit = preserved_citations_ratio(original, refined);
    let mut failures: Vec<String> = Vec::new();
    if cov < COVERAGE_RATIO_MIN {
        failures.push(format!("coverage_ratio {cov:.3} < {COVERAGE_RATIO_MIN}"));
    }
    if cit < PRESERVED_CITATIONS_MIN {
        failures.push(format!(
            "preserved_citations {cit:.3} < {PRESERVED_CITATIONS_MIN}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        tracing::warn!(
            coverage = cov,
            citations = cit,
            failures = failures.len(),
            "integrator: safeguards FAILED"
        );
        Err(failures.join("; "))
    }
}

fn collapse_whitespace(s: &str) -> String {
    tracing::trace!(input_len = s.len(), "integrator: collapse_whitespace");
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn citation_set(text: &str) -> BTreeSet<String> {
    // Citations are `sk_<id>` tokens. Match them by sliding a
    // 3-char window and looking for the prefix. This avoids the
    // overhead of a regex while staying readable.
    let bytes = text.as_bytes();
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"sk_" {
            // Find the longest run of `[a-zA-Z0-9_]` that follows.
            let start = i;
            let mut j = i + 3;
            while j < bytes.len() {
                let b = bytes[j];
                if b.is_ascii_alphanumeric() || b == b'_' {
                    j += 1;
                } else {
                    break;
                }
            }
            if j > start + 3 {
                let id = std::str::from_utf8(&bytes[start..j])
                    .unwrap_or("")
                    .to_string();
                set.insert(id);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    tracing::trace!(
        bytes = bytes.len(),
        citations = set.len(),
        "integrator: citation_set"
    );
    set
}

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
    tracing::debug!(
        category_id = %category_id,
        cluster_id = %cluster_id,
        member_count,
        clusters_max_members,
        sources = sources.len(),
        body_len = body.len(),
        "integrator: build_doc"
    );
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
    tracing::trace!(
        category_id = %category_id,
        label = %label,
        "integrator: category_header"
    );
    format!("# Category: {category_id} — {label}\n\n")
}

/// Coalesce extractions into a single markdown body. The integrator
/// phase replaces the LLM join with this local helper when the LLM
/// call fails.
pub fn local_join(category_id: &str, label: &str, extractions: &[FacetExtraction]) -> String {
    tracing::debug!(
        category_id = %category_id,
        label = %label,
        extractions = extractions.len(),
        "integrator: local_join"
    );
    let mut buf = category_header(category_id, label);
    for ext in extractions {
        buf.push_str(&format!("## {}\n\n", ext.facet_id));
        buf.push_str(ext.body.trim());
        buf.push_str("\n\n");
    }
    tracing::trace!(total_bytes = buf.len(), "integrator: local_join done");
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

    // -- Safeguard helpers (catalog decision 42, V4 §6.10) -------

    #[test]
    fn coverage_ratio_is_one_for_identical_bodies() {
        let s = "# heading\n\nbody body body.\n";
        assert_eq!(coverage_ratio(s, s), 1.0);
    }

    #[test]
    fn coverage_ratio_normalises_whitespace() {
        // A re-write that only adjusts line breaks still counts as
        // 100% coverage.
        let a = "alpha\nbeta\ngamma";
        let b = "alpha beta gamma";
        assert!((coverage_ratio(a, b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn coverage_ratio_drops_when_content_is_truncated() {
        // Refined is half the original → coverage drops to ~0.5.
        let a = "x".repeat(100);
        let b = "x".repeat(50);
        let c = coverage_ratio(&a, &b);
        assert!(c > 0.4 && c < 0.6, "got {c}");
    }

    #[test]
    fn coverage_ratio_for_empty_original_is_one() {
        // Nothing to lose → trivially safe.
        assert_eq!(coverage_ratio("", "anything"), 1.0);
    }

    #[test]
    fn coverage_ratio_clamps_overflow() {
        // A refined body longer than the original is clamped at 1.0
        // (we do not penalise the LLM for adding context).
        let a = "x".repeat(10);
        let b = "x".repeat(1000);
        assert_eq!(coverage_ratio(&a, &b), 1.0);
    }

    #[test]
    fn preserved_citations_ratio_keeps_all() {
        let a = "body cites sk_001 and sk_002\n";
        let b = "refined body still cites sk_001 and sk_002\n";
        assert!((preserved_citations_ratio(a, b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn preserved_citations_ratio_drops_when_sketch_lost() {
        let a = "body cites sk_001 and sk_002\n";
        let b = "refined body cites only sk_001\n";
        let r = preserved_citations_ratio(a, b);
        assert!(r > 0.4 && r < 0.6, "got {r}");
    }

    #[test]
    fn preserved_citations_ratio_for_no_citations_is_one() {
        // Original has no `sk_*` tokens → trivially safe.
        assert_eq!(preserved_citations_ratio("no citations here", ""), 1.0);
    }

    #[test]
    fn meets_safeguards_passes_when_both_ok() {
        let a = "body with sk_001 and sk_002 cited throughout";
        let b = "refined body with sk_001 and sk_002 cited throughout still";
        assert!(meets_safeguards(a, b).is_ok());
    }

    #[test]
    fn meets_safeguards_fails_on_short_refined() {
        let a = "x".repeat(100);
        let b = "tiny";
        let err = meets_safeguards(&a, b).unwrap_err();
        assert!(err.contains("coverage_ratio"), "got: {err}");
        assert!(err.contains("<"), "got: {err}");
    }

    #[test]
    fn meets_safeguards_fails_on_dropped_citations() {
        let a = "cites sk_001 sk_002 sk_003 sk_004 sk_005\ncites sk_001 again\n";
        let b = "cites sk_001\n";
        let err = meets_safeguards(a, b).unwrap_err();
        assert!(err.contains("preserved_citations"), "got: {err}");
    }

    #[test]
    fn meets_safeguards_reports_both_failures() {
        // Body a has real citations but is dropped to a one-liner
        // that drops most of them — both coverage AND citations
        // must fail.
        let a = "x x x x sk_001 sk_002 sk_003 sk_004 sk_005\n".repeat(20);
        let b = "sk_001";
        let err = meets_safeguards(&a, b).unwrap_err();
        assert!(err.contains("coverage_ratio"));
        assert!(err.contains("preserved_citations"));
    }

    #[test]
    fn citation_set_ignores_short_sk_tokens() {
        // `sk_` followed by a non-alphanumeric must NOT count.
        let set = citation_set("sk_ and sk.001 are not real ids");
        assert!(set.is_empty());
    }

    #[test]
    fn citation_set_picks_up_real_sk_ids() {
        let set = citation_set("see sk_001 and sk_002a, also sk_x");
        assert!(set.contains("sk_001"));
        assert!(set.contains("sk_002a"));
        assert!(set.contains("sk_x"));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn coverage_ratio_min_is_documented() {
        // Pin the documented threshold so a future change is a
        // conscious decision (catalog decision 42 + V4 §6.10).
        assert!((COVERAGE_RATIO_MIN - 0.85).abs() < 1e-6);
        assert!((PRESERVED_CITATIONS_MIN - 0.9).abs() < 1e-6);
    }
}
