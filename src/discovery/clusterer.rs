//! Discovery clustering — Embedder + cosine similarity (D.1.3).
//!
//! The clustering algorithm:
//!
//! 1. Each sketch is embedded via the injected [`Embedder`]
//!    (default `HashingEmbedder`, 256-dim FNV-1a). Embeddings are
//!    L2-normalised so cosine similarity collapses to a dot
//!    product.
//! 2. Pairwise clustering via union-find: two records join the
//!    same cluster when `1 - cosine <= threshold`, i.e.
//!    `cosine >= 1 - threshold`. The threshold keeps the same
//!    "max distance" semantic as the previous Jaccard-based pass
//!    so existing call-sites do not need re-tuning.
//! 3. The LLM-refinement pass over each cluster (handled by the
//!    phase; this module only owns the cluster-label merge logic).
//!
//! The cluster id format is `cluster_NN` (zero-padded) so files
//! sort naturally. The category id `cat_NN` is assigned later by
//! the integrator phase based on cluster density.
//!
//! Dependency injection via `&dyn Embedder` keeps the door open for
//! the `RemoteEmbedder` and `fastembed` adapters (D.1.3 follow-up,
//! deferred) without touching this module's API.

use std::collections::BTreeMap;

use rayon::prelude::*;

use crate::llm::embed::{Embedder, cosine};
use crate::ranking::cluster::jaccard_distance;

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

/// Result of the embedder-based clustering pass.
#[derive(Debug, Clone)]
pub struct ClusterChunk {
    /// Zero-based index in the original input list.
    pub member_indices: Vec<usize>,
    /// Texts that were clustered.
    pub texts: Vec<String>,
}

/// Run the embedder-based clustering pass on the records. Two
/// records join the same cluster when the cosine similarity of
/// their embeddings exceeds `1 - threshold`. The `threshold` keeps
/// the same "max distance" semantic as the previous Jaccard-based
/// pass so existing call-sites do not need re-tuning.
pub fn cluster(
    records: &[SketchRecord],
    embedder: &dyn Embedder,
    threshold: f32,
) -> Vec<ClusterChunk> {
    tracing::debug!(
        records = records.len(),
        threshold,
        "clusterer: cluster enter"
    );
    let texts: Vec<String> = records.iter().map(|r| r.text.clone()).collect();
    let groups = cluster_by_embedder(&texts, embedder, threshold);
    let chunks: Vec<ClusterChunk> = groups
        .into_iter()
        .map(|member_indices| {
            let texts = member_indices.iter().map(|i| texts[*i].clone()).collect();
            ClusterChunk {
                member_indices,
                texts,
            }
        })
        .collect();
    tracing::info!(
        clusters = chunks.len(),
        records = records.len(),
        "clusterer: cluster done"
    );
    chunks
}

/// Embedder-based clustering helper. Embeds each text via
/// `embedder`, then uses union-find over the
/// `1 - cosine <= threshold` predicate. Pairs whose cosine
/// similarity is at least `1 - threshold` join the same cluster.
///
/// Exposed for integration tests that want to compare the
/// embedder-based grouping against the legacy Jaccard grouping
/// without going through the `SketchRecord` wrapper.
pub fn cluster_by_embedder(
    texts: &[String],
    embedder: &dyn Embedder,
    threshold: f32,
) -> Vec<Vec<usize>> {
    tracing::debug!(
        texts = texts.len(),
        threshold,
        "clusterer: cluster_by_embedder enter"
    );
    let n = texts.len();
    let embeddings: Vec<Vec<f32>> = texts.iter().map(|t| embedder.embed(t)).collect();
    tracing::trace!(
        embeddings = embeddings.len(),
        dim = embeddings.first().map(|e| e.len()).unwrap_or(0),
        "clusterer: embeddings computed"
    );
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
    let mut pair_count = 0usize;
    for i in 0..n {
        for j in (i + 1)..n {
            if 1.0 - cosine(&embeddings[i], &embeddings[j]) <= threshold {
                union(&mut parent, i, j);
                pair_count += 1;
            }
        }
    }
    tracing::trace!(pair_count, "clusterer: pair scan done");
    let mut clusters: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }
    let out: Vec<Vec<usize>> = clusters.into_values().collect();
    tracing::debug!(
        clusters = out.len(),
        pair_count,
        "clusterer: cluster_by_embedder done"
    );
    out
}

/// Map a `ClusterChunk` index back to the sketch ids in the
/// original records list.
pub fn member_ids(records: &[SketchRecord], chunk: &ClusterChunk) -> Vec<String> {
    let out: Vec<String> = chunk
        .member_indices
        .iter()
        .map(|i| records[*i].id.clone())
        .collect();
    tracing::trace!(members = out.len(), "clusterer: member_ids resolved");
    out
}

/// Build a cluster id from its zero-based cluster index
/// (`cluster_00`, `cluster_01`, …).
pub fn cluster_id_for(idx: usize) -> String {
    tracing::trace!(idx, "clusterer: cluster_id_for");
    format!("cluster_{:02}", idx)
}

/// Compute the centroid "popularity" of a cluster — the number of
/// members (the LLM-refinement step stretches this into a label).
///
/// **F4 (parallel CPU-bound)**: the `O(n²)` Jaccard pair scan is
/// distributed across `n` outer iterations via `rayon::par_iter`.
/// Each row `(i, ..)` independently sums `(i+1..n)` Jaccard
/// similarities, and the per-row partial sums are reduced with a
/// final `sum()`. The pair count is the same as the sequential
/// reference; only the addition order changes. Float sum order can
/// permute ULPs, so callers that compare `cohesion` across
/// versions should allow `1e-6` tolerance — the unit tests in this
/// module use that threshold.
pub fn cohesion(records: &[SketchRecord], chunk: &ClusterChunk) -> f32 {
    if chunk.member_indices.is_empty() {
        tracing::trace!("clusterer: cohesion empty cluster");
        return 0.0;
    }
    let n = chunk.member_indices.len();
    tracing::debug!(n, "clusterer: cohesion enter");
    // Distribute the upper-triangular row scan across cores. The
    // inner `(i + 1..n)` loop stays sequential per row because the
    // chunk size is small (5–10 in the operator's runs) and
    // bridging it to a parallel iterator costs more than the
    // work it would save.
    let total: f32 = (0..n)
        .into_par_iter()
        .map(|i| {
            let mut row_total = 0.0_f32;
            for j in (i + 1)..n {
                let a = &records[chunk.member_indices[i]].text;
                let b = &records[chunk.member_indices[j]].text;
                let d = jaccard_distance(a, b);
                row_total += 1.0 - d;
            }
            row_total
        })
        .sum();
    let pairs = n * (n.saturating_sub(1)) / 2;
    if pairs == 0 {
        tracing::trace!(n, "clusterer: cohesion singleton");
        1.0
    } else {
        let v = total / pairs as f32;
        tracing::trace!(n, pairs, value = v, "clusterer: cohesion done");
        v
    }
}

/// Distribute sketch ids into per-cluster buckets keyed by their
/// `cluster_id_for` index. The output is a `BTreeMap` so iteration
/// is deterministic.
pub fn bucket_by_cluster(
    records: &[SketchRecord],
    chunks: &[ClusterChunk],
) -> BTreeMap<String, Vec<String>> {
    tracing::debug!(
        records = records.len(),
        clusters = chunks.len(),
        "clusterer: bucket_by_cluster"
    );
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
    use crate::llm::embed::HashingEmbedder;

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
        let embedder = HashingEmbedder::default();
        let cs = cluster(&records, &embedder, 0.9);
        assert_eq!(cs.len(), 2);
        // The two ROYGBIV records must be in the same cluster.
        let merged = cs
            .iter()
            .find(|c| c.member_indices.contains(&0) && c.member_indices.contains(&1));
        assert!(merged.is_some(), "expected sk_001 + sk_002 to merge");
    }

    #[test]
    fn cluster_handles_empty() {
        let embedder = HashingEmbedder::default();
        let cs = cluster(&[], &embedder, 0.5);
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

    #[test]
    fn cluster_by_embedder_merges_close_texts() {
        // Two texts sharing most tokens yield cosine > 0.85. The
        // third is disjoint. Threshold 0.15 (cosine >= 0.85) merges
        // only the similar pair.
        let texts = vec![
            "Postgres connection pool with sqlx and tokio async runtime".to_string(),
            "Postgres connection pool with sqlx and tokio async runtime for rust backend"
                .to_string(),
            "Quantum mechanics probability distribution function".to_string(),
        ];
        let embedder = HashingEmbedder::default();
        let v0 = embedder.embed(&texts[0]);
        let v1 = embedder.embed(&texts[1]);
        let sim = crate::llm::embed::cosine(&v0, &v1);
        assert!(sim > 0.85, "expected cosine > 0.85, got {sim}");
        let groups = cluster_by_embedder(&texts, &embedder, 0.15);
        assert_eq!(groups.len(), 2);
        let merged = groups
            .iter()
            .find(|g| g.contains(&0) && g.contains(&1))
            .expect("expected texts[0] + texts[1] to cluster together");
        let mut sorted = merged.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1]);
        assert!(groups.iter().any(|g| g == &vec![2]));
    }

    // -----------------------------------------------------------------
    // F4 snapshot tests for `cohesion`. Float sum order can change
    // by a few ULPs when the reduction tree is parallel, so the
    // comparison allows a `1e-6` tolerance as documented in the
    // function's docstring. The pair-set itself is identical
    // because the parallel iterator enumerates exactly the same
    // `(i, j)` upper-triangular pairs as the sequential loop.
    // -----------------------------------------------------------------

    /// Pre-rayon sequential reference for `cohesion`. Kept as a
    /// private helper so the F4 snapshot test can compare the
    /// parallel reduction against the exact algorithm the refactor
    /// replaced.
    fn cohesion_sequential(records: &[SketchRecord], chunk: &ClusterChunk) -> f32 {
        if chunk.member_indices.is_empty() {
            return 0.0;
        }
        let mut total = 0.0_f32;
        let n = chunk.member_indices.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let a = &records[chunk.member_indices[i]].text;
                let b = &records[chunk.member_indices[j]].text;
                let d = jaccard_distance(a, b);
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

    /// Build a deterministic cluster fixture of `size` records
    /// with varied token features. The seed is fixed so re-runs
    /// produce the same numbers; the test compares parallel vs
    /// sequential reductions, not two RNG outputs.
    fn fixture_records(size: usize) -> Vec<SketchRecord> {
        let mut rng = fastrand::Rng::with_seed(0xC0FF_EE42_DEAD_BEEF);
        (0..size)
            .map(|i| {
                let mut text = String::new();
                let word_count = rng.usize(6..14);
                for w in 0..word_count {
                    if w > 0 {
                        text.push(' ');
                    }
                    let word_len = rng.usize(4..9);
                    for _ in 0..word_len {
                        text.push(rng.u8(b'a'..=b'z') as char);
                    }
                }
                SketchRecord {
                    id: format!("sk_{i:03}"),
                    text,
                }
            })
            .collect()
    }

    /// F4 snapshot: `cohesion` over a 20-record cluster must match
    /// the pre-rayon sequential reference within `1e-6`. The
    /// `1e-6` tolerance covers float sum reorder; the pair set is
    /// identical because the parallel iterator enumerates the same
    /// `(i, j)` upper-triangular indices.
    #[test]
    fn cohesion_parallel_matches_sequential_size_20() {
        let records = fixture_records(20);
        let chunk = ClusterChunk {
            member_indices: (0..20).collect(),
            texts: records.iter().map(|r| r.text.clone()).collect(),
        };
        let parallel = cohesion(&records, &chunk);
        let sequential = cohesion_sequential(&records, &chunk);
        assert!(
            (parallel - sequential).abs() < 1e-6,
            "cohesion rayon refactor must match the sequential reference within 1e-6; parallel={parallel} sequential={sequential}"
        );
    }

    /// F4 mini-benchmark: confirms `cohesion` over a 20-record
    /// cluster completes in a reasonable time. The `O(n²)` pair
    /// count is 190, distributed across cores by `rayon::par_iter`.
    #[test]
    fn cohesion_parallel_completes_in_reasonable_time() {
        let records = fixture_records(20);
        let chunk = ClusterChunk {
            member_indices: (0..20).collect(),
            texts: records.iter().map(|r| r.text.clone()).collect(),
        };
        let start = std::time::Instant::now();
        let _ = cohesion(&records, &chunk);
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "cohesion(20) took {elapsed:?} — parallel path is hung"
        );
    }
}
