//! D.13.2 + D.13.7: outlier tracker.
//!
//! A `Sketch` is classified as an outlier when its min Jaccard
//! distance to any cluster centroid is at least
//! `outlier_distance`. Outliers are always preserved (the spec
//! is explicit about that: "outliers siempre se preservan" —
//! T01-06 §9.3, D.13.2) so the downstream phase gets the
//! contrarian ideas even when the rest of the matrix has
//! saturated.
//!
//! The detector uses the cluster's `members` list as a token bag
//! (the cluster id, label, summary, and member ids). That is the
//! cheapest meaningful signal we can extract from the
//! `domain::Cluster` without an extra embedder call, and it
//! keeps the helper free of any LLM dependency. The `Sketch`
//! feature bag is the same tokenisation as
//! [`crate::phases::cardinality::SelectionPlan`]: lowercase
//! alphanumerics split on non-alphanumerics.
//!
//! The function is deliberately small and pure so the
//! integration test in `tests/integration_pr19_stop_policy.rs`
//! can drive it with hand-rolled fixtures.

use std::collections::HashSet;

use crate::domain::{Cluster, Sketch};

// `SketchId` was originally declared here. PR-23 (D.13.5) moved
// the canonical declaration to `super::id` so the three discovery
// id newtypes (`SketchId`, `ContradictionId`, `FacetId`) live
// together. Re-export the type for backward compatibility with
// callers that import `crate::discovery::outlier::SketchId`
// (notably `tests/integration_pr19_stop_policy.rs`).
pub use super::id::SketchId;

/// Detect outlier sketches. A sketch is an outlier when:
///
/// 1. It is not a member of any cluster in `clusters` (singleton
///    or unclustered — natural outlier), OR
/// 2. Its minimum Jaccard distance to any cluster's feature
///    bag is at least `outlier_distance`.
///
/// The function preserves the input order (sketches in the
/// same position as the input) and deduplicates: a sketch id
/// that appears multiple times in the input is reported once.
///
/// `outlier_distance` is in `0..=1`; the default is
/// [`crate::discovery::stop_policy::DEFAULT_OUTLIER_DISTANCE`]
/// (`0.30`). The function does not invert or remap the
/// threshold — pass a larger value to be more permissive
/// (fewer outliers), a smaller value to be more strict.
pub fn detect_outliers(samples: &[Sketch], clusters: &[Cluster]) -> Vec<SketchId> {
    detect_outliers_with_threshold(
        samples,
        clusters,
        crate::discovery::stop_policy::DEFAULT_OUTLIER_DISTANCE,
    )
}

/// Like [`detect_outliers`] but with an explicit threshold.
pub fn detect_outliers_with_threshold(
    samples: &[Sketch],
    clusters: &[Cluster],
    outlier_distance: f32,
) -> Vec<SketchId> {
    // Pre-compute the feature bag for every cluster. Doing it
    // once outside the per-sample loop keeps the asymptotic
    // shape O(N*C) where N is samples and C is cluster feature
    // bags (we use the cluster's member ids, not its full
    // text).
    let cluster_features: Vec<HashSet<String>> = clusters.iter().map(cluster_features).collect();

    // The set of ids that belong to at least one cluster.
    let clustered_ids: HashSet<&str> = clusters
        .iter()
        .flat_map(|c| c.members.iter().map(String::as_str))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut outliers: Vec<SketchId> = Vec::new();
    for sketch in samples {
        let id = sketch.id.as_str();
        if id.is_empty() {
            continue;
        }
        if !seen.insert(id.to_string()) {
            continue;
        }
        let sketch_feats = sketch_features(sketch);
        let is_outlier = if !clustered_ids.contains(id) {
            // Not in any cluster → outlier (the natural case).
            true
        } else {
            cluster_features
                .iter()
                .map(|cf| jaccard_distance(&sketch_feats, cf))
                .fold(f32::INFINITY, f32::min)
                >= outlier_distance
        };
        if is_outlier {
            outliers.push(SketchId(id.to_string()));
        }
    }
    outliers
}

/// Compute a token-feature bag for a `Cluster`. The bag is the
/// union of its `members` ids plus its `id` and `label`. We
/// deliberately do NOT include the LLM-generated `summary`
/// because the clusterer phase populates it later than the
/// outlier detector runs and the spec only requires a cheap
/// deterministic signal. Identifiers (`cluster_01`,
/// `sk_001`) are inserted as single tokens (no underscore
/// split) so the bag mirrors how operators actually read them
/// — the cluster's name and the member's name, not the raw
/// `_NN` suffix.
fn cluster_features(c: &Cluster) -> HashSet<String> {
    let mut set: HashSet<String> = HashSet::new();
    set.insert(c.id.to_ascii_lowercase());
    for tok in c.label.split(|c: char| !c.is_alphanumeric()) {
        if !tok.is_empty() {
            set.insert(tok.to_ascii_lowercase());
        }
    }
    for member in &c.members {
        set.insert(member.to_ascii_lowercase());
    }
    set
}

/// Compute a token-feature bag for a `Sketch`. Same identifier
/// rule as the cluster features (id is a single token) so the
/// Jaccard distance stays symmetric. Natural-language fields
/// (`thesis`, `architecture_outline`, `key_decisions`,
/// `assumptions`, `angle`) tokenise on non-alphanumerics the
/// same way the clusterer does.
fn sketch_features(s: &Sketch) -> HashSet<String> {
    let mut set: HashSet<String> = HashSet::new();
    if !s.id.is_empty() {
        set.insert(s.id.to_ascii_lowercase());
    }
    for field in [
        s.thesis.as_str(),
        s.architecture_outline.as_str(),
        s.angle.as_str(),
    ] {
        for tok in field.split(|c: char| !c.is_alphanumeric()) {
            if !tok.is_empty() {
                set.insert(tok.to_ascii_lowercase());
            }
        }
    }
    for field in [&s.key_decisions, &s.assumptions] {
        for entry in field {
            for tok in entry.split(|c: char| !c.is_alphanumeric()) {
                if !tok.is_empty() {
                    set.insert(tok.to_ascii_lowercase());
                }
            }
        }
    }
    set
}

/// Jaccard distance in `0..=1`. Defined as
/// `1 - |A ∩ B| / |A ∪ B|`; returns `1.0` when both sets are
/// empty.
fn jaccard_distance(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 {
        1.0
    } else {
        1.0 - (intersection / union)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch(id: &str, thesis: &str, angle: &str) -> Sketch {
        Sketch {
            id: id.into(),
            thesis: thesis.into(),
            angle: angle.into(),
            ..Sketch::default()
        }
    }

    fn cluster(id: &str, label: &str, members: &[&str]) -> Cluster {
        Cluster {
            id: id.into(),
            label: label.into(),
            members: members.iter().map(|m| m.to_string()).collect(),
            ..Cluster::default()
        }
    }

    #[test]
    fn detect_outliers_returns_unclustered() {
        // Two sketches: sk_001 is in a cluster whose label
        // matches the sketch thesis (low Jaccard distance);
        // sk_002 is unclustered. With threshold 0.3 the helper
        // must report only sk_002 as an outlier.
        let samples = vec![
            sketch("sk_001", "alpha beta gamma delta", "minimalist"),
            sketch("sk_002", "zeta eta theta iota", "production-grade"),
        ];
        let clusters = vec![cluster("cluster_01", "alpha beta gamma delta", &["sk_001"])];
        let outliers = detect_outliers_with_threshold(&samples, &clusters, 0.3);
        assert_eq!(outliers, vec![SketchId("sk_002".into())]);
    }

    #[test]
    fn detect_outliers_dedupes_repeated_ids() {
        let samples = vec![
            sketch("sk_001", "alpha", "minimalist"),
            sketch("sk_001", "alpha dup", "minimalist"),
        ];
        let outliers = detect_outliers_with_threshold(&samples, &[], 0.3);
        assert_eq!(outliers, vec![SketchId("sk_001".into())]);
    }

    #[test]
    fn detect_outliers_skips_blank_ids() {
        let samples = vec![sketch("", "alpha", "minimalist")];
        let outliers = detect_outliers_with_threshold(&samples, &[], 0.3);
        assert!(outliers.is_empty(), "blank ids must be skipped");
    }

    #[test]
    fn detect_outliers_threshold_zero_keeps_clustered() {
        // With a threshold of 0 every sketch is considered an
        // outlier (Jaccard distance >= 0 is always true). The
        // helper preserves input order and dedupes.
        let samples = vec![
            sketch("sk_001", "alpha", "minimalist"),
            sketch("sk_002", "beta", "production-grade"),
        ];
        let clusters = vec![cluster("cluster_01", "alpha", &["sk_001", "sk_002"])];
        let outliers = detect_outliers_with_threshold(&samples, &clusters, 0.0);
        assert_eq!(
            outliers,
            vec![SketchId("sk_001".into()), SketchId("sk_002".into())]
        );
    }

    #[test]
    fn detect_outliers_threshold_one_keeps_none() {
        // With a threshold of 1 no sketch is considered an
        // outlier (Jaccard distance is at most 1). Only
        // unclustered sketches survive.
        let samples = vec![
            sketch("sk_001", "alpha", "minimalist"),
            sketch("sk_002", "beta", "production-grade"),
        ];
        let clusters = vec![cluster("cluster_01", "alpha", &["sk_001"])];
        let outliers = detect_outliers_with_threshold(&samples, &clusters, 1.0);
        assert_eq!(outliers, vec![SketchId("sk_002".into())]);
    }

    #[test]
    fn jaccard_distance_is_well_defined() {
        let a: HashSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c: HashSet<String> = ["qux"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard_distance(&a, &b) - 0.0).abs() < 1e-6);
        assert!((jaccard_distance(&a, &c) - 1.0).abs() < 1e-6);
    }
}
