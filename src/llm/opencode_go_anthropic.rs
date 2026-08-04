//! `opencode_go_anthropic` provider — Anthropic-compatible wire format
//! served by OpenCode Go at `https://opencode.ai/zen/go/v1/messages`.
//!
//! Models served at this endpoint (per the operator's 2026-08-04 model
//! roster) are:
//!
//! - `minimax-m3`, `minimax-m2.7`, `minimax-m2.5` (Anthropic SDK)
//! - `qwen3.8-max`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus` (Anthropic SDK)
//!
//! The wire format is identical to the `minimax` provider so the
//! request body (MessagesRequestBody) and response decoder
//! (MessagesResponseBody) are shared via `super::http`. The differences
//! are limited to the `name` field, the BLOCKED_MODELS gate, and the
//! API key env var (`OPENCODE_GO_API_KEY`).
//!
//! Per-model temperature overrides (Fix #5, B + A) live in
//! `super::opencode_go::MODEL_TEMPERATURE_OVERRIDES`. Unknown models
//! fall back to the per-role temperature; if the upstream rejects
//! with a 400, the retry path in `phase.rs::call_with_retry_parse`
//! surfaces the error so the operator can extend the map.

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

use super::http::{body_from_request, build_client, build_headers, classify_status, retry_after};
use super::provider::Provider;
use super::wire::{Request, Response};

/// OpenCode Go provider routed through the Anthropic-compatible
/// `/v1/messages` endpoint. Distinct from the `minimax` provider so
/// that future behavior changes (e.g. response_format, custom
/// headers) don't leak across backends.
#[derive(Debug, Clone)]
pub struct OpenCodeGoAnthropicProvider {
    name: String,
    model: String,
    endpoint: String,
    api_key: SecretString,
    client: reqwest::Client,
    max_retries: u32,
}

impl OpenCodeGoAnthropicProvider {
    /// Build from a provider config and a resolved API key. The
    /// `spec.kind` must be `"opencode_go"` and the endpoint must end
    /// in `/v1/messages` (or `/v1` so we can append).
    pub fn new(spec: &ProviderConfig, api_key: SecretString) -> Result<Self> {
        if spec.kind != "opencode_go" {
            return Err(Error::InvalidArgs(format!(
                "opencode_go_anthropic provider got kind '{}'",
                spec.kind
            )));
        }
        let client = build_client()?;
        Ok(Self {
            name: "opencode_go".to_owned(),
            model: spec.model.clone(),
            endpoint: spec.endpoint.clone(),
            api_key,
            client,
            max_retries: 3,
        })
    }

    /// Build from config using `OPENCODE_GO_API_KEY`.
    pub fn from_config(spec: &ProviderConfig) -> Result<Self> {
        let key = std::env::var("OPENCODE_GO_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::InvalidApiKey(
                    "OPENCODE_GO_API_KEY not set; provide via env, --api-key, or config file"
                        .into(),
                )
            })?;
        Self::new(spec, SecretString::new(key))
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

    async fn sleep_with_jitter(attempt: u32, suggested: Option<std::time::Duration>) {
        let base = suggested.unwrap_or(std::time::Duration::from_millis(500));
        let jitter = (fastrand::u64(..) % 250) + 1;
        let total = base + std::time::Duration::from_millis(jitter);
        let half = total / 2;
        let low = total.saturating_sub(half);
        let high = total + half;
        let span = high.as_millis().saturating_sub(low.as_millis()) as u64;
        let chosen = if span == 0 {
            low
        } else {
            low + std::time::Duration::from_millis(fastrand::u64(..) % span)
        };
        tokio::time::sleep(chosen).await;
        let _ = attempt;
    }
}

#[async_trait]
impl Provider for OpenCodeGoAnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        let url = self.messages_url();
        let body = body_from_request(req);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let headers = build_headers(self.api_key.expose(), &[])?;
            let request_started = std::time::Instant::now();
            tracing::debug!(
                provider = self.name,
                attempt,
                stage = "http.request.started",
                "Provider HTTP stage"
            );
            let result = self.client.post(&url).headers(headers).json(&body).send().await;
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
                        let decode_started = std::time::Instant::now();
                        let parsed: super::http::MessagesResponseBody = resp
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_url_handles_known_suffixes() {
        let p = OpenCodeGoAnthropicProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "qwen3.7-max".into(),
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
            "https://opencode.ai/zen/go/v1/messages"
        );
    }

    #[test]
    fn messages_url_handles_messages_suffix() {
        let p = OpenCodeGoAnthropicProvider::new(
            &ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1/messages".into(),
                model: "minimax-m3".into(),
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
            "https://opencode.ai/zen/go/v1/messages"
        );
    }

    #[test]
    fn from_config_errors_when_kind_mismatch() {
        let result = OpenCodeGoAnthropicProvider::new(
            &ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "x".into(),
                max_tokens: None,
                temperature: None,
                top_p: None,
                hard_incompatibilities: vec![],
            },
            SecretString::new("dummy".into()),
        );
        assert!(matches!(result, Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn from_config_errors_when_key_missing() {
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
        }
        let result = OpenCodeGoAnthropicProvider::from_config(&ProviderConfig {
            kind: "opencode_go".into(),
            endpoint: "https://opencode.ai/zen/go/v1".into(),
            model: "x".into(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            hard_incompatibilities: vec![],
        });
        assert!(matches!(result, Err(Error::InvalidApiKey(_))));
    }
}
