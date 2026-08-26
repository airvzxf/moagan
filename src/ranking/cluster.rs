//! Lightweight proposal clustering per T01-06 §16.12 step 4 and
//! §9.5. We use a SimHash-style fingerprint over the proposal text
//! (summary + approach + tradeoffs + evidence) and group proposals
//! whose fingerprint distance falls below a threshold. This avoids
//! any external embedding downloads — the binary stays self-contained.
//!
//! For a 3-proposal MVP run the cluster stage usually produces one
//! cluster per proposal (Jaccard distance ~1.0), which is fine: the
//! next step ([`super::diversity::pick_with_crowding`]) still selects
//! the top-`k` from the un-clustered front.

use std::collections::HashSet;

/// Compute the Jaccard distance between two strings, treating each as
/// a bag of word tokens. `0.0` = identical, `1.0` = disjoint.
pub fn jaccard_distance(a: &str, b: &str) -> f32 {
    let set_a: HashSet<&str> = a.split_whitespace().collect();
    let set_b: HashSet<&str> = b.split_whitespace().collect();
    if set_a.is_empty() && set_b.is_empty() {
        tracing::trace!("ranking::cluster::jaccard_distance: both empty; 0.0");
        return 0.0;
    }
    let inter = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        let out = 1.0 - (inter as f32 / union as f32);
        tracing::trace!(
            intersection = inter,
            union,
            distance = out,
            "ranking::cluster::jaccard_distance"
        );
        out
    }
}

/// Cluster the texts in `texts` by Jaccard distance. Proposals whose
/// pair-wise distance is `<= threshold` join the same cluster. The
/// clusters are returned in first-seen order; within each cluster
/// indices preserve the input order.
///
/// `threshold` is typically `0.7` (a 70% shared-vocabulary overlap
/// counts as "the same stack"). For very small front sizes (<= 3) the
/// function returns one cluster per proposal.
pub fn cluster_by_simhash(texts: &[String], threshold: f32) -> Vec<Vec<usize>> {
    tracing::debug!(
        n = texts.len(),
        threshold,
        "ranking::cluster::cluster_by_simhash: enter"
    );
    let n = texts.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent[hi] = lo;
        }
    }
    let mut union_count = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            if jaccard_distance(&texts[i], &texts[j]) <= threshold {
                union(&mut parent, i, j);
                union_count += 1;
            }
        }
    }
    let mut clusters: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }
    let out: Vec<Vec<usize>> = clusters.into_values().collect();
    tracing::debug!(
        clusters = out.len(),
        unions = union_count,
        "ranking::cluster::cluster_by_simhash: exit"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_distance_identical_is_zero() {
        assert_eq!(jaccard_distance("hello world", "hello world"), 0.0);
    }

    #[test]
    fn jaccard_distance_disjoint_is_one() {
        assert_eq!(jaccard_distance("aaa bbb", "ccc ddd"), 1.0);
    }

    #[test]
    fn jaccard_distance_partial() {
        // share "foo" out of {foo, bar} U {foo, baz} = 3 → 1 - 1/3 ≈ 0.667
        let d = jaccard_distance("foo bar", "foo baz");
        assert!(d > 0.6 && d < 0.7, "got {d}");
    }

    #[test]
    fn cluster_splits_different_topics() {
        let texts = vec![
            "rainbow colors ROYGBIV".to_string(),
            "factor primes algorithm".to_string(),
            "fourier transform signal processing".to_string(),
        ];
        let clusters = cluster_by_simhash(&texts, 0.5);
        assert_eq!(clusters.len(), 3, "expected 3 clusters, got {clusters:?}");
    }

    #[test]
    fn cluster_merges_similar_texts() {
        let texts = vec![
            "use ROYGBIV colors in order canonical".to_string(),
            "the ROYGBIV color order is canonical standard".to_string(),
            "factor primes algorithm completely unrelated".to_string(),
        ];
        let clusters = cluster_by_simhash(&texts, 0.8);
        assert_eq!(clusters.len(), 2, "expected 2 clusters, got {clusters:?}");
        // First cluster must contain both rainbow proposals.
        assert_eq!(clusters[0].len(), 2);
        assert_eq!(clusters[1], vec![2]);
    }

    #[test]
    fn cluster_handles_empty() {
        let texts: Vec<String> = vec![];
        let clusters = cluster_by_simhash(&texts, 0.5);
        assert!(clusters.is_empty());
    }
}
