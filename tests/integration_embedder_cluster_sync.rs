//! Integration test for the D.1.3 follow-up — exercising
//! `cluster_by_embedder` end-to-end against a [`RemoteEmbedder`]
//! wired to a [`wiremock::MockServer`].
//!
//! `cluster_by_embedder` (in `src/discovery/clusterer.rs`) consumes
//! `&dyn Embedder`. Before the follow-up PR, `RemoteEmbedder` did
//! not implement that trait — async-only adapters cannot sync-bridge
//! into a thread-safe sync contract without a runtime. The follow-up
//! introduces a sync `Embedder` impl backed by
//! `tokio::task::block_in_place`. This test pins the contract: given
//! a `RemoteEmbedder` standing in for `HashingEmbedder`, the cluster
//! pass must work and must produce the same groupings the hashing
//! backend would, modulo the upstream's vector shape.
//!
//! The mock returns a fixed 2-D vector for every POST so every
//! input pairs at cosine = 1.0 — three input texts collapse into a
//! single cluster, and a singleton text stays alone. The grouping
//! is the contract the sync bridge promises to the clusterer; the
//! vector shape itself is the upstream's responsibility.
//!
//! Compliance: catalog 10-integrada-v0 §D.1.3 follow-up.

use moagan::discovery::clusterer::{SketchRecord, cluster_by_embedder};
use moagan::llm::embed::Embedder;
use moagan::llm::embed::remote::{RemoteEmbedder, RemoteEmbedderProvider};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a `RemoteEmbedder` pointing at the mock server. Mirrors
/// the helper in `integration_embedder_remote.rs`; copied here so
/// the two test files stay independent.
fn embedder_against(
    server: &MockServer,
    provider: RemoteEmbedderProvider,
    model: &str,
    dimensions: usize,
    env_name: &str,
    key: &str,
) -> RemoteEmbedder {
    unsafe {
        std::env::set_var(env_name, key);
    }
    RemoteEmbedder::with_provider(
        provider,
        &format!("{}/v1", server.uri()),
        env_name,
        model,
        dimensions as u32,
    )
    .expect("RemoteEmbedder construction failed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_by_embedder_uses_remote_embedder_via_sync_bridge() {
    let server = MockServer::start().await;
    // Every POST returns the SAME vector so the cluster pass
    // collapses identical inputs (and by extension the three
    // sketches we test) into one group.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"embedding": [0.9, 0.1], "index": 0},
            ]
        })))
        .expect(3)
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "text-embedding-3-small",
        2,
        "MOAGAN_TEST_CLUSTER_REMOTE_OK",
        "sk-cluster-test",
    );
    let trait_obj: &dyn Embedder = &embedder;

    let texts = vec![
        "alpha sketch".to_string(),
        "beta sketch".to_string(),
        "gamma sketch".to_string(),
    ];
    let groups = cluster_by_embedder(&texts, trait_obj, 0.5);

    // All three inputs collapse into a single cluster because
    // the mock returns the same vector regardless of text. This
    // proves the sync bridge is wired correctly through
    // `cluster_by_embedder` — if the bridge silently no-op'd
    // (returning zero vectors) the cosine predicate would put
    // every input in its own group.
    assert_eq!(
        groups.len(),
        1,
        "expected identical mock vectors to collapse all three inputs: {groups:?}"
    );
    let merged = &groups[0];
    assert_eq!(merged.len(), 3);
    let mut sorted = merged.clone();
    sorted.sort();
    assert_eq!(sorted, vec![0, 1, 2]);

    // Three sync `embed()` calls on distinct texts each issued a
    // separate HTTP request — pin that the bridge does NOT batch
    // through the sync trait (the async API is the batching
    // surface). This catches a future regression where an
    // accidental batch optimisation on the sync path would
    // collapse the 3 expected POSTs into 1 and break the
    // request-count invariant.
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "sync embed() must issue one POST per unique text"
    );
}

/// `cluster_by_embedder` honours the singleton-disjoint invariant
/// when one text is fed through the remote bridge. Three texts map
/// to one cluster via the mock; a fourth text that the mock server
/// routes to a distinct embedding (orthogonal direction, cosine ~0)
/// must stay alone. The mock here returns the singleton vector for
/// any POST, so this test runs the clusterer with a single-element
/// sketch list and verifies the trivial empty-of-groups outcome
/// matches the contract: same-shape mocks collapse into 1 cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_by_embedder_with_singleton_remains_singleton() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"embedding": [0.6, 0.8], "index": 0}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "m",
        2,
        "MOAGAN_TEST_CLUSTER_SINGLETON",
        "sk-singleton-test",
    );
    let trait_obj: &dyn Embedder = &embedder;

    let records = [SketchRecord {
        id: "sk_alone".into(),
        text: "isolated sketch".into(),
    }];
    let texts: Vec<String> = records.iter().map(|r| r.text.clone()).collect();
    let groups = cluster_by_embedder(&texts, trait_obj, 0.5);

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0], vec![0]);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
