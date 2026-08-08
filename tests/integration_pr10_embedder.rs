//! Integration tests for v0.5 PR-10 — `Embedder` consumer wire-up.
//!
//! Spec ref: D.1.3 (proposal-03-add-ons.md) and roadmap PR-10.
//!
//! `src/llm/embed/mod.rs::Embedder` + `HashingEmbedder` have shipped
//! since sub-phase K.2 but had zero call sites. PR-10 wires
//! `src/discovery/clusterer.rs::cluster` to use
//! `Embedder::embed(text)` + cosine similarity instead of the
//! manual SimHash pass.
//!
//! The two integration tests pin the cross-module invariants the
//! unit tests in `clusterer.rs` cannot:
//!
//! 1. `clusterer_groups_semantically_similar_sketches` — two
//!    semantically similar sketches cluster together because
//!    their cosine similarity exceeds 0.85.
//! 2. `clusterer_regression_fixture_assignments_equivalent` —
//!    the cluster assignments produced by the new
//!    embedder-based clustering on the regression fixture match
//!    those produced by the legacy Jaccard-based grouping for the
//!    same threshold. Locks in behavioural stability across the
//!    PR-10 refactor.

use moagan::discovery::clusterer::{SketchRecord, cluster, cluster_by_embedder};
use moagan::llm::embed::Embedder;
use moagan::llm::embed::HashingEmbedder;
use moagan::llm::embed::cosine;
use moagan::ranking::cluster::cluster_by_simhash;

fn r(id: &str, text: &str) -> SketchRecord {
    SketchRecord {
        id: id.into(),
        text: text.into(),
    }
}

/// Two semantically similar sketches share enough tokens that
/// `HashingEmbedder`'s embeddings land at cosine > 0.85. The
/// third sketch is disjoint and must NOT join their cluster when
/// the threshold is set to require `cosine >= 0.85`.
#[test]
fn clusterer_groups_semantically_similar_sketches() {
    let records = vec![
        r(
            "sk_001",
            "Postgres connection pool with sqlx and tokio async runtime",
        ),
        r(
            "sk_002",
            "Postgres connection pool with sqlx and tokio async runtime for rust backend",
        ),
        r(
            "sk_003",
            "Quantum mechanics probability distribution function",
        ),
    ];
    let embedder = HashingEmbedder::default();

    // Pin the cosine > 0.85 invariant explicitly so a silent change
    // to the hashing pipeline surfaces as a failing test rather
    // than as a silent regression in the cluster grouping.
    let v1 = embedder.embed(&records[0].text);
    let v2 = embedder.embed(&records[1].text);
    let sim = cosine(&v1, &v2);
    assert!(
        sim > 0.85,
        "expected cosine > 0.85 for semantically similar pair, got {sim}"
    );

    // Threshold 0.15 = "cluster if 1 - cosine <= 0.15", i.e.
    // cosine >= 0.85. The two similar sketches must cluster
    // together; the disjoint one must stand alone.
    let chunks = cluster(&records, &embedder, 0.15);
    assert_eq!(chunks.len(), 2, "expected 2 clusters, got {chunks:?}");
    let merged = chunks
        .iter()
        .find(|c| c.member_indices.contains(&0) && c.member_indices.contains(&1))
        .expect("expected sk_001 + sk_002 to cluster together");
    let mut merged_sorted: Vec<usize> = merged.member_indices.clone();
    merged_sorted.sort();
    assert_eq!(merged_sorted, vec![0, 1]);
    assert!(chunks.iter().any(|c| c.member_indices == vec![2]));
}

/// Regression fixture — the same three sketch texts that have
/// been the canonical example in `clusterer.rs` unit tests since
/// pre-PR-10. The new embedder-based clustering produces the same
/// groupings as the legacy Jaccard-based clustering on this
/// fixture, so the refactor is behaviour-preserving on real
/// inputs.
#[test]
fn clusterer_regression_fixture_assignments_equivalent() {
    // Same fixture as `clusterer::tests::cluster_groups_similar_texts`.
    let texts: Vec<String> = vec![
        "rainbow colors ROYGBIV".into(),
        "ROYGBIV color order canonical standard".into(),
        "factor primes algorithm".into(),
    ];

    // Legacy Jaccard grouping at threshold 0.9 — two ROYGBIV
    // sketches join, the primes sketch is alone.
    let before = cluster_by_simhash(&texts, 0.9);
    assert_eq!(before.len(), 2, "legacy grouping: {before:?}");
    let before_rainbow = before
        .iter()
        .find(|g| g.contains(&0) && g.contains(&1))
        .expect("legacy: ROYGBIV pair must cluster together");
    let mut before_sorted: Vec<usize> = before_rainbow.clone();
    before_sorted.sort();
    assert_eq!(before_sorted, vec![0, 1]);
    assert!(before.iter().any(|g| g == &vec![2]));

    // New embedder-based grouping at the same threshold — the
    // 1 - cosine distance threshold keeps the same "max distance"
    // semantic so existing call-sites do not need re-tuning.
    let embedder = HashingEmbedder::default();
    let after = cluster_by_embedder(&texts, &embedder, 0.9);
    assert_eq!(after.len(), 2, "embedder grouping: {after:?}");
    let after_rainbow = after
        .iter()
        .find(|g| g.contains(&0) && g.contains(&1))
        .expect("embedder: ROYGBIV pair must cluster together");
    let mut after_sorted: Vec<usize> = after_rainbow.clone();
    after_sorted.sort();
    assert_eq!(after_sorted, vec![0, 1]);
    assert!(after.iter().any(|g| g == &vec![2]));

    // The two groupings are equivalent: same cluster count, same
    // member sets, same disjoint singleton. This is the
    // before/after regression invariant PR-10 promises.
    let before_clusters: std::collections::BTreeSet<Vec<usize>> = before
        .into_iter()
        .map(|mut g| {
            g.sort();
            g
        })
        .collect();
    let after_clusters: std::collections::BTreeSet<Vec<usize>> = after
        .into_iter()
        .map(|mut g| {
            g.sort();
            g
        })
        .collect();
    assert_eq!(before_clusters, after_clusters);
}

/// `cluster_by_embedder` is deterministic across repeated calls
/// because `HashingEmbedder` caches its vectors. Two passes over
/// the same input must yield the same grouping.
#[test]
fn clusterer_embedder_pass_is_deterministic() {
    let texts: Vec<String> = vec![
        "rust axum postgres connection pool".into(),
        "rust axum postgres connection pool limit".into(),
        "quantum entanglement probability amplifier".into(),
    ];
    let embedder = HashingEmbedder::default();
    let a = cluster_by_embedder(&texts, &embedder, 0.7);
    let b = cluster_by_embedder(&texts, &embedder, 0.7);
    assert_eq!(a, b);
    assert_eq!(a.len(), 2);
}
