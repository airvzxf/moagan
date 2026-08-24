//! `minimax` provider — Anthropic-compatible endpoint at
//! `https://api.minimax.io/anthropic/v1/messages`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::capabilities::{MINIMAX_MAX_TOKENS_CAP, ProviderCapabilities};
use super::circuit_breaker::CircuitBreaker;
use super::http::{
    MessagesResponseBody, build_client, build_headers, classify_status, retry_after,
};
use super::probe_table::MaxTokensTable;
use super::provider::Provider;
use super::wire::{Request, Response};
use super::wire_format::{AnthropicWire, WireFormat};

/// `minimax` provider. Talks to the Anthropic-compatible
/// `/v1/messages` endpoint.
#[derive(Clone)]
pub struct MinimaxProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: Client,
    max_retries: u32,
    breaker: CircuitBreaker,
    /// Per-provider hard cap on `max_tokens` (set from
    /// `ProviderConfig::max_tokens`). The default is
    /// `DEFAULT_MAX_TOKENS` (1,000,000), so the per-role runtime
    /// value normally fits under the cap. The clamp below exists
    /// for the rare cases where a TOML override sets a smaller
    /// provider-specific limit, so the upstream never rejects the
    /// request.
    provider_max_tokens: Option<u32>,
    /// Auto-probed `max_tokens` table. When `Some` the
    /// `resolve_cached(self.name(), self.model())` value joins the
    /// clamp chain as the second-highest layer (operator override
    /// wins, discovered ceiling is the floor). `None` when the
    /// provider was built without going through `registry_from_config`
    /// — unit tests and legacy call paths.
    max_tokens_table: Option<Arc<MaxTokensTable>>,
}

impl MinimaxProvider {
    /// Build a provider from a config and a resolved API key.
    /// Kept for backwards compatibility with hand-rolled callers
    /// (legacy test fixtures); new dispatcher code goes through
    /// [`Self::from_resolved`].
    ///
    /// When `spec.models` is empty (the v0.9 fixture shape) the
    /// section-level `endpoint` is reused for a synthetic
    /// `ModelConfig` so the rest of the constructor (URL builder,
    /// clamp chain, `max_tokens_table` lookup by `(name, model)`)
    /// sees the same shape the v0.10 dispatcher passes in.
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        let client = build_client()?;
        let first = spec
            .models
            .first()
            .cloned()
            .unwrap_or_else(|| crate::config::ModelConfig {
                id: "MiniMax-M3".to_owned(),
                endpoint: spec.endpoint.clone(),
                max_tokens: None,
            });
        Ok(Self {
            name: "minimax".to_owned(),
            model: first.id.clone(),
            endpoint: first
                .endpoint
                .clone()
                .unwrap_or_else(|| "https://api.minimax.io/anthropic/v1/messages".to_owned()),
            api_key,
            client,
            max_retries: 3,
            breaker: CircuitBreaker::default(),
            provider_max_tokens: first.max_tokens,
            max_tokens_table: None,
        })
    }

    /// Build from config, resolving the API key via the unified
    /// helper. Kept for backwards compatibility; new dispatcher
    /// code goes through [`Self::from_resolved`].
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = super::api_keys::lookup_key("minimax", None)
            .ok_or_else(|| Error::InvalidApiKey {
                message: "MINIMAX_API_KEY not set; provide via env, --api-key, or api_keys.toml"
                    .into(),
                http_status: None,
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey { message, .. } => Error::InvalidApiKey {
                    message: format!(
                        "minimax: {message}; check api_keys.toml and the env var fallback"
                    ),
                    http_status: None,
                },
                other => other,
            })?;
        Self::new(spec, SecretString::new(key))
    }

    /// v0.10 dispatcher entry point. Builds a `MinimaxProvider`
    /// from a `ResolvedModelConfig` and resolves the API key via
    /// the unified helper. The key lookup falls back from the
    /// section name to the canonical `kind` so a per-model alias
    /// like `minimax-m2.7-highspeed` (kind=`"minimax"`) resolves
    /// against `MINIMAX_API_KEY` rather than the non-existent
    /// `MINIMAX-M2.7-HIGHSPEED_API_KEY`. The dispatcher routes
    /// the canonical MiniMax section to this constructor; the
    /// per-model URL is the same for every MiniMax model so the
    /// dispatcher does not have to inspect the URL.
    pub fn from_resolved(resolved: &crate::config::ResolvedModelConfig) -> Result<Self> {
        let kind = super::api_keys::lookup_kind_for_resolved(resolved);
        let key = super::api_keys::lookup_key(&kind, None)
            .ok_or_else(|| Error::InvalidApiKey {
                message: format!(
                    "{}_API_KEY not set; provide via env, --api-key, or api_keys.toml",
                    kind.to_ascii_uppercase()
                ),
                http_status: None,
            })?
            .map_err(|e| match e {
                Error::InvalidApiKey { message, .. } => Error::InvalidApiKey {
                    message: format!(
                        "{}: {message}; check api_keys.toml and the env var fallback",
                        kind
                    ),
                    http_status: None,
                },
                other => other,
            })?;
        let client = build_client()?;
        Ok(Self {
            name: resolved.section.clone(),
            model: resolved.id.clone(),
            endpoint: resolved.endpoint.clone(),
            api_key: SecretString::new(key),
            client,
            max_retries: 3,
            breaker: CircuitBreaker::default(),
            provider_max_tokens: resolved.max_tokens,
            max_tokens_table: None,
        })
    }

    /// Set the maximum number of retries (default 3).
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Attach the shared auto-probe `max_tokens` table so `send()`
    /// layers the discovered ceiling into the clamp chain. Wired by
    /// `registry_from_config` when the registry has a table. The
    /// builder takes `self` by value to stay fluent with the other
    /// `with_*` builders in this provider.
    pub fn with_max_tokens_table(mut self, table: Arc<MaxTokensTable>) -> Self {
        self.max_tokens_table = Some(table);
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

    /// Shared HTTP retry body for [`Provider::send`] and
    /// [`Provider::send_probe`]. The caller applies the per-call
    /// `max_tokens` clamp before invoking this so the wire body
    /// carries whatever value the caller approved. Inherent
    /// (non-trait) method so the HTTP loop lives in one place.
    async fn send_http(&self, req: Request) -> Result<(u16, Response)> {
        self.send_http_with_retries(req, self.max_retries).await
    }

    /// HTTP body used by both `send` (with `self.max_retries`) and
    /// `send_probe` (with `probe_max_retries: 0`). The probe path
    /// passes `0` because a 4xx IS the algorithm's signal — a
    /// "max tokens rejected" response must not be retried, or the
    /// 5 s probe timeout blows and the algorithm confuses
    /// `Indeterminate` with `Rejected`.
    async fn send_http_with_retries(
        &self,
        req: Request,
        probe_max_retries: u32,
    ) -> Result<(u16, Response)> {
        let result = async {
            let url = self.messages_url();
            let body = AnthropicWire.encode_body(&req)?;
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
                    .body(body.clone())
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
                            let parsed: MessagesResponseBody =
                                resp.json().await.map_err(|e| Error::Provider {
                                    message: format!("decode response: {e}"),
                                    http_status: None,
                                })?;
                            tracing::debug!(
                                provider = self.name,
                                attempt,
                                stage = "http.body.decoded",
                                status = status_code,
                                elapsed_ms = decode_started.elapsed().as_millis(),
                                "Provider HTTP stage"
                            );
                            let resp = parsed.into_response().map_err(|e| Error::Provider {
                                message: e.to_string(),
                                http_status: None,
                            })?;
                            return Ok((status_code, resp));
                        }
                        let body = resp.text().await.unwrap_or_default();
                        let err = classify_status(status, &body);
                        // Retryable classifications:
                        // - `Timeout` and `Provider`: transient network / upstream blip.
                        // - `PlanExhausted`: persistent quota — but the per-(provider,
                        //   role) breaker hasn't tripped yet, so a one-shot retry is
                        //   worthwhile when the downstream `phase::call_*` retry budget
                        //   allows it.
                        // - `Throttled`: transient rate-limit — retry after the throttle's
                        //   adaptive backoff so the recovery is bounded by `Retry-After`
                        //   when the upstream provided one. The throttle governor lives
                        //   upstream of this loop, so the `sleep_with_jitter(attempt,
                        //   retry_after)` is purely the per-attempt wait; the throttle
                        //   covers role-shaping.
                        let retryable = matches!(
                            err,
                            Error::Timeout { .. }
                                | Error::PlanExhausted { .. }
                                | Error::Throttled { .. }
                                | Error::Provider { .. }
                        );
                        if !retryable || attempt >= probe_max_retries {
                            return Err(err);
                        }
                        Self::sleep_with_jitter(attempt, retry_after).await;
                    }
                    Err(e) => {
                        if attempt >= probe_max_retries {
                            return Err(Error::Provider {
                                message: format!("network: {e}"),
                                http_status: None,
                            });
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

/// Custom Debug that masks `max_tokens_table` — `MaxTokensTable`
/// does not implement `Debug` (that lives in `probe_table.rs`,
/// outside this provider's owned files) and exposing the table
/// reference in a Debug dump adds noise without signal: the table
/// is the only `Arc` on the struct, so showing the rest is enough
/// to identify the instance.
impl std::fmt::Debug for MinimaxProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinimaxProvider")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("provider_max_tokens", &self.provider_max_tokens)
            .field("max_tokens_table", &"<shared>")
            .finish()
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
        let mut req = req.clone();
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        // Three-layer cap (highest priority first, smallest wins):
        //   1. MINIMAX_MAX_TOKENS_CAP — documented upstream ceiling;
        //      the upstream returns HTTP 400 above it.
        //   2. ProviderConfig::max_tokens — operator override.
        //   3. MaxTokensTable::resolve_cached — auto-probed
        //      per-(provider, model) value, primary source of truth.
        let cap = operator_cap.min(table_cap).min(MINIMAX_MAX_TOKENS_CAP);
        req.max_tokens = req.max_tokens.min(cap);
        self.send_http(req).await
    }

    fn effective_max_tokens(&self, req: &Request) -> u32 {
        // Mirror of the clamp chain in `send()` so the audit-log hash
        // is byte-for-byte identical to the wire body. Reordering or
        // dropping a layer here would let a request leave the
        // process with `max_tokens = 1_000_000` while the audit
        // records the sha256 of `max_tokens = 524_288`, and the
        // proxy verify step would flag every call as a body
        // mismatch.
        let operator_cap = self.provider_max_tokens.unwrap_or(u32::MAX);
        let table_cap = self
            .max_tokens_table
            .as_ref()
            .and_then(|t| t.resolve_cached(self.name(), self.model()))
            .unwrap_or(u32::MAX);
        req.max_tokens
            .min(operator_cap)
            .min(table_cap)
            .min(MINIMAX_MAX_TOKENS_CAP)
    }

    /// Bypass variant for the auto-probe. Skips every cap — operator
    /// override, cached table, and `MINIMAX_MAX_TOKENS_CAP` — so the
    /// algorithm sees the upstream's real boundary. The regular
    /// `send` keeps every cap so a stale or empty table cannot leak
    /// an unbounded value into the wire body.
    ///
    /// **Why skip ALL caps (not just the safety ceiling)**: if the
    /// operator's TOML pins `max_tokens = 524288` (the historical cap
    /// from PR #379), the operator_cap clamps the wire body to
    /// 524288 for every probe, so the algorithm only ever sees
    /// `max_tokens=524288` and concludes "accepts everything" — even
    /// though the upstream rejects `max_tokens=524289`. To discover
    /// the real boundary, the probe must send whatever value the
    /// algorithm chose, unmodified.
    async fn send_probe(&self, req: &Request) -> Result<(u16, Response)> {
        // M1: the floor on the returned value is applied by
        // `detect_max_tokens` itself; raising the requested value
        // here would mask boundaries below MIN_AUTOPROBE_FLOOR.
        self.send_http(req.clone()).await
    }

    /// Cap the exponential probe at `MINIMAX_MAX_TOKENS_CAP`. The
    /// upstream rejects `max_tokens > 524_288` with HTTP 400, so the
    /// exponential phase must stop at the first `2^k` past the cap
    /// (k=20 → 1_048_576) rather than waste a probe round-trip on a
    /// value the upstream will never accept. Mirrors the wiring on
    /// `OpenAiCompatProvider` (which uses `kind_hard_cap`); minimax
    /// has no `kind_hard_cap` and clamps inline instead, so the
    /// ceiling lives directly on this provider.
    fn max_tokens_probe_ceiling(&self) -> u32 {
        MINIMAX_MAX_TOKENS_CAP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_handles_known_suffixes() {
        let p = MinimaxProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
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
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
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
            endpoint: None,
            models: Vec::new(),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            plan: None,
        };
        let r = MinimaxProvider::from_config(&cfg);
        assert!(matches!(r, Err(Error::InvalidApiKey { .. })));
    }

    fn test_provider(endpoint: String) -> MinimaxProvider {
        MinimaxProvider::new(
            &ProviderConfig {
                endpoint: Some(endpoint),
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap()
        .with_max_retries(1)
    }

    fn test_request() -> Request {
        Request {
            model: "MiniMax-M3".into(),
            role: crate::llm::role::Role::Intake,
            system: String::new(),
            user: "test".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
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

        assert!(matches!(result, Err(Error::Provider { .. })));
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
                models: vec![crate::config::ModelConfig {
                    id: model.into(),
                    endpoint: None,
                    max_tokens: None,
                }],
                endpoint: None,
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            };
            let p = MinimaxProvider::new(&cfg, SecretString::new("dummy".into()))
                .expect("MinimaxProvider::new should accept every canonical model");
            assert_eq!(p.model(), model, "model name did not round-trip");
            assert_eq!(p.name(), "minimax");
            // v0.10: the canonical `endpoint` carries the full URL
            // (including the wire-format path); the constructor
            // passes it through unchanged.
            assert_eq!(p.endpoint(), "https://api.minimax.io/anthropic/v1/messages");
        }
    }

    /// v0.5 PR-12 parity test: the canonical `minimax` provider
    /// now serializes the request body via
    /// `AnthropicWire::encode_body` (instead of the legacy
    /// `body_from_request`). This test pins the contract: the JSON
    /// posted to the upstream `/v1/messages` endpoint must carry
    /// the Anthropic-compatible shape (`model`, `max_tokens`,
    /// `system`, `messages: [{role: "user", content: ...}]`) and
    /// must NOT regress to an empty body or a different schema.
    #[tokio::test]
    async fn minimax_send_posts_anthropic_compatible_body() {
        use std::sync::{Arc, Mutex};

        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let captured_for_responder = captured.clone();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "dummy"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(move |req: &wiremock::Request| {
                captured_for_responder
                    .lock()
                    .expect("capture lock")
                    .replace(req.body.clone());
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 11,
                        "output_tokens": 4,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    }
                }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let provider = MinimaxProvider::new(
            &ProviderConfig {
                models: Vec::new(),
                endpoint: Some(server.uri()),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .expect("MinimaxProvider::new should accept the mock endpoint");
        let req = Request {
            model: "MiniMax-M3".into(),
            role: crate::llm::role::Role::Intake,
            system: "you are minimax".into(),
            user: "hello upstream".into(),
            max_tokens: 32,
            temperature: Some(0.4),
            top_p: Some(0.9),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let (status, resp) = provider
            .send(&req)
            .await
            .expect("minimax send should succeed against the mock server");
        assert_eq!(status, 200);
        assert_eq!(resp.text, "ok");

        let bytes = captured
            .lock()
            .expect("capture lock")
            .clone()
            .expect("mock server captured the request body");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("captured body is valid JSON");
        // Anthropic-compatible shape contract:
        assert_eq!(json["model"], "MiniMax-M3");
        assert_eq!(json["max_tokens"], 32);
        assert_eq!(json["system"], "you are minimax");
        // PR-D2 follow-up: JSON-required roles (Intake is one)
        // get an assistant prefill of `{` to bias the model toward
        // producing valid JSON. The test pins the two-message
        // shape; the production code path also honours a
        // caller-supplied `extra_messages` array.
        assert_eq!(
            json["messages"],
            serde_json::json!([
                {"role": "user", "content": "hello upstream"},
                {"role": "assistant", "content": "{"},
            ]),
            "Intake must include the JSON prefill assistant message"
        );
        let temp = json["temperature"].as_f64().expect("temperature numeric");
        assert!((temp - 0.4).abs() < 1e-6, "temperature must round-trip");
        let top_p = json["top_p"].as_f64().expect("top_p numeric");
        assert!((top_p - 0.9).abs() < 1e-6, "top_p must round-trip");
        // `thinking` must remain absent (never set by M-series
        // request path — the reference sweep relies on thinking
        // staying ON implicitly).
        assert!(
            json.get("thinking").is_none(),
            "minimax must not emit thinking control, got: {json}"
        );
    }

    /// Per-provider `max_tokens` cap from `ProviderConfig::max_tokens`
    /// must clamp the value sent on the wire, mirroring the
    /// `OpenAiCompatProvider` behaviour. With `max_tokens: Some(8192)`
    /// and a `Request { max_tokens: 1_000_000, .. }`, the JSON body
    /// the mock server captures must carry `"max_tokens":8192`.
    #[tokio::test]
    async fn send_clamps_max_tokens_to_provider_cap() {
        use std::sync::{Arc, Mutex};

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let captured_for_responder = captured.clone();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &wiremock::Request| {
                captured_for_responder
                    .lock()
                    .expect("capture lock")
                    .replace(req.body.clone());
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    }
                }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let provider = MinimaxProvider::new(
            &ProviderConfig {
                models: vec![crate::config::ModelConfig {
                    id: "MiniMax-M3".into(),
                    // v0.10: the constructor reads `endpoint` from
                    // the per-model `ModelConfig`, not the section
                    // level. Point the per-model entry at the mock
                    // server so `messages_url()` appends `/v1/messages`
                    // to the mock base instead of the real MiniMax URL.
                    endpoint: Some(server.uri()),
                    max_tokens: Some(8192),
                }],
                endpoint: None,
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .expect("MinimaxProvider::new should accept the cap");

        let req = Request {
            model: "MiniMax-M3".into(),
            role: crate::llm::role::Role::Intake,
            system: String::new(),
            user: "hello".into(),
            max_tokens: 1_000_000,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let (status, _resp) = provider
            .send(&req)
            .await
            .expect("minimax send should succeed against the mock server");
        assert_eq!(status, 200);

        let bytes = captured
            .lock()
            .expect("capture lock")
            .clone()
            .expect("mock server captured the request body");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("captured body is valid JSON");
        assert_eq!(
            json["max_tokens"], 8192,
            "provider_max_tokens cap must clamp the wire value, got body: {json}"
        );
    }

    /// A `[providers.minimax]` override ABOVE the upstream limit must
    /// not leak into the request: `MINIMAX_MAX_TOKENS_CAP` (524_288)
    /// is the hard ceiling enforced at the wire body. With
    /// `max_tokens: Some(2_000_000)` and a `Request { max_tokens:
    /// 1_000_000, .. }`, the captured body must carry
    /// `"max_tokens":524288` — otherwise the upstream answers HTTP 400
    /// ("model[MiniMax-M3] does not support max tokens > 524288").
    #[tokio::test]
    async fn send_clamps_max_tokens_to_minimax_hard_cap() {
        use std::sync::{Arc, Mutex};

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let captured_for_responder = captured.clone();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(move |req: &wiremock::Request| {
                captured_for_responder
                    .lock()
                    .expect("capture lock")
                    .replace(req.body.clone());
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    }
                }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let provider = MinimaxProvider::new(
            &ProviderConfig {
                models: Vec::new(),
                endpoint: Some(server.uri()),
                // Deliberately above the upstream ceiling.
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .expect("MinimaxProvider::new should accept the oversized cap");

        let req = Request {
            model: "MiniMax-M3".into(),
            role: crate::llm::role::Role::Intake,
            system: String::new(),
            user: "hello".into(),
            max_tokens: 1_000_000,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let (status, _resp) = provider
            .send(&req)
            .await
            .expect("minimax send should succeed against the mock server");
        assert_eq!(status, 200);

        let bytes = captured
            .lock()
            .expect("capture lock")
            .clone()
            .expect("mock server captured the request body");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("captured body is valid JSON");
        assert_eq!(
            json["max_tokens"],
            serde_json::json!(MINIMAX_MAX_TOKENS_CAP),
            "MINIMAX_MAX_TOKENS_CAP must clamp an oversized per-provider \
             override at the wire, got body: {json}"
        );
    }

    /// Auto-probe table clamp contract: when `with_max_tokens_table`
    /// attaches a table carrying a discovered value smaller than the
    /// requested `max_tokens`, the wire body must carry the
    /// discovered value (not the per-role default of 1_000_000 and
    /// not the operator override when one is absent). Pins the v0.7
    /// precedence order: `MINIMAX_MAX_TOKENS_CAP` > operator override
    /// > table > requested.
    #[tokio::test]
    async fn send_clamps_max_tokens_to_table_value() {
        use std::sync::Arc;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::llm::probe::{MIN_AUTOPROBE_FLOOR, ProbeOutcome, ProbeTransport};

        #[derive(Clone)]
        struct CappedTransport {
            cap: u32,
        }

        #[async_trait::async_trait]
        impl ProbeTransport for CappedTransport {
            async fn probe_send(&self, n: u32) -> ProbeOutcome {
                if n <= self.cap {
                    ProbeOutcome::Accepted
                } else {
                    ProbeOutcome::Rejected
                }
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Probe and store so the entry comes from the public API
        // (the inner field is private to `probe_table`); a transport
        // that accepts everything ≤ 24_000 seeds the binary search.
        // The discovered value drives the wire-body assertion below
        // directly: this test pins the wiring contract (table value
        // honoured on the wire) without depending on the probe
        // algorithm's exact convergence — that algorithm has a
        // known ±N imprecision at non-trivial boundaries
        // (see pre-existing `probe::tests::detect_finds_cap_at_8k`).
        let transport: Arc<dyn ProbeTransport> = Arc::new(CappedTransport { cap: 24_000 });
        let table = Arc::new(MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR));
        let discovered = table
            .probe_and_store(
                "minimax",
                "MiniMax-M3",
                transport,
                crate::llm::probe::MAX_AUTOPROBE_CEILING,
            )
            .await
            .expect("probe_and_store");

        let provider = MinimaxProvider::new(
            &ProviderConfig {
                models: Vec::new(),
                endpoint: Some(server.uri()),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                plan: None,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
            },
            SecretString::new("dummy".into()),
        )
        .expect("MinimaxProvider::new should accept the mock endpoint")
        .with_max_tokens_table(table);

        let req = Request {
            model: "MiniMax-M3".into(),
            role: crate::llm::role::Role::Intake,
            system: String::new(),
            user: "hello".into(),
            max_tokens: 1_000_000,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let (status, _resp) = provider
            .send(&req)
            .await
            .expect("minimax send should succeed against the mock server");
        assert_eq!(status, 200);

        let received = server
            .received_requests()
            .await
            .expect("recording must be enabled by default");
        assert_eq!(received.len(), 1, "exactly one request must be sent");
        let json: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("mock server received a JSON body");
        assert_eq!(
            json["max_tokens"],
            serde_json::json!(discovered),
            "wire body must carry the table-resolved value ({discovered}), got body: {json}"
        );
    }

    /// `effective_max_tokens` must mirror the same 3-layer clamp chain
    /// `send()` applies so the audit-log hash matches the wire body
    /// byte-for-byte. This is the regression pin for the bug where
    /// `phases::phase::dispatch_to_provider` re-derived a 2-layer
    /// `operator_cap.min(table_cap)` chain that disagreed with
    /// `send()`'s 3-layer
    /// `operator_cap.min(table_cap).min(MINIMAX_MAX_TOKENS_CAP)` chain
    /// once PR #400 raised `DEFAULT_MAX_TOKENS` to 1M. Every layer
    /// below must produce the same value here that `send()` writes
    /// onto the wire.
    #[tokio::test]
    async fn effective_max_tokens_matches_send_clamp_chain() {
        use crate::llm::probe::{MIN_AUTOPROBE_FLOOR, ProbeOutcome, ProbeTransport};

        // Layer-by-layer equivalence: with no operator cap and no
        // table, `effective_max_tokens` clamps the requested value
        // down to `MINIMAX_MAX_TOKENS_CAP` (524_288). This is the
        // critical case for the audit hash — `DEFAULT_MAX_TOKENS` is
        // 1_000_000 since PR #400, so without this clamp every call
        // would record the sha256 of `max_tokens = 1_000_000` while
        // the proxy sees `max_tokens = 524_288`.
        let p = MinimaxProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p.effective_max_tokens(&Request {
                max_tokens: 1_000_000,
                ..test_request()
            }),
            MINIMAX_MAX_TOKENS_CAP,
            "with no caps the value must clamp to MINIMAX_MAX_TOKENS_CAP"
        );
        // Below the cap: pass-through (request under the ceiling
        // flows unchanged, no audit-hash mutation needed).
        assert_eq!(
            p.effective_max_tokens(&Request {
                max_tokens: 4096,
                ..test_request()
            }),
            4096,
            "requests below MINIMAX_MAX_TOKENS_CAP must pass through"
        );

        // Operator override wins over the requested value.
        let p_op = MinimaxProvider::new(
            &ProviderConfig {
                endpoint: None,
                // v0.10: the operator `max_tokens` cap lives on the
                // per-model `ModelConfig`, not as a section-level
                // field. Provide an explicit entry so the constructor
                // sees `provider_max_tokens = Some(8192)`.
                models: vec![crate::config::ModelConfig {
                    id: "MiniMax-M3".into(),
                    endpoint: None,
                    max_tokens: Some(8192),
                }],
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p_op.effective_max_tokens(&Request {
                max_tokens: 1_000_000,
                ..test_request()
            }),
            8_192,
            "operator override must clamp below the request value"
        );

        // Operator override above the upstream ceiling must still
        // cap at MINIMAX_MAX_TOKENS_CAP (the upstream rejects
        // `max_tokens > 524_288` with HTTP 400).
        let p_above = MinimaxProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap();
        assert_eq!(
            p_above.effective_max_tokens(&Request {
                max_tokens: 1_000_000,
                ..test_request()
            }),
            MINIMAX_MAX_TOKENS_CAP,
            "MINIMAX_MAX_TOKENS_CAP must still clamp an oversized operator override"
        );

        // Table value wins over the requested value but stays below
        // the operator cap and `MINIMAX_MAX_TOKENS_CAP`. Seed via the
        // public `probe_and_store` API so we exercise the same path
        // the runtime uses (the inner entry field is private).
        #[derive(Clone)]
        struct CappedTransport {
            cap: u32,
        }
        #[async_trait::async_trait]
        impl ProbeTransport for CappedTransport {
            async fn probe_send(&self, n: u32) -> ProbeOutcome {
                if n <= self.cap {
                    ProbeOutcome::Accepted
                } else {
                    ProbeOutcome::Rejected
                }
            }
        }
        let transport: Arc<dyn ProbeTransport> = Arc::new(CappedTransport { cap: 24_000 });
        let table = Arc::new(MaxTokensTable::empty(MIN_AUTOPROBE_FLOOR));
        let discovered = table
            .probe_and_store(
                "minimax",
                "MiniMax-M3",
                transport,
                crate::llm::probe::MAX_AUTOPROBE_CEILING,
            )
            .await
            .expect("probe_and_store");
        let p_table = MinimaxProvider::new(
            &ProviderConfig {
                endpoint: None,
                models: Vec::new(),
                temperature: None,
                top_p: None,
                omit_max_tokens: false,
                max_token_auto: None,
                max_token_auto_enabled: None,
                max_token_auto_save: true,
                plan: None,
            },
            SecretString::new("dummy".into()),
        )
        .unwrap()
        .with_max_tokens_table(table);
        assert_eq!(
            p_table.effective_max_tokens(&Request {
                max_tokens: 1_000_000,
                ..test_request()
            }),
            discovered,
            "table value ({discovered}) must win when below operator cap and MINIMAX_MAX_TOKENS_CAP"
        );
    }
}
