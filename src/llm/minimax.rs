//! `minimax` provider — Anthropic-compatible endpoint at
//! `https://api.minimax.io/anthropic/v1/messages`.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::config::ProviderConfig;
use crate::error::{Error, Result};
use crate::secret::SecretString;

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
        // ±50% jitter as per 10-integrada-v0 §D.4.7.
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

    async fn send(&self, req: &Request) -> Result<Response> {
        let url = self.messages_url();
        let body = body_from_request(req);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let headers = build_headers(self.api_key.expose(), &[])?;
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
                    let retry_after = retry_after(&resp);
                    if status.is_success() {
                        let parsed: MessagesResponseBody = resp
                            .json()
                            .await
                            .map_err(|e| Error::Provider(format!("decode response: {e}")))?;
                        let stop = parsed.stop_reason().map(str::to_owned);
                        let resp = parsed
                            .into_response()
                            .map_err(|e| Error::Provider(e.to_string()))?;
                        if stop.as_deref() == Some("max_tokens") {
                            eprintln!(
                                "[minimax] WARNING: response truncated at max_tokens (text_len={})",
                                resp.text.len()
                            );
                        }
                        return Ok(resp);
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
}
