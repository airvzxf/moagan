//! `minimax` provider — Anthropic-compatible endpoint at
//! `https://api.minimax.io/anthropic/v1/messages`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::ProviderCapabilities;
use super::circuit_breaker::CircuitBreaker;
use super::http::{
    MessagesResponseBody, body_from_request, build_client, build_headers, classify_status,
    retry_after,
};
use super::provider::Provider;
use super::wire::{Request, Response};

/// `minimax` provider. Talks to the Anthropic-compatible
/// `/v1/messages` endpoint.
#[derive(Debug, Clone)]
pub struct MinimaxProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: Client,
    max_retries: u32,
    breaker: CircuitBreaker,
}

impl MinimaxProvider {
    /// Build a provider from a config and a resolved API key.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "minimax" {
            return Err(Error::InvalidArgs(format!(
                "minimax provider got kind '{}'",
                spec.kind
            )));
        }
        let client = build_client()?;
        Ok(Self {
            name: "minimax".to_owned(),
            model: spec.model.clone(),
            endpoint: spec.endpoint.clone(),
            api_key,
            client,
            max_retries: 3,
            breaker: CircuitBreaker::default(),
        })
    }

    /// Build from config using the `MOAGAN_MINIMAX_API_KEY` env when
    /// no key is supplied in spec.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = std::env::var("MINIMAX_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "MINIMAX_API_KEY not set; provide via env, --api-key, or config file".into(),
                )
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// Set the maximum number of retries (default 3).
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Compute the URL for the messages endpoint.
    fn messages_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/v1/messages") {
            base.to_owned()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    /// Sleep helper that honours `Retry-After` plus jitter.
    async fn sleep_with_jitter(attempt: u32, suggested: Option<Duration>) {
        let base = suggested.unwrap_or(Duration::from_millis(500));
        let jitter = (fastrand::u64(..) % 250) + 1;
        let total = base + Duration::from_millis(jitter);
        // ±50% jitter as per catalog 10-integrada-v0 (decision row 34, §4.7 retries).
        let half = total / 2;
        let low = total.saturating_sub(half);
        let high = total + half;
        let span = high.as_millis().saturating_sub(low.as_millis()) as u64;
        let chosen = if span == 0 {
            low
        } else {
            low + Duration::from_millis(fastrand::u64(..) % span)
        };
        tokio::time::sleep(chosen).await;
        let _ = attempt;
    }
}

#[async_trait]
impl Provider for MinimaxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::for_minimax()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let result = async {
            let url = self.messages_url();
            let body = body_from_request(req);
            let mut attempt: u32 = 0;
            loop {
                attempt += 1;
                let headers = build_headers(self.api_key.expose(), &[])?;
                let request_started = Instant::now();
                tracing::debug!(
                    provider = self.name,
                    attempt,
                    stage = "http.request.started",
                    "Provider HTTP stage"
                );
                let result = self
                    .client
                    .post(&url)
                    .headers(headers)
                    .json(&body)
                    .send()
                    .await;
                match result {
                    Ok(resp) => {
                        let status = resp.status();
                        let status_code = status.as_u16();
                        tracing::debug!(
                            provider = self.name,
                            attempt,
                            stage = "http.headers.received",
                            status = status_code,
                            elapsed_ms = request_started.elapsed().as_millis(),
                            "Provider HTTP stage"
                        );
                        let retry_after = retry_after(&resp);
                        if status.is_success() {
                            let decode_started = Instant::now();
                            let parsed: MessagesResponseBody = resp
                                .json()
                                .await
                                .map_err(|e| Error::Provider(format!("decode response: {e}")))?;
                            tracing::debug!(
                                provider = self.name,
                                attempt,
                                stage = "http.body.decoded",
                                status = status_code,
                                elapsed_ms = decode_started.elapsed().as_millis(),
                                "Provider HTTP stage"
                            );
                            let resp = parsed
                                .into_response()
                                .map_err(|e| Error::Provider(e.to_string()))?;
                            return Ok((status_code, resp));
                        }
                        let body = resp.text().await.unwrap_or_default();
                        let err = classify_status(status, &body);
                        let retryable = matches!(
                            err,
                            Error::Timeout(_) | Error::PlanExhausted(_) | Error::Provider(_)
                        );
                        if !retryable || attempt >= self.max_retries {
                            return Err(err);
                        }
                        Self::sleep_with_jitter(attempt, retry_after).await;
                    }
                    Err(e) => {
                        if attempt >= self.max_retries {
                            return Err(Error::Provider(format!("network: {e}")));
                        }
                        Self::sleep_with_jitter(attempt, None).await;
                    }
                }
            }
        }
        .await;
        match &result {
            Ok(_) => self.breaker.record_success(),
            Err(err) => self.breaker.record_failure_if_circuit_opening(err),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_handles_known_suffixes() {
        let p = MinimaxProvider::new(
            &ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic/v1".into(),
                model: "MiniMax-M3".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p.messages_url(),
            "https://api.minimax.io/anthropic/v1/messages"
        );
    }

    #[test]
    fn messages_url_handles_anthropic_suffix() {
        let p = MinimaxProvider::new(
            &ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic".into(),
                model: "MiniMax-M3".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p.messages_url(),
            "https://api.minimax.io/anthropic/v1/messages"
        );
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
        }
        let cfg = ProviderConfig {
            kind: "minimax".into(),
            endpoint: "https://api.minimax.io/anthropic/v1".into(),
            model: "MiniMax-M3".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        };
        let r = MinimaxProvider::from_config(&cfg);
        assert!(matches!(r, Err(Error::InvalidApiKey(_))));
    }

    fn test_provider(endpoint: String) -> MinimaxProvider {
        MinimaxProvider::new(
            &ProviderConfig {
                kind: "minimax".into(),
                endpoint,
                model: "MiniMax-M3".into(),
                max_tokens: Some(16),
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            },
            SecretString::new("dummy".into()),
        )
        .unwrap()
        .with_max_retries(1)
    }

    fn test_request() -> Request {
        Request {
            role: crate::llm::role::Role::Intake,
            model: "MiniMax-M3".into(),
            system: String::new(),
            user: "test".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
        }
    }

    #[tokio::test]
    async fn minimax_records_circuit_opening_on_provider_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream failed"))
            .mount(&server)
            .await;
        let provider = test_provider(server.uri());
        let breaker = provider.breaker.clone();

        let result = provider.send(&test_request()).await;

        assert!(matches!(result, Err(Error::Provider(_))));
        assert_eq!(breaker.failure_count(), 1);
    }

    #[tokio::test]
    async fn minimax_skips_circuit_recording_on_429() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let responder_calls = calls.clone();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |_: &wiremock::Request| {
                if responder_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(429).insert_header("retry-after", "0")
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    }))
                }
            })
            .mount(&server)
            .await;
        let mut provider = test_provider(server.uri());
        provider.max_retries = 2;
        let breaker = provider.breaker.clone();

        let result = provider.send(&test_request()).await;

        assert!(matches!(result, Ok((200, _))));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(breaker.failure_count(), 0);
    }

    #[tokio::test]
    async fn minimax_provider_breaker_opens_after_threshold() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .mount(&server)
            .await;
        let mut provider = test_provider(server.uri());
        provider.breaker = CircuitBreaker::new(2, Duration::from_secs(60), Duration::from_secs(60));
        let breaker = provider.breaker.clone();

        for _ in 0..2 {
            assert!(provider.send(&test_request()).await.is_err());
        }

        assert_eq!(breaker.failure_count(), 2);
        assert!(breaker.is_open());
    }

    /// Q5 pin: every canonical MiniMax model name must round-trip
    /// through `MinimaxProvider::new` and the resulting provider's
    /// `model()` accessor. This is the contract the smoke script
    /// depends on (`--provider minimax-m2.5` and `--model
    /// MiniMax-M2.7` both reach the wire carrying the right model
    /// identifier).
    #[test]
    fn new_round_trips_each_canonical_model() {
        let canonical = [
            "MiniMax-M3",
            "MiniMax-M2.7",
            "MiniMax-M2.7-highspeed",
            "MiniMax-M2.5",
        ];
        for model in canonical {
            let cfg = ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic/v1".into(),
                model: model.into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            };
            let p = MinimaxProvider::new(&cfg, SecretString::new("dummy".into()))
                .expect("MinimaxProvider::new should accept every canonical model");
            assert_eq!(p.model(), model, "model name did not round-trip");
            assert_eq!(p.name(), "minimax");
            assert_eq!(p.endpoint(), "https://api.minimax.io/anthropic/v1");
        }
    }
}
