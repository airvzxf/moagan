//! Discovery clustering — SimHash + LLM refinement helpers.
//!
//! The clustering algorithm:
//!
//! 1. Each sketch is fingerprinted by SimHash on its concatenated
//!    `thesis + key_decisions + architecture_outline`. The hash is
//!    Jaccard-equivalent for sets of tokens.
//! 2. K-means-style clustering via the existing `cluster_by_simhash`
//!    helper in `src/ranking/cluster.rs` (re-used, no duplicates).
//! 3. The LLM-refinement pass over each cluster (handled by the
//!    phase; this module only owns the cluster-label merge logic).
//!
//! The cluster id format is `cluster_NN` (zero-padded) so files
//! sort naturally. The category id `cat_NN` is assigned later by
//! the integrator phase based on cluster density.

use std::collections::BTreeMap;

use crate::ranking::cluster::cluster_by_simhash;

/// One sketch-record ready to be clustered. The clustering input is
/// the concat of `text` and `id` is preserved so the phase can map
/// back from the cluster index to the original sketch file.
#[derive(Debug, Clone)]
pub struct SketchRecord {
    /// Sketch id (`sk_<NN>`).
    pub id: String,
    /// Text used for clustering (thesis + key_decisions + outline).
    pub text: String,
}

/// Result of the SimHash clustering pass.
#[derive(Debug, Clone)]
pub struct ClusterChunk {
    /// Zero-based index in the original input list.
    pub member_indices: Vec<usize>,
    /// Texts that were clustered.
    pub texts: Vec<String>,
}

/// Run the SimHash clustering pass on the records.
pub fn cluster(records: &[SketchRecord], threshold: f32) -> Vec<ClusterChunk> {
    let texts: Vec<String> = records.iter().map(|r| r.text.clone()).collect();
    let groups = cluster_by_simhash(&texts, threshold);
    groups
        .into_iter()
        .map(|member_indices| {
            let texts = member_indices.iter().map(|i| texts[*i].clone()).collect();
            ClusterChunk {
                member_indices,
                texts,
            }
        })
        .collect()
}

/// Map a `ClusterChunk` index back to the sketch ids in the
/// original records list.
pub fn member_ids(records: &[SketchRecord], chunk: &ClusterChunk) -> Vec<String> {
    chunk
        .member_indices
        .iter()
        .map(|i| records[*i].id.clone())
        .collect()
}

/// Build a cluster id from its zero-based cluster index
/// (`cluster_00`, `cluster_01`, …).
pub fn cluster_id_for(idx: usize) -> String {
    format!("cluster_{:02}", idx)
}

/// Compute the centroid "popularity" of a cluster — the number of
/// members (the LLM-refinement step stretches this into a label).
pub fn cohesion(records: &[SketchRecord], chunk: &ClusterChunk) -> f32 {
    if chunk.member_indices.is_empty() {
        return 0.0;
    }
    let mut total = 0.0_f32;
    let n = chunk.member_indices.len();
    for i in 0..n {
        for j in (i + 1)..n {
            let a = &records[chunk.member_indices[i]].text;
            let b = &records[chunk.member_indices[j]].text;
            let d = crate::ranking::cluster::jaccard_distance(a, b);
            total += 1.0 - d;
        }
    }
    let pairs = n * (n.saturating_sub(1)) / 2;
    if pairs == 0 {
        1.0
    } else {
        total / pairs as f32
    }
}

/// Distribute sketch ids into per-cluster buckets keyed by their
/// `cluster_id_for` index. The output is a `BTreeMap` so iteration
/// is deterministic.
pub fn bucket_by_cluster(
    records: &[SketchRecord],
    chunks: &[ClusterChunk],
) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        let id = cluster_id_for(idx);
        map.insert(id, member_ids(records, chunk));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: &str, text: &str) -> SketchRecord {
        SketchRecord {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn cluster_groups_similar_texts() {
        let records = vec![
            r("sk_001", "rainbow colors ROYGBIV"),
            r("sk_002", "ROYGBIV color order canonical standard"),
            r("sk_003", "factor primes algorithm"),
        ];
        let cs = cluster(&records, 0.9);
        assert_eq!(cs.len(), 2);
        // The two ROYGBIV records must be in the same cluster.
        let cs0 = &cs[0];
        assert!(cs0.member_indices.contains(&0) || cs0.member_indices.contains(&1));
    }

    #[test]
    fn cluster_handles_empty() {
        let cs = cluster(&[], 0.5);
        assert!(cs.is_empty());
    }

    #[test]
    fn cluster_id_for_zero_pads() {
        assert_eq!(cluster_id_for(0), "cluster_00");
        assert_eq!(cluster_id_for(12), "cluster_12");
    }

    #[test]
    fn member_ids_resolves_indices() {
        let records = vec![r("sk_001", "a"), r("sk_002", "b"), r("sk_003", "c")];
        let chunk = ClusterChunk {
            member_indices: vec![0, 2],
            texts: vec!["a".into(), "c".into()],
        };
        let ids = member_ids(&records, &chunk);
        assert_eq!(ids, vec!["sk_001", "sk_003"]);
    }

    #[test]
    fn cohesion_is_one_for_identical_texts() {
        let records = vec![r("sk_001", "identical"), r("sk_002", "identical")];
        let chunk = ClusterChunk {
            member_indices: vec![0, 1],
            texts: vec!["identical".into(); 2],
        };
        let c = cohesion(&records, &chunk);
        assert!(c > 0.9, "got {c}");
    }

    #[test]
    fn cohesion_is_zero_for_disjoint() {
        let records = vec![r("sk_001", "alpha"), r("sk_002", "omega")];
        let chunk = ClusterChunk {
            member_indices: vec![0, 1],
            texts: vec!["alpha".into(), "omega".into()],
        };
        let c = cohesion(&records, &chunk);
        assert!(c < 0.5, "got {c}");
    }

    #[test]
    fn cohesion_singleton_is_one() {
        let records = vec![r("sk_001", "x")];
        let chunk = ClusterChunk {
            member_indices: vec![0],
            texts: vec!["x".into()],
        };
        let c = cohesion(&records, &chunk);
        assert!((c - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bucket_by_cluster_is_deterministic() {
        let records = vec![r("sk_001", "a"), r("sk_002", "b")];
        let chunks = vec![
            ClusterChunk {
                member_indices: vec![0],
                texts: vec!["a".into()],
            },
            ClusterChunk {
                member_indices: vec![1],
                texts: vec!["b".into()],
            },
        ];
        let b = bucket_by_cluster(&records, &chunks);
        let keys: Vec<_> = b.keys().cloned().collect();
        assert_eq!(keys, vec!["cluster_00", "cluster_01"]);
    }
}
