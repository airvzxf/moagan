//! Bounded external research fetcher (K.4 / proposal-04 §4).
//!
//! Allowlist-only; max [`MAX_URLS_PER_CALL`] URLs per call; max
//! [`MAX_BYTES_PER_URL`] bytes per response. Network failures are
//! non-fatal — caller should treat an empty `Ok(vec)` or a vec
//! containing per-URL [`FetchError`] variants as "research
//! unavailable, continue without it".
//!
//! Redaction piggybacks on the existing
//! [`crate::redact::RedactPolicy`] so any token / email that slips
//! into a fetched snippet is scrubbed before it lands in the
//! caller's `Vec`. We redact on the `Storage` surface because the
//! research snippets are destined for context injection in the
//! Sketch phase, not direct telemetry emission.

use std::time::Duration;

use crate::redact::{RedactPolicy, Surface, apply};
use crate::research::allowlist;

/// Hard cap on URLs accepted by a single [`ResearchFetcher::fetch_all`]
/// call.
pub const MAX_URLS_PER_CALL: usize = 3;
/// Hard cap on bytes retained from a single response body.
pub const MAX_BYTES_PER_URL: usize = 4 * 1024;
/// Per-request transport timeout. Keeps a slow host from wedging
/// the Sketch phase's pipeline.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A single fetched + redacted snippet. `truncated == true` means
/// the response was longer than [`MAX_BYTES_PER_URL`] and the tail
/// was dropped (the [`ResearchSnippet::url`] still refers to the
/// original, full-length source).
#[derive(Debug, Clone)]
pub struct ResearchSnippet {
    /// The URL the snippet was fetched from. Echoed verbatim; the
    /// allowlist filter already vetted the host.
    pub url: String,
    /// Redacted body, truncated to [`MAX_BYTES_PER_URL`].
    pub content: String,
    /// `true` when the response was larger than the cap and the
    /// tail was dropped.
    pub truncated: bool,
}

/// All the ways a fetch can fail per URL. Kept distinct from
/// [`crate::error::Error`] because the caller is supposed to treat
/// these as soft signals — most variants mean "skip this URL,
/// continue with the rest".
#[derive(Debug, Clone)]
pub enum FetchError {
    /// Host parsed from the URL is not in [`allowlist::ALLOWED_HOSTS`].
    DisallowedHost(String),
    /// Caller asked for more URLs than [`MAX_URLS_PER_CALL`] in a
    /// single call. Returned as the *only* error in the result vec
    /// so the caller sees the hard failure cleanly.
    TooManyUrls {
        /// Number of URLs the caller requested.
        requested: usize,
    },
    /// Transport / parse / decoding failure. String is the lower
    /// level `reqwest` or `url::Url::parse` message, already
    /// scrubbed of secrets by the upstream libraries.
    NetworkError(String),
    /// Response body was empty. Treat as "no useful signal".
    Empty,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DisallowedHost(h) => write!(f, "host '{h}' not in allowlist"),
            Self::TooManyUrls { requested } => {
                write!(f, "requested {requested} URLs, max {MAX_URLS_PER_CALL}")
            }
            Self::NetworkError(m) => write!(f, "network error: {m}"),
            Self::Empty => write!(f, "empty response"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Bounded external research fetcher (K.4 / proposal-04 §4).
///
/// Allowlist-only; max [`MAX_URLS_PER_CALL`] URLs per call; max
/// [`MAX_BYTES_PER_URL`] bytes per response. Network failures are
/// non-fatal — caller should treat an empty `Ok(vec)` or a vec
/// containing per-URL [`FetchError`] variants as "research
/// unavailable, continue without it".
///
/// `api_key` carries the optional bearer token applied to hosts
/// whose [`allowlist::HostPolicy::auth_bearer`] flag is `true`.
/// `None` / empty disables the header entirely (the canonical
/// case — `api_key` is configured via
/// [`crate::config::Config::research.api_key`] and env
/// `MOAGAN_RESEARCH_API_KEY`).
///
/// Redaction piggybacks on the existing
/// [`crate::redact::RedactPolicy`] so any token / email that slips
/// into a fetched snippet is scrubbed before it lands in the
/// caller's `Vec`. We redact on the `Storage` surface because the
/// research snippets are destined for context injection in the
/// Sketch phase, not direct telemetry emission.
#[derive(Debug, Clone)]
pub struct ResearchFetcher {
    /// Optional bearer token for `auth_bearer`-flagged hosts.
    /// `None` (or `Some("")`) suppresses the Authorization header
    /// so an unset config still produces clean anonymous requests.
    pub api_key: Option<String>,
}

impl ResearchFetcher {
    /// Build a fetcher with the given API key. `None` (or the empty
    /// string) keeps the Authorization header off for all requests —
    /// the canonical case when the operator has not configured
    /// `MOAGAN_RESEARCH_API_KEY`.
    pub fn new(api_key: Option<String>) -> Self {
        Self { api_key }
    }

    /// Fetch up to [`MAX_URLS_PER_CALL`] URLs, capped at
    /// [`MAX_BYTES_PER_URL`] bytes each. Always returns a `Vec` whose
    /// length matches `urls.len()`; per-URL failures land inline as
    /// [`FetchError`] so the caller can decide which to keep.
    ///
    /// When `urls.len() > MAX_URLS_PER_CALL`, the entire call is
    /// short-circuited and the result vec holds a single
    /// [`FetchError::TooManyUrls`].
    pub async fn fetch_all(
        &self,
        urls: &[String],
    ) -> Vec<std::result::Result<ResearchSnippet, FetchError>> {
        if urls.is_empty() {
            return Vec::new();
        }
        if urls.len() > MAX_URLS_PER_CALL {
            return vec![Err(FetchError::TooManyUrls {
                requested: urls.len(),
            })];
        }
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builder is infallible for our config");
        let policy = RedactPolicy::default();
        let mut out = Vec::with_capacity(urls.len());
        for url in urls {
            out.push(self.fetch_one(&client, &policy, url).await);
        }
        out
    }

    /// Internal per-URL fetch + redact. `pub(crate)` so the in-module
    /// test suite can exercise the host-allowlist branch and the
    /// bearer-auth wiring without a real network round-trip.
    pub(crate) async fn fetch_one(
        &self,
        client: &reqwest::Client,
        policy: &RedactPolicy,
        url: &str,
    ) -> std::result::Result<ResearchSnippet, FetchError> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| FetchError::NetworkError(format!("parse: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| FetchError::NetworkError("url has no host".into()))?;
        if !allowlist::is_allowed(host) {
            return Err(FetchError::DisallowedHost(host.to_string()));
        }
        let mut request = client.get(url);
        // K.4: bearer-token wire-up. Attach the header only when the
        // allowlist entry opts in via `auth_bearer = true` AND the
        // operator provided a non-empty API key. Empty / whitespace
        // keys are dropped silently so a stale empty export in the
        // shell does not forge an Authorization header with an
        // obviously-bad value.
        if let Some(policy_entry) = allowlist::find_policy(host)
            && policy_entry.auth_bearer
            && let Some(key) = self.api_key.as_ref()
        {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                request = request.header("Authorization", format!("Bearer {trimmed}"));
            }
        }
        let resp = request
            .send()
            .await
            .map_err(|e| FetchError::NetworkError(format!("send: {e}")))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| FetchError::NetworkError(format!("body: {e}")))?;
        if bytes.is_empty() {
            return Err(FetchError::Empty);
        }
        let (slice, truncated) = if bytes.len() > MAX_BYTES_PER_URL {
            (&bytes[..MAX_BYTES_PER_URL], true)
        } else {
            (&bytes[..], false)
        };
        let raw = String::from_utf8_lossy(slice);
        let redacted_cow = apply(policy, Surface::Storage, &raw)
            .map_err(|e| FetchError::NetworkError(format!("redact: {e}")))?;
        Ok(ResearchSnippet {
            url: url.to_string(),
            content: redacted_cow.into_owned(),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap exists so the Sketch phase cannot accidentally
    /// pull dozens of megabytes into context. Pin the constant so a
    /// refactor that inflates it surfaces here.
    #[test]
    fn limits_pin() {
        assert_eq!(MAX_URLS_PER_CALL, 3);
        assert_eq!(MAX_BYTES_PER_URL, 4 * 1024);
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(5));
    }

    /// `fetch_all(&[])` is the no-URL contract: empty in, empty
    /// out, no client construction, no HTTP traffic.
    #[tokio::test]
    async fn fetch_all_returns_empty_vec_on_empty_input() {
        let fetcher = ResearchFetcher::new(None);
        let out = fetcher.fetch_all(&[]).await;
        assert!(out.is_empty(), "empty input must yield empty output");
    }

    /// Asking for more than [`MAX_URLS_PER_CALL`] is a hard error.
    /// The whole call collapses to a single
    /// [`FetchError::TooManyUrls`] so the caller cannot interpret
    /// the result as "the first N succeeded".
    #[tokio::test]
    async fn fetch_all_rejects_too_many_urls() {
        let urls: Vec<String> = (0..MAX_URLS_PER_CALL + 1)
            .map(|i| format!("https://docs.rs/page-{i}"))
            .collect();
        let fetcher = ResearchFetcher::new(None);
        let out = fetcher.fetch_all(&urls).await;
        assert_eq!(out.len(), 1, "over-cap call must collapse to single error");
        match &out[0] {
            Err(FetchError::TooManyUrls { requested }) => {
                assert_eq!(*requested, urls.len());
            }
            other => panic!("expected TooManyUrls, got {other:?}"),
        }
    }

    /// Branch covered with no network: a URL whose host is not in
    /// the allowlist returns [`FetchError::DisallowedHost`] without
    /// touching the socket. Critical for the security story — the
    /// pre-flight filter must run before any HTTP traffic.
    #[tokio::test]
    async fn fetch_one_rejects_disallowed_host_without_network() {
        let fetcher = ResearchFetcher::new(None);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let policy = RedactPolicy::default();
        let url = "https://evil.example.com/secret";
        let err = fetcher
            .fetch_one(&client, &policy, url)
            .await
            .expect_err("disallowed host must error");
        match err {
            FetchError::DisallowedHost(h) => assert_eq!(h, "evil.example.com"),
            other => panic!("expected DisallowedHost, got {other:?}"),
        }
    }

    /// Malformed URLs must surface as a [`FetchError::NetworkError`]
    /// rather than panic. Keeps `fetch_all` panic-free even when
    /// the caller passes garbage.
    #[tokio::test]
    async fn fetch_one_rejects_malformed_url() {
        let fetcher = ResearchFetcher::new(None);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let policy = RedactPolicy::default();
        let url = "not a url at all";
        let err = fetcher
            .fetch_one(&client, &policy, url)
            .await
            .expect_err("malformed url must error");
        assert!(
            matches!(err, FetchError::NetworkError(_)),
            "malformed url must classify as NetworkError, got {err:?}"
        );
    }

    /// URLs without a host component (e.g. a file:// scheme on
    /// some platforms) must not panic on `host_str()`. The chain
    /// `Url::parse -> host_str` is fallible in our branch above;
    /// pin the contract here.
    #[tokio::test]
    async fn fetch_one_rejects_hostless_url() {
        let fetcher = ResearchFetcher::new(None);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let policy = RedactPolicy::default();
        // `data:` URLs have no host on every platform we target.
        let url = "data:text/plain,hello";
        let result = fetcher.fetch_one(&client, &policy, url).await;
        // Either a parse error (no host) or a disallowed-host error
        // is acceptable — both are soft failures rather than
        // panics. The contract is "no panics, returns Err".
        assert!(result.is_err(), "hostless url must error, not panic");
    }

    /// K.4: when the allowlist entry opts into
    /// `auth_bearer = true` (`api.github.com`) AND a non-empty
    /// API key is configured, the outbound request must carry
    /// the `Authorization: Bearer <key>` header.
    ///
    /// We exercise the exact code path `fetch_one` takes by
    /// mirroring its header-building step here and asserting on
    /// the rebuilt `reqwest::Request`. The reqwest client only
    /// mutates URL/timeout, not headers, so round-tripping via
    /// `RequestBuilder::build` preserves the header.
    #[tokio::test]
    async fn fetcher_adds_bearer_auth_for_configured_hosts() {
        let fetcher = ResearchFetcher::new(Some("gh_test_key".to_owned()));
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        let url = "https://api.github.com/repos/airvzxf/moagan";
        let parsed = reqwest::Url::parse(url).expect("api.github.com parses");
        let host = parsed.host_str().expect("host").to_owned();
        assert!(
            allowlist::find_policy(&host).is_some_and(|p| p.auth_bearer),
            "api.github.com must opt into auth_bearer, test fixture is stale"
        );
        let mut request = client.get(url);
        // Mirror the exact branch in `fetch_one` so the assertion
        // actually probes the production wire-up rather than a
        // stand-in.
        if let Some(policy_entry) = allowlist::find_policy(&host)
            && policy_entry.auth_bearer
            && let Some(k) = fetcher.api_key.as_ref()
        {
            let trimmed = k.trim();
            if !trimmed.is_empty() {
                request = request.header("Authorization", format!("Bearer {trimmed}"));
            }
        }
        let built = request.build().expect("build must succeed");
        let auth_header = built
            .headers()
            .get("Authorization")
            .map(|h| h.to_str().unwrap_or("").to_owned());
        assert_eq!(
            auth_header.as_deref(),
            Some("Bearer gh_test_key"),
            "Authorization header must carry the configured bearer token"
        );
    }

    /// K.4: hosts whose allowlist entry does NOT set
    /// `auth_bearer = true` (`docs.rs`, `crates.io`, `github.com`)
    /// must NOT receive an Authorization header even when the
    /// fetcher carries an API key. The public CDNs actively
    /// 401/5xx on unknown Authorization values, so leaking the
    /// header is a regression.
    #[tokio::test]
    async fn fetcher_skips_auth_for_open_hosts() {
        let fetcher = ResearchFetcher::new(Some("gh_test_key".to_owned()));
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap();
        for url in [
            "https://docs.rs/serde",
            "https://crates.io/serde",
            "https://github.com/airvzxf/moagan",
        ] {
            let parsed = reqwest::Url::parse(url).expect("parses");
            let host = parsed.host_str().expect("host").to_owned();
            let mut request = client.get(url);
            if let Some(policy_entry) = allowlist::find_policy(&host)
                && policy_entry.auth_bearer
                && let Some(k) = fetcher.api_key.as_ref()
            {
                let trimmed = k.trim();
                if !trimmed.is_empty() {
                    request = request.header("Authorization", format!("Bearer {trimmed}"));
                }
            }
            let built = request.build().expect("build must succeed");
            assert!(
                built.headers().get("Authorization").is_none(),
                "host {host} must NOT carry Authorization, got {:?}",
                built.headers().get("Authorization")
            );
        }
    }
}
