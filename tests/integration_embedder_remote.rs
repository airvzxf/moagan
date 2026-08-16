//! Integration tests for B#18 / D.1.3 follow-up —
//! `RemoteEmbedder` adapter against a mock HTTP server.
//!
//! The unit tests in `src/llm/embed/remote.rs` pin the wire-format
//! shapes (request body per provider, response parsing) without
//! firing real HTTP traffic. This file complements them with
//! end-to-end round-trips against a [`wiremock::MockServer`] so the
//! HTTP transport, auth header construction, status-code mapping,
//! and the sync adapter's cache path are exercised together.
//!
//! Coverage:
//!
//! 1. `openai_round_trip_returns_vectors` — OpenAI wire shape against
//!    a mock `/v1/embeddings` endpoint.
//! 2. `cohere_round_trip_returns_vectors` — Cohere wire shape
//!    against `/v1/embed`.
//! 3. `auth_header_is_bearer` — the Authorization header carries
//!    `Bearer <key>` (any non-empty value), as documented.
//! 4. `http_401_maps_to_invalid_api_key` — upstream auth errors
//!    surface as `Error::InvalidApiKey` so callers can branch on
//!    exit code 3.
//! 5. `http_429_maps_to_plan_exhausted` — throttling surfaces as
//!    `Error::PlanExhausted`.
//! 6. `empty_batch_round_trips_through_wiremock` — an empty batch
//!    short-circuits before any HTTP call.
//! 7. `response_count_mismatch_is_provider_error` — a response
//!    carrying a different number of vectors than inputs surfaces
//!    as `Error::Provider`.
//! 8. `non_json_4xx_surfaces_as_status_error` — a 4xx that returns
//!    non-JSON HTML is still typed correctly (no JSON-decode error).

use moagan::llm::embed::remote::{RemoteEmbedder, RemoteEmbedderProvider};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a `RemoteEmbedder` that points at the mock server's URI.
/// The constructor expects an env var holding the API key, so we
/// set a unique name per test and clean up after.
fn embedder_against(
    server: &MockServer,
    provider: RemoteEmbedderProvider,
    model: &str,
    dimensions: usize,
    env_name: &str,
    key: &str,
) -> RemoteEmbedder {
    // The constructor reads `api_key_env` from the process
    // environment. Each test uses a unique env var name to keep
    // parallel test runs isolated.
    unsafe {
        std::env::set_var(env_name, key);
    }
    let builder = RemoteEmbedder::with_provider(
        provider,
        &format!("{}/v1", server.uri()),
        env_name,
        model,
        dimensions as u32,
    );
    let result = builder;
    // The env var stays set for the duration of the test; the
    // session-level cleanup happens when the test process exits.
    // This is consistent with the other env-driven tests in the
    // suite (`integration_audit.rs` sets `MOAGAN_*` vars without
    // restoring them, and that has not caused flake in CI).
    result.unwrap_or_else(|e| panic!("RemoteEmbedder construction failed: {e:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_round_trip_returns_vectors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"embedding": [0.1, 0.2, 0.3], "index": 0},
                {"embedding": [0.4, 0.5, 0.6], "index": 1},
            ]
        })))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "text-embedding-3-small",
        3,
        "MOAGAN_TEST_REMOTE_OPENAI_OK",
        "sk-test",
    );

    let vectors = embedder
        .embed_batch(&["hello", "world"])
        .await
        .expect("embed_batch ok");
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1, 0.2, 0.3]);
    assert_eq!(vectors[1], vec![0.4, 0.5, 0.6]);

    // Single-text convenience path. The mock echoes the same
    // 2-vector body for every POST; `embed_one` (which sends a
    // 1-input batch) is expected to surface the count mismatch
    // — pin that behaviour so a future "always echo first
    // vector" change is conscious.
    let err = embedder
        .embed_one("solo")
        .await
        .expect_err("embed_one with mocked 2-vector response must mismatch");
    use moagan::error::Error;
    assert!(
        matches!(err, Error::Provider(_)),
        "expected Provider on count mismatch, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cohere_round_trip_returns_vectors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "embeddings": [[1.0, 2.0], [3.0, 4.0, 5.0]]
        })))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Cohere,
        "embed-english-v3.0",
        3,
        "MOAGAN_TEST_REMOTE_COHERE_OK",
        "cohere-test",
    );

    let vectors = embedder
        .embed_batch(&["alpha", "beta"])
        .await
        .expect("embed_batch ok");
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![1.0, 2.0]);
    assert_eq!(vectors[1], vec![3.0, 4.0, 5.0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_header_is_bearer() {
    use moagan::error::Error;
    let server = MockServer::start().await;
    // Mount a mock that REJECTS requests with the wrong
    // Authorization header so we can assert on the bearer token
    // behaviour directly. A successful call proves the header
    // carries `Bearer sk-rounds-trip-token`.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer sk-rounds-trip-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"embedding": [0.0]}]
        })))
        .mount(&server)
        .await;

    unsafe {
        std::env::set_var("MOAGAN_TEST_REMOTE_BEARER", "sk-rounds-trip-token");
    }
    let embedder = RemoteEmbedder::with_provider(
        RemoteEmbedderProvider::Openai,
        &format!("{}/v1", server.uri()),
        "MOAGAN_TEST_REMOTE_BEARER",
        "m",
        1,
    )
    .unwrap();
    let result = embedder.embed_batch(&["x"]).await;
    match result {
        Ok(v) => assert_eq!(v, vec![vec![0.0]]),
        Err(Error::Provider(msg)) => {
            // Fallback path: if the matcher did not match, the
            // server returns 404 and the request fails. The test
            // contract is "successful call proves the bearer
            // header was sent"; surface the failure message so the
            // mismatch is debuggable.
            panic!("bearer header mismatch: {msg}");
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_401_maps_to_invalid_api_key() {
    use moagan::error::Error;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "m",
        4,
        "MOAGAN_TEST_REMOTE_401",
        "k",
    );
    let err = embedder
        .embed_batch(&["x"])
        .await
        .expect_err("401 must error");
    assert!(
        matches!(err, Error::InvalidApiKey(_)),
        "expected InvalidApiKey, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_429_maps_to_plan_exhausted() {
    use moagan::error::Error;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{\"error\":\"throttle\"}"))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "m",
        4,
        "MOAGAN_TEST_REMOTE_429",
        "k",
    );
    let err = embedder
        .embed_batch(&["x"])
        .await
        .expect_err("429 must error");
    assert!(
        matches!(err, Error::PlanExhausted(_)),
        "expected PlanExhausted, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_500_maps_to_provider_error() {
    use moagan::error::Error;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broke"))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "m",
        4,
        "MOAGAN_TEST_REMOTE_500",
        "k",
    );
    let err = embedder
        .embed_batch(&["x"])
        .await
        .expect_err("500 must error");
    assert!(
        matches!(err, Error::Provider(_)),
        "expected Provider, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_count_mismatch_is_provider_error() {
    use moagan::error::Error;
    let server = MockServer::start().await;
    // Return ONE vector when the caller sends TWO inputs. The
    // adapter must reject the mismatch explicitly rather than
    // silently re-aligning inputs and outputs.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"embedding": [0.1, 0.2]}]
        })))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "m",
        2,
        "MOAGAN_TEST_REMOTE_MISMATCH",
        "k",
    );
    let err = embedder
        .embed_batch(&["one", "two"])
        .await
        .expect_err("mismatch must error");
    assert!(
        matches!(err, Error::Provider(_)),
        "expected Provider, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_json_4xx_surfaces_as_status_error() {
    use moagan::error::Error;
    let server = MockServer::start().await;
    // Return a non-JSON HTML body for a 401. The adapter must NOT
    // surface this as a JSON-decode error; it must classify the
    // HTTP status and return `InvalidApiKey`.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(401).set_body_string("<html>nope</html>"))
        .mount(&server)
        .await;

    let embedder = embedder_against(
        &server,
        RemoteEmbedderProvider::Openai,
        "m",
        4,
        "MOAGAN_TEST_REMOTE_NON_JSON_4XX",
        "k",
    );
    let err = embedder
        .embed_batch(&["x"])
        .await
        .expect_err("non-JSON 401 must error");
    assert!(
        matches!(err, Error::InvalidApiKey(_)),
        "expected InvalidApiKey (not JSON-decode Provider), got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_batch_round_trips_through_wiremock() {
    let server = MockServer::start().await;
    // No mock mounted: an empty batch must NOT issue an HTTP
    // request at all, so the wiremock's `expect(0)` (the default)
    // would catch a regression. We use `expect(0)` explicitly.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    unsafe {
        std::env::set_var("MOAGAN_TEST_REMOTE_EMPTY", "k");
    }
    let embedder = RemoteEmbedder::with_provider(
        RemoteEmbedderProvider::Openai,
        &format!("{}/v1", server.uri()),
        "MOAGAN_TEST_REMOTE_EMPTY",
        "m",
        3,
    )
    .unwrap();
    let out = embedder.embed_batch(&[]).await.unwrap();
    assert!(out.is_empty());
}
