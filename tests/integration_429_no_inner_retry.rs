//! A 429 must produce exactly ONE HTTP request from the SDK retry
//! loop.
//!
//! `AnthropicClient::send` used to list `Error::Throttled` in its
//! internal `retryable` set, so a single throttled call fired
//! `max_retries + 1 == 4` requests before the error escaped to
//! `dispatch_with_governors`, which is the only place
//! `ProviderGate` lives. The gate was therefore informed *after*
//! the damage: a burst of N rejected calls became ~3N requests
//! aimed at an already-saturated provider. Across measured
//! `discover` runs roughly two of every three observed HTTP 429s
//! were self-inflicted by that amplification.
//!
//! The retry is not removed, only relocated. `ErrorCode::Http429`
//! is `is_retriable()`, so the outer `call_with_retry_parse` loop
//! still schedules the next attempt — and because that retry
//! re-enters through `dispatch_with_governors`, it now passes
//! through `ProviderGate::pre_call` and is actually gated.
//!
//! The third test is the guard against over-correction: 5xx and
//! timeout retries must survive, since those have no shared
//! provider-level cooldown to hand the decision to.

use std::sync::Arc;

use moagan::error::Error;
use moagan::llm::Role;
use moagan::llm::client::{AnthropicClient, LlmClient, LlmRequest};
use moagan::secret::SecretString;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// MiniMax's verbatim transient-saturation body. It contains both
/// "Token Plan" and "Upgrade", which is exactly why the retired
/// keyword scan mislabelled it as a dead quota.
const MINIMAX_429_BODY: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Token Plan rate limit reached: Upgrade your Token Plan or switch to pay-as-you-go API usage. (2062)"},"request_id":"070a63e52d2d3e884629319bbd84cf50"}"#;

fn build_client(server_uri: String) -> Arc<AnthropicClient> {
    let cfg = moagan::config::ProviderConfig {
        models: Vec::new(),
        endpoint: Some(server_uri),
        temperature: None,
        top_p: None,
        omit_max_tokens: false,
        plan: None,
        max_token_auto: None,
        max_token_auto_enabled: None,
        max_token_auto_save: true,
        temperature_auto_enabled: None,
        top_p_auto_enabled: None,
        top_k_auto_enabled: None,
    };
    Arc::new(
        AnthropicClient::new(&cfg, SecretString::new("sk-test".to_owned()))
            .expect("AnthropicClient::new should accept the test config"),
    )
}

fn request() -> LlmRequest {
    LlmRequest {
        role: Role::Sketch,
        model: "MiniMax-M3".into(),
        system: String::new(),
        user: "u".into(),
        max_tokens: Some(16),
        temperature: None,
        top_p: None,
        top_k: None,
        response_schema: None,
        stream: false,
        extra_messages: vec![],
        attachments: vec![],
        tool_choice: None,
    }
}

#[tokio::test]
async fn throttled_429_is_not_retried_inside_the_sdk_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(MINIMAX_429_BODY)
                .insert_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;

    let client = build_client(server.uri());
    let result = client.send(&request()).await;

    // The error still surfaces as `Throttled` — the classification
    // from #977 is untouched. Only the *number of attempts* changed.
    match result {
        Err(Error::Throttled { http_status, .. }) => {
            assert_eq!(http_status, Some(429), "429 must keep its status code");
        }
        other => panic!("expected Error::Throttled, got {other:?}"),
    }

    let received = server
        .received_requests()
        .await
        .expect("wiremock request log should be readable");
    assert_eq!(
        received.len(),
        1,
        "a 429 must cost exactly one HTTP request; got {} \
         (the SDK retry loop is amplifying again)",
        received.len()
    );
}

#[tokio::test]
async fn throttled_429_carries_the_upstream_retry_after_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(MINIMAX_429_BODY)
                .insert_header("retry-after", "3")
                .insert_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;

    let client = build_client(server.uri());
    let result = client.send(&request()).await;

    // `classify_status` hard-codes `retry_after_ms: None`, so the
    // SDK has to attach the parsed header itself. Without this the
    // throttle governor — which now owns the backoff decision —
    // would only ever see a blind exponential curve.
    match result {
        Err(Error::Throttled { retry_after_ms, .. }) => {
            assert_eq!(
                retry_after_ms,
                Some(3_000),
                "the upstream Retry-After must reach the governor"
            );
        }
        other => panic!("expected Error::Throttled, got {other:?}"),
    }
}

#[tokio::test]
async fn upstream_5xx_is_still_retried_inside_the_sdk_loop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_string(r#"{"error":"unavailable"}"#)
                .insert_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;

    let client = build_client(server.uri());
    let result = client.send(&request()).await;

    assert!(result.is_err(), "a 503 must not be reported as success");
    match result {
        Err(Error::Provider { http_status, .. }) => {
            assert_eq!(http_status, Some(503));
        }
        other => panic!("expected Error::Provider, got {other:?}"),
    }

    let received = server
        .received_requests()
        .await
        .expect("wiremock request log should be readable");
    // The loop is 1-indexed (`attempt` starts at 1), so with
    // `max_retries = 3` attempts 1 and 2 sleep and retry while
    // attempt 3 hits `attempt >= max_retries` and gives up: 3
    // requests total. This pins that the 429 change did not quietly
    // disable transient-error retries, which have no shared provider
    // cooldown to defer to.
    assert_eq!(
        received.len(),
        3,
        "5xx must keep its in-loop retry budget; got {}",
        received.len()
    );
}
