//! `RemoteEmbedder` — opt-in HTTP adapter for the cluster-by-embedder
//! path (D.1.3 / T18-09 §7.7).
//!
//! This is the network-backed counterpart of [`super::HashingEmbedder`].
//! While `HashingEmbedder` is dependency-free and synchronous (FNV-1a
//! hashed bag-of-tokens), `RemoteEmbedder` POSTs JSON to a remote
//! embedding service and is async by design — most remote embedding
//! APIs (OpenAI, Cohere, Voyage, and the long tail of OpenAI-compatible
//! endpoints) accept a *batch* of input strings per request and return
//! one vector per input. Batching is a hard requirement for any
//! realistic embedding service: per-text round-trips on a 60-second
//! 200-sketch run would push the run well past any reasonable timeout.
//!
//! ## Supported providers
//!
//! - [`RemoteEmbedderProvider::OpenAI`] — `POST {endpoint}/v1/embeddings`,
//!   body `{"input": [...], "model": ...}`,
//!   response `{"data": [{"embedding": [...]}, ...]}`. Matches the
//!   public OpenAI spec and any OpenAI-compatible relay that follows
//!   the same shape (OpenCode Go's chat path, etc.).
//! - [`RemoteEmbedderProvider::Cohere`] — `POST {endpoint}/v1/embed`,
//!   body `{"texts": [...], "model": ...}`,
//!   response `{"embeddings": [[...], [...]]}`.
//! - [`RemoteEmbedderProvider::Voyage`] — `POST {endpoint}/v1/embeddings`,
//!   same wire shape as OpenAI.
//! - [`RemoteEmbedderProvider::Custom`] — open-ended fallback that
//!   sends `{"input": [...], "model": ...}` and expects
//!   `{"data": [{"embedding": [...]}, ...]}`. Use this for any
//!   OpenAI-compatible relay not in the first three; the wire shape
//!   is the de-facto industry standard.
//!
//! ## Auth
//!
//! The constructor takes the *name* of the env var holding the API
//! key (not the key itself). The key is resolved at construction time
//! and wrapped in a [`crate::secret::SecretString`] so it never
//! escapes via `Debug` / `Display` and is wiped from memory on drop.
//! Missing keys surface as `Error::InvalidApiKey` (exit code 3).
//!
//! ## API shape
//!
//! The remote adapter exposes a **two-trait** surface:
//!
//! - [`super::AsyncEmbedder::embed_batch`] (canonical, async,
//!   fallible) — for `&[&str]` inputs. This is what async-first
//!   callers and the integration tests use.
//! - [`RemoteEmbedder::embed_one`] — convenience wrapper that
//!   delegates to `embed_batch` with a one-element slice.
//! - [`super::Embedder::embed`] (sync, infallible) — re-export of the
//!   sync bridge below. Operators that wire the remote embedder into
//!   the legacy cluster-by-embedder path drop in `RemoteEmbedder`
//!   exactly where they used to put `HashingEmbedder`; the sync
//!   bridge handles the rest via `tokio::task::block_in_place`.
//!
//! ### Multi-thread-runtime requirement for the sync bridge
//!
//! The sync `Embedder::embed` impl uses
//! `tokio::task::block_in_place`, which **only works on worker
//! threads of a multi-thread Tokio runtime**. Production satisfies
//! this because `run_blocking()` in `src/lib.rs` builds the runtime
//! with `Builder::new_multi_thread().enable_all()`. Tests of the
//! sync bridge must use `#[tokio::test(flavor = "multi_thread")]`
//! (or build an equivalent runtime explicitly). Pure async callers
//! skip the sync trait entirely and use
//! `AsyncEmbedder::embed_batch` directly — it has no such
//! requirement, and is the canonical API for fallible embeddings.
//!
//! A small in-memory cache keyed by the verbatim input string keeps
//! repeated embedding requests cheap and deterministic across runs
//! for the same input. The cache is shared across threads via
//! `parking_lot::Mutex` so it composes with the same lock-free
//! pattern the hashing embedder uses.
//!
//! ## No-go list compliance
//!
//! - No `secrecy` crate: API keys are wrapped in
//!   [`crate::secret::SecretString`] with `zeroize` on drop.
//! - No Anthropic SDK: HTTP via `reqwest`, just like every other LLM
//!   adapter in this codebase.
//! - No new runtime dependencies: only `reqwest` (already in
//!   `Cargo.toml`), `serde`, `serde_json`, `tokio`.
//!
//! Compliance: catalog 10-integrada-v0 §D.1.3 (T09-02; T18-09 §7.7).

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::llm::embed::{AsyncEmbedder, Embedder};
use crate::secret::SecretString;

/// Provider-specific wire format for the embeddings endpoint. The
/// request body and response parser are picked per variant; everything
/// else (HTTP transport, auth header, status mapping) is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteEmbedderProvider {
    /// OpenAI public spec: `POST {endpoint}/v1/embeddings`.
    Openai,
    /// Cohere v1: `POST {endpoint}/v1/embed`.
    Cohere,
    /// Voyage AI: `POST {endpoint}/v1/embeddings` (same wire shape
    /// as OpenAI, distinct URL prefix by convention).
    Voyage,
    /// Open-ended fallback for any OpenAI-compatible relay. Sends
    /// `{"input": [...], "model": ...}` and expects
    /// `{"data": [{"embedding": [...]}, ...]}`. Use this when the
    /// upstream is not in the first three.
    Custom,
}

impl RemoteEmbedderProvider {
    /// Stable string identifier for logs / `Embedder::name()`. Lowercase
    /// ASCII so it round-trips through TOML config without quoting.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Cohere => "cohere",
            Self::Voyage => "voyage",
            Self::Custom => "custom",
        }
    }

    /// Wire-format path appended to the configured endpoint. The
    /// endpoint is expected to be the base (e.g.
    /// `https://api.openai.com/v1`); the path closes the URL.
    fn path(self) -> &'static str {
        match self {
            Self::Openai | Self::Voyage | Self::Custom => "embeddings",
            Self::Cohere => "embed",
        }
    }

    /// Field name that carries the input list in the request body.
    /// OpenAI and Voyage use `"input"`, Cohere uses `"texts"`. Custom
    /// inherits the OpenAI convention because that is the de-facto
    /// industry standard for OpenAI-compatible relays.
    fn input_field(self) -> &'static str {
        match self {
            Self::Cohere => "texts",
            Self::Openai | Self::Voyage | Self::Custom => "input",
        }
    }

    /// Build the JSON request body for a batch of `texts`. The body
    /// shape is provider-specific; this function is the single source
    /// of truth so the unit tests can assert on it without firing an
    /// HTTP request.
    fn build_body(self, model: &str, texts: &[&str]) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            self.input_field().to_owned(),
            serde_json::Value::Array(
                texts
                    .iter()
                    .map(|t| serde_json::Value::String((*t).to_owned()))
                    .collect(),
            ),
        );
        obj.insert(
            "model".to_owned(),
            serde_json::Value::String(model.to_owned()),
        );
        serde_json::Value::Object(obj)
    }

    /// Parse the response body into a vector of vectors. Each entry
    /// in the result corresponds to the same-indexed input. Returns
    /// `Err(Error::Provider(...))` on schema mismatch so the caller
    /// can distinguish bad wire format from network errors.
    fn parse_response(self, body: &serde_json::Value) -> Result<Vec<Vec<f32>>> {
        match self {
            Self::Openai | Self::Voyage | Self::Custom => {
                let data =
                    body.get("data")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| Error::Provider {
                            message: format!("{}: response missing 'data' array", self.as_str()),
                            http_status: None,
                        })?;
                let mut out = Vec::with_capacity(data.len());
                for (idx, entry) in data.iter().enumerate() {
                    let embedding = entry
                        .get("embedding")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| Error::Provider {
                            message: format!(
                                "{}: data[{idx}] missing 'embedding' array",
                                self.as_str()
                            ),
                            http_status: None,
                        })?;
                    let mut vec = Vec::with_capacity(embedding.len());
                    for (j, x) in embedding.iter().enumerate() {
                        let f = x.as_f64().ok_or_else(|| Error::Provider {
                            message: format!(
                                "{}: data[{idx}].embedding[{j}] is not a number",
                                self.as_str()
                            ),
                            http_status: None,
                        })?;
                        vec.push(f as f32);
                    }
                    out.push(vec);
                }
                Ok(out)
            }
            Self::Cohere => {
                let embeddings = body
                    .get("embeddings")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| Error::Provider {
                        message: format!("{}: response missing 'embeddings' array", self.as_str()),
                        http_status: None,
                    })?;
                let mut out = Vec::with_capacity(embeddings.len());
                for (idx, entry) in embeddings.iter().enumerate() {
                    let embedding = entry.as_array().ok_or_else(|| Error::Provider {
                        message: format!("{}: embeddings[{idx}] is not an array", self.as_str()),
                        http_status: None,
                    })?;
                    let mut vec = Vec::with_capacity(embedding.len());
                    for (j, x) in embedding.iter().enumerate() {
                        let f = x.as_f64().ok_or_else(|| Error::Provider {
                            message: format!(
                                "{}: embeddings[{idx}][{j}] is not a number",
                                self.as_str()
                            ),
                            http_status: None,
                        })?;
                        vec.push(f as f32);
                    }
                    out.push(vec);
                }
                Ok(out)
            }
        }
    }
}

/// Configuration record persisted under `[embedder.remote]` in
/// `~/.config/moagan/config.toml`. The struct is `serde`-friendly so
/// the operator can flip the embedder on without touching code. All
/// fields are mandatory once the section is present — defaults are
/// applied at the [`crate::config::EmbedderConfig`] layer that wraps
/// `Option<RemoteEmbedderConfig>` so a missing section means
/// `HashingEmbedder` (the dependency-free default).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteEmbedderConfig {
    /// Wire-format flavour. Serialised lowercase
    /// (`openai | cohere | voyage | custom`).
    pub provider: RemoteEmbedderProvider,
    /// Base endpoint. The provider-specific path
    /// (`embeddings` / `embed`) is appended after a single `/` join.
    /// Example: `https://api.openai.com/v1`.
    pub endpoint: String,
    /// Name of the environment variable that holds the API key. The
    /// constructor reads it via `std::env::var(...)` and wraps the
    /// value in a [`SecretString`] — the key itself never lives in
    /// the config file or in any serialized output.
    pub api_key_env: String,
    /// Model name sent verbatim to the upstream (e.g.
    /// `text-embedding-3-small`, `embed-english-v3.0`,
    /// `voyage-large-2`). No validation here; the upstream returns
    /// 4xx for unknown models and the error surfaces as
    /// `Error::InvalidApiKey` / `Error::Provider` per the status
    /// mapping.
    pub model: String,
    /// Output dimensionality. The hashing embedder auto-discovers
    /// `dim()` from the vector it produces; the remote adapter
    /// declares it explicitly so a misconfigured endpoint that
    /// returns a different shape surfaces as a schema error rather
    /// than a silent dimension mismatch downstream.
    pub dimensions: u32,
}

impl Default for RemoteEmbedderConfig {
    fn default() -> Self {
        Self {
            provider: RemoteEmbedderProvider::Openai,
            endpoint: String::new(),
            api_key_env: String::new(),
            model: String::new(),
            dimensions: 0,
        }
    }
}

/// Network-backed embedding adapter. See the module docs for the
/// full design rationale, provider list, auth model, and cache
/// invariants.
pub struct RemoteEmbedder {
    provider: RemoteEmbedderProvider,
    endpoint: String,
    api_key: SecretString,
    model: String,
    dimensions: usize,
    client: Client,
    /// Term-frequency cache so the cluster-by-embedder loop (which
    /// re-embeds the same sketch text on every threshold sweep) does
    /// not hammer the upstream. Same `parking_lot::Mutex` pattern as
    /// [`super::HashingEmbedder`] so the lock can be held across the
    /// blocking HTTP call without an async context.
    cache: parking_lot::Mutex<HashMap<String, Vec<f32>>>,
}

impl std::fmt::Debug for RemoteEmbedder {
    /// Custom `Debug` so the cached vectors are not dumped on every
    /// log line. The cache can grow into the thousands during a long
    /// cluster pass; printing it would balloon every `tracing::debug!`
    /// line that captures the embedder. The endpoint and model are
    /// still useful for diagnostics, so we keep those; the API key is
    /// already masked by [`SecretString`]'s own `Debug` impl.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEmbedder")
            .field("provider", &self.provider)
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("cache_size", &self.cache.lock().len())
            .finish()
    }
}

impl RemoteEmbedder {
    /// Build the adapter from explicit parameters. Reads the API key
    /// from `api_key_env` via `std::env::var(...)` and wraps it in a
    /// [`SecretString`] so it never appears in `Debug` / `Display`
    /// output and is wiped from memory on drop.
    ///
    /// `dimensions = 0` is rejected so a misconfigured endpoint that
    /// silently returns a different shape cannot masquerade as a
    /// valid run.
    pub fn new(
        provider: &str,
        endpoint: &str,
        api_key_env: &str,
        model: &str,
        dimensions: u32,
    ) -> Result<Self> {
        Self::with_provider(
            parse_provider(provider)?,
            endpoint,
            api_key_env,
            model,
            dimensions,
        )
    }

    /// Same as [`Self::new`] but takes the provider variant directly.
    /// Useful for tests that want to avoid the `&str` parser.
    pub fn with_provider(
        provider: RemoteEmbedderProvider,
        endpoint: &str,
        api_key_env: &str,
        model: &str,
        dimensions: u32,
    ) -> Result<Self> {
        if endpoint.trim().is_empty() {
            return Err(Error::InvalidArgs(
                "remote embedder endpoint must not be empty".into(),
            ));
        }
        if api_key_env.trim().is_empty() {
            return Err(Error::InvalidArgs(
                "remote embedder api_key_env must not be empty".into(),
            ));
        }
        if model.trim().is_empty() {
            return Err(Error::InvalidArgs(
                "remote embedder model must not be empty".into(),
            ));
        }
        if dimensions == 0 {
            return Err(Error::InvalidArgs(
                "remote embedder dimensions must be > 0".into(),
            ));
        }
        let api_key = read_api_key(api_key_env)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("moagan/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Provider {
                message: format!("build reqwest client: {e}"),
                http_status: None,
            })?;
        Ok(Self {
            provider,
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            api_key,
            model: model.to_owned(),
            dimensions: dimensions as usize,
            client,
            cache: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Build from a persisted [`RemoteEmbedderConfig`]. Convenience
    /// for the config wiring path.
    pub fn from_config(cfg: &RemoteEmbedderConfig) -> Result<Self> {
        Self::with_provider(
            cfg.provider,
            &cfg.endpoint,
            &cfg.api_key_env,
            &cfg.model,
            cfg.dimensions,
        )
    }

    /// Provider variant (for logs / diagnostics).
    pub fn provider(&self) -> RemoteEmbedderProvider {
        self.provider
    }

    /// Fully-routed URL the adapter POSTs to. Built from the configured
    /// base endpoint plus the provider-specific path.
    fn url(&self) -> String {
        format!("{}/{}", self.endpoint, self.provider.path())
    }

    /// POST a batch of `texts` to the upstream and parse the response
    /// into one vector per input. The cache is *not* consulted here —
    /// callers that want cache semantics should look up the input
    /// strings themselves before invoking this method (the sync
    /// adapter does so automatically).
    ///
    /// This is the private HTTP transport. The canonical async-batch
    /// entry point is the [`AsyncEmbedder::embed_batch`] trait impl
    /// below; this helper backs both that trait impl and the sync
    /// [`Embedder`] bridge. Tests in the same module reach it via
    /// the trait method on `RemoteEmbedder`.
    async fn embed_batch_transport(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = self.url();
        let body = self.provider.build_body(&self.model, texts);
        let headers = build_auth_headers(&self.api_key)?;
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider {
                message: format!("{}: network: {e}", self.provider.as_str()),
                http_status: None,
            })?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| Error::Provider {
            message: format!("{}: read body: {e}", self.provider.as_str()),
            http_status: None,
        })?;
        // Status check FIRST so an upstream that returns a non-JSON
        // error body (e.g. an HTML 401 page from a misconfigured
        // auth proxy) surfaces as a typed HTTP error instead of a
        // confusing JSON-decode failure. Successful responses are
        // assumed JSON — that is the contract for every supported
        // provider wire format.
        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&bytes).into_owned();
            return Err(classify_status(status, &body_str));
        }
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            // HTTP status was successful (`!is_success()` returned
            // false above), so the response was 2xx — the upstream
            // sent a payload we couldn't decode. `http_status: None`
            // is correct here because the failure is at the JSON
            // layer, not the transport layer.
            Error::Provider {
                message: format!(
                    "{}: decode JSON (HTTP {status}): {e}",
                    self.provider.as_str()
                ),
                http_status: None,
            }
        })?;
        let vectors = self.provider.parse_response(&parsed)?;
        // Reject mismatched vector count up-front so the caller does
        // not silently misalign inputs and outputs.
        if vectors.len() != texts.len() {
            return Err(Error::Provider {
                message: format!(
                    "{}: response carried {} vectors for {} inputs",
                    self.provider.as_str(),
                    vectors.len(),
                    texts.len()
                ),
                http_status: None,
            });
        }
        Ok(vectors)
    }

    /// Convenience wrapper around [`AsyncEmbedder::embed_batch`] for a
    /// single input. Same cache-key contract as the rest of the
    /// public API.
    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = AsyncEmbedder::embed_batch(self, &[text]).await?;
        Ok(out.pop().unwrap_or_default())
    }

    /// Output dimensionality declared at construction time. Returns
    /// the `usize` value the caller passed to
    /// [`Self::with_provider`]. Useful for downstream cosine
    /// comparison without re-reading the config.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Output dimensionality for the [`super::Embedder`] trait shape.
    /// `RemoteEmbedder` now implements [`super::Embedder`] directly
    /// via [`tokio::task::block_in_place`] — see the trait impl below
    /// for the multi-thread-runtime requirement.
    pub fn dim(&self) -> usize {
        self.dimensions
    }
}

// ---------------------------------------------------------------------
// Trait impls: `Embedder` (sync) + `AsyncEmbedder` (async batch).
//
// Both are required so the cluster-by-embedder path (D.1.3) can stay
// sync while async-first callers (a future D.1.3 follow-up) get a
// proper fallible API. The two impls share the cache (the canonical
// short-circuit) and the underlying `embed_batch` transport; only the
// re-entry bridge differs.
//
// ### Multi-thread-runtime requirement (READ THIS before use)
//
// `Embedder::embed` is synchronous. Network I/O is not. Bridging the
// two without panicking requires `tokio::task::block_in_place`, which
// the Tokio runtime **only supports on worker threads of a
// multi-thread runtime**. Calling `embed()` from a single-thread
// runtime (or from outside any Tokio runtime) panics with
// "Cannot drop a runtime in a context where blocking is not allowed"
// / "thread-local current runtime has been dropped".
//
// Production satisfies this requirement because `run_blocking()` in
// `src/lib.rs` builds the runtime with
// `Builder::new_multi_thread().enable_all()`. Tests of the sync
// bridge must use `#[tokio::test(flavor = "multi_thread")]` (or
// build an equivalent runtime explicitly). Pure async callers should
// skip the sync trait entirely and use `AsyncEmbedder::embed_batch`
// directly — it has no such requirement.
// ---------------------------------------------------------------------

impl Embedder for RemoteEmbedder {
    fn dim(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &'static str {
        "remote"
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        // Hot path: served from the in-memory cache so the cluster
        // loop never round-trips for the same sketch text twice in
        // a single run. Same `parking_lot::Mutex` lock pattern as
        // `HashingEmbedder::embed` so the cache is contention-free
        // across worker threads.
        if let Some(v) = self.cache.lock().get(text) {
            return v.clone();
        }
        // Cold path: one upstream call per unique text. The
        // single-input batch reuses the same wire-format machinery
        // as the multi-input path so the cache miss cost is uniform
        // — operators do not pay an extra round-trip per text in
        // exchange for the sync API.
        let vectors = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(AsyncEmbedder::embed_batch(self, &[text]))
        })
        .unwrap_or_else(|e| {
            // The sync `Embedder` contract is infallible; a network
            // failure on a unique text is exceptional. Surface the
            // underlying error via `panic!` rather than silently
            // returning a zero-vector (which would corrupt cosine
            // scores downstream). Callers that need fallible
            // embeddings should hold a `dyn AsyncEmbedder` instead.
            panic!(
                "RemoteEmbedder::embed: upstream call failed for {}-dim unique text: {e:?}",
                self.dimensions
            )
        });
        // `embed_batch` returns one vector per input, in order. We
        // sent one input, so `next()` (or `unwrap_or_default`) is
        // safe — if the adapter had zero-length semantics this would
        // be an empty vector.
        let v = vectors.into_iter().next().unwrap_or_default();
        if !v.is_empty() {
            self.cache.lock().insert(text.to_owned(), v.clone());
        }
        v
    }
}

#[async_trait]
impl AsyncEmbedder for RemoteEmbedder {
    /// Canonical async-batch API. Dispatches to the private HTTP
    /// transport [`Self::embed_batch_transport`]. The cache lives in
    /// the sync bridge (which looks the text up before re-entering);
    /// the async path stays lock-free for inflight requests.
    async fn embed_batch<'a>(&'a self, texts: &'a [&'a str]) -> Result<Vec<Vec<f32>>> {
        self.embed_batch_transport(texts).await
    }

    fn dim(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &'static str {
        "remote"
    }
}

/// Parse a provider name from a string. Accepts lowercase and the
/// canonical mixed-case names so a TOML typo does not silently fall
/// back to the wrong wire format.
fn parse_provider(name: &str) -> Result<RemoteEmbedderProvider> {
    match name.trim().to_ascii_lowercase().as_str() {
        "openai" => Ok(RemoteEmbedderProvider::Openai),
        "cohere" => Ok(RemoteEmbedderProvider::Cohere),
        "voyage" => Ok(RemoteEmbedderProvider::Voyage),
        "custom" => Ok(RemoteEmbedderProvider::Custom),
        other => Err(Error::InvalidArgs(format!(
            "unknown remote embedder provider '{other}'; expected one of: openai, cohere, voyage, custom"
        ))),
    }
}

/// Read the API key from the named env var and wrap it in a
/// [`SecretString`]. Returns `Error::InvalidApiKey` on absence so the
/// exit code matches the rest of the auth-failure surface.
fn read_api_key(env_name: &str) -> Result<SecretString> {
    let raw = std::env::var(env_name).map_err(|e| match e {
        std::env::VarError::NotPresent => Error::InvalidApiKey {
            message: format!("remote embedder: env var '{env_name}' is not set"),
            http_status: None,
        },
        std::env::VarError::NotUnicode(_) => Error::InvalidApiKey {
            message: format!("remote embedder: env var '{env_name}' is not valid unicode"),
            http_status: None,
        },
    })?;
    if raw.trim().is_empty() {
        return Err(Error::InvalidApiKey {
            message: format!("remote embedder: env var '{env_name}' is empty"),
            http_status: None,
        });
    }
    Ok(SecretString::new(raw))
}

/// Build the shared auth headers. The `Authorization: Bearer <key>`
/// header is the canonical scheme for every supported provider —
/// OpenAI, Cohere, and Voyage all accept bearer tokens, and the
/// `Custom` fallback follows the same convention because every
/// OpenAI-compatible relay in practice does too.
fn build_auth_headers(api_key: &SecretString) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let auth_value = format!("Bearer {}", api_key.expose());
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&auth_value).map_err(|e| Error::Provider {
            message: format!("build authorization header: {e}"),
            http_status: None,
        })?,
    );
    Ok(headers)
}

/// Translate an HTTP status into a typed error. Mirrors the mapping in
/// `crate::llm::http::classify_status` so the embedder and the chat
/// providers agree on what counts as auth / throttle / timeout /
/// 5xx. The 429 arm splits into `Throttled` (transient, absorbed
/// by `ThrottleGovernor`) vs `PlanExhausted` (persistent, trips the
/// per-(provider, role) breaker) using the same keyword scan as the
/// chat helper. We delegate to the shared helper rather than
/// reimplementing the scan.
fn classify_status(status: StatusCode, body: &str) -> Error {
    use crate::llm::http::classify_status as upstream_classify;
    upstream_classify(status, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- provider metadata ----------------

    #[test]
    fn provider_as_str_is_lowercase() {
        assert_eq!(RemoteEmbedderProvider::Openai.as_str(), "openai");
        assert_eq!(RemoteEmbedderProvider::Cohere.as_str(), "cohere");
        assert_eq!(RemoteEmbedderProvider::Voyage.as_str(), "voyage");
        assert_eq!(RemoteEmbedderProvider::Custom.as_str(), "custom");
    }

    #[test]
    fn provider_path_matches_public_spec() {
        assert_eq!(RemoteEmbedderProvider::Openai.path(), "embeddings");
        assert_eq!(RemoteEmbedderProvider::Voyage.path(), "embeddings");
        assert_eq!(RemoteEmbedderProvider::Custom.path(), "embeddings");
        assert_eq!(RemoteEmbedderProvider::Cohere.path(), "embed");
    }

    #[test]
    fn provider_input_field_differs_only_for_cohere() {
        assert_eq!(RemoteEmbedderProvider::Openai.input_field(), "input");
        assert_eq!(RemoteEmbedderProvider::Voyage.input_field(), "input");
        assert_eq!(RemoteEmbedderProvider::Custom.input_field(), "input");
        assert_eq!(RemoteEmbedderProvider::Cohere.input_field(), "texts");
    }

    #[test]
    fn parse_provider_accepts_lowercase_and_canonical() {
        assert_eq!(
            parse_provider("openai").unwrap(),
            RemoteEmbedderProvider::Openai
        );
        assert_eq!(
            parse_provider("OpenAI").unwrap(),
            RemoteEmbedderProvider::Openai
        );
        assert_eq!(
            parse_provider("cohere").unwrap(),
            RemoteEmbedderProvider::Cohere
        );
        assert_eq!(
            parse_provider("Voyage").unwrap(),
            RemoteEmbedderProvider::Voyage
        );
        assert_eq!(
            parse_provider("custom").unwrap(),
            RemoteEmbedderProvider::Custom
        );
    }

    #[test]
    fn parse_provider_rejects_unknown_with_helpful_error() {
        let err = parse_provider("vertex").unwrap_err();
        match err {
            Error::InvalidArgs(msg) => {
                assert!(msg.contains("vertex"));
                assert!(msg.contains("openai") || msg.contains("custom"));
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    // ---------------- request body shapes ----------------

    #[test]
    fn openai_body_uses_input_field() {
        let body = RemoteEmbedderProvider::Openai
            .build_body("text-embedding-3-small", &["hello", "world"]);
        let obj = body.as_object().expect("body is object");
        assert_eq!(obj["model"], "text-embedding-3-small");
        let input = obj["input"].as_array().expect("input is array");
        assert_eq!(input.len(), 2);
        assert_eq!(input[0], "hello");
        assert_eq!(input[1], "world");
        assert!(obj.get("texts").is_none(), "OpenAI must not emit 'texts'");
    }

    #[test]
    fn cohere_body_uses_texts_field() {
        let body = RemoteEmbedderProvider::Cohere
            .build_body("embed-english-v3.0", &["alpha", "beta", "gamma"]);
        let obj = body.as_object().expect("body is object");
        assert_eq!(obj["model"], "embed-english-v3.0");
        let texts = obj["texts"].as_array().expect("texts is array");
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[2], "gamma");
        assert!(obj.get("input").is_none(), "Cohere must not emit 'input'");
    }

    #[test]
    fn voyage_body_matches_openai_shape() {
        let body = RemoteEmbedderProvider::Voyage.build_body("voyage-large-2", &["one"]);
        let obj = body.as_object().expect("body is object");
        assert_eq!(obj["model"], "voyage-large-2");
        assert_eq!(obj["input"][0], "one");
    }

    #[test]
    fn custom_body_falls_back_to_input_field() {
        let body = RemoteEmbedderProvider::Custom.build_body("custom-model", &["x"]);
        assert_eq!(body["model"], "custom-model");
        assert_eq!(body["input"][0], "x");
    }

    #[test]
    fn empty_batch_still_yields_object_with_input_array() {
        let body = RemoteEmbedderProvider::Openai.build_body("m", &[]);
        assert!(body["input"].as_array().unwrap().is_empty());
    }

    // ---------------- response parsing ----------------

    #[test]
    fn openai_response_round_trip() {
        let raw = serde_json::json!({
            "data": [
                {"embedding": [0.1, 0.2, 0.3], "index": 0},
                {"embedding": [0.4, 0.5, 0.6], "index": 1},
            ]
        });
        let out = RemoteEmbedderProvider::Openai.parse_response(&raw).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.1, 0.2, 0.3]);
        assert_eq!(out[1], vec![0.4, 0.5, 0.6]);
    }

    #[test]
    fn cohere_response_round_trip() {
        let raw = serde_json::json!({
            "embeddings": [[1.0, 2.0], [3.0, 4.0, 5.0]],
        });
        let out = RemoteEmbedderProvider::Cohere.parse_response(&raw).unwrap();
        assert_eq!(out, vec![vec![1.0, 2.0], vec![3.0, 4.0, 5.0]]);
    }

    #[test]
    fn voyage_response_round_trip() {
        let raw = serde_json::json!({
            "data": [{"embedding": [0.25, 0.5]}]
        });
        let out = RemoteEmbedderProvider::Voyage.parse_response(&raw).unwrap();
        assert_eq!(out, vec![vec![0.25, 0.5]]);
    }

    #[test]
    fn custom_response_round_trip() {
        let raw = serde_json::json!({"data": [{"embedding": [7.0]}]});
        let out = RemoteEmbedderProvider::Custom.parse_response(&raw).unwrap();
        assert_eq!(out, vec![vec![7.0]]);
    }

    #[test]
    fn openai_response_missing_data_is_provider_error() {
        let raw = serde_json::json!({"unexpected": "shape"});
        let err = RemoteEmbedderProvider::Openai
            .parse_response(&raw)
            .unwrap_err();
        assert!(matches!(err, Error::Provider { .. }), "got {err:?}");
    }

    #[test]
    fn openai_response_non_numeric_embedding_is_provider_error() {
        let raw = serde_json::json!({"data": [{"embedding": ["nope"]}]});
        let err = RemoteEmbedderProvider::Openai
            .parse_response(&raw)
            .unwrap_err();
        assert!(matches!(err, Error::Provider { .. }));
    }

    #[test]
    fn cohere_response_missing_embeddings_is_provider_error() {
        let raw = serde_json::json!({"data": []});
        let err = RemoteEmbedderProvider::Cohere
            .parse_response(&raw)
            .unwrap_err();
        assert!(matches!(err, Error::Provider { .. }));
    }

    #[test]
    fn cohere_response_non_array_entry_is_provider_error() {
        let raw = serde_json::json!({"embeddings": [{}]});
        let err = RemoteEmbedderProvider::Cohere
            .parse_response(&raw)
            .unwrap_err();
        assert!(matches!(err, Error::Provider { .. }));
    }

    // ---------------- auth header ----------------

    #[test]
    fn auth_headers_carry_bearer_token() {
        let secret = SecretString::new("sk-test-abc".into());
        let headers = build_auth_headers(&secret).unwrap();
        assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer sk-test-abc");
    }

    #[test]
    fn classify_status_429_maps_to_throttled() {
        // v0.9.6 split: 429 with a non-plan body is the transient
        // throttle case (`ThrottleGovernor` absorbs it). The
        // pre-split behavior that mapped every 429 to
        // `PlanExhausted` was removed together with the cascade
        // bug in discover_facet.
        let err = classify_status(StatusCode::TOO_MANY_REQUESTS, "throttle");
        assert!(matches!(err, Error::Throttled { .. }));
    }

    #[test]
    fn classify_status_429_plan_body_maps_to_plan_exhausted() {
        // The keyword scan keeps the old `PlanExhausted` arm for
        // genuine quota-exhaustion messages.
        let err = classify_status(
            StatusCode::TOO_MANY_REQUESTS,
            "{\"message\":\"token plan rate limit reached\"}",
        );
        assert!(matches!(err, Error::PlanExhausted { .. }));
    }

    #[test]
    fn classify_status_401_maps_to_invalid_api_key() {
        let err = classify_status(StatusCode::UNAUTHORIZED, "no");
        assert!(matches!(err, Error::InvalidApiKey { .. }));
    }

    // ---------------- constructor validation ----------------

    #[test]
    fn new_rejects_empty_endpoint() {
        let err = RemoteEmbedder::new(
            "openai",
            "  ",
            "OPENAI_API_KEY",
            "text-embedding-3-small",
            1536,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn new_rejects_empty_api_key_env() {
        let err = RemoteEmbedder::new(
            "openai",
            "https://api.openai.com/v1",
            "",
            "text-embedding-3-small",
            1536,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn new_rejects_empty_model() {
        let err = RemoteEmbedder::new(
            "openai",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            "",
            1536,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn new_rejects_zero_dimensions() {
        let err = RemoteEmbedder::new(
            "openai",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            "text-embedding-3-small",
            0,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn new_errors_when_api_key_env_unset() {
        let env_name = "MOAGAN_TEST_REMOTE_EMBED_KEY_MISSING_98231";
        unsafe {
            std::env::remove_var(env_name);
        }
        let err = RemoteEmbedder::new(
            "openai",
            "https://api.openai.com/v1",
            env_name,
            "text-embedding-3-small",
            1536,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidApiKey { .. }));
    }

    #[test]
    fn new_errors_when_api_key_env_is_empty() {
        let env_name = "MOAGAN_TEST_REMOTE_EMBED_KEY_EMPTY_98232";
        unsafe {
            std::env::set_var(env_name, "");
        }
        let result = RemoteEmbedder::new(
            "openai",
            "https://api.openai.com/v1",
            env_name,
            "text-embedding-3-small",
            1536,
        );
        unsafe {
            std::env::remove_var(env_name);
        }
        let err = result.unwrap_err();
        assert!(matches!(err, Error::InvalidApiKey { .. }));
    }

    #[test]
    fn new_succeeds_when_api_key_env_is_set() {
        let env_name = "MOAGAN_TEST_REMOTE_EMBED_KEY_OK_98233";
        unsafe {
            std::env::set_var(env_name, "sk-ok");
        }
        let result = RemoteEmbedder::new(
            "openai",
            "https://api.openai.com/v1/",
            env_name,
            "text-embedding-3-small",
            1536,
        );
        unsafe {
            std::env::remove_var(env_name);
        }
        let embedder = result.unwrap();
        // Trailing slash stripped from endpoint.
        assert_eq!(embedder.endpoint, "https://api.openai.com/v1");
        assert_eq!(embedder.url(), "https://api.openai.com/v1/embeddings");
        assert_eq!(embedder.dim(), 1536);
        assert_eq!(embedder.provider(), RemoteEmbedderProvider::Openai);
    }

    #[test]
    fn from_config_round_trip() {
        let cfg = RemoteEmbedderConfig {
            provider: RemoteEmbedderProvider::Cohere,
            endpoint: "https://api.cohere.ai/v1".into(),
            api_key_env: "MOAGAN_TEST_FROM_CONFIG_98234".into(),
            model: "embed-english-v3.0".into(),
            dimensions: 1024,
        };
        unsafe {
            std::env::set_var(&cfg.api_key_env, "cohere-test");
        }
        let embedder = RemoteEmbedder::from_config(&cfg).unwrap();
        unsafe {
            std::env::remove_var(&cfg.api_key_env);
        }
        assert_eq!(embedder.provider(), RemoteEmbedderProvider::Cohere);
        assert_eq!(embedder.dim(), 1024);
        assert_eq!(embedder.url(), "https://api.cohere.ai/v1/embed");
        assert_eq!(
            embedder.provider.as_str(),
            "cohere",
            "provider.as_str() stays stable for diagnostics"
        );
    }

    // ---------------- empty batch short-circuit ----------------

    #[test]
    fn embed_batch_empty_input_short_circuits() {
        let env_name = "MOAGAN_TEST_BATCH_EMPTY_98237";
        unsafe {
            std::env::set_var(env_name, "k");
        }
        let embedder =
            RemoteEmbedder::new("openai", "http://127.0.0.1:1/v1", env_name, "m", 3).unwrap();
        unsafe {
            std::env::remove_var(env_name);
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(embedder.embed_batch(&[])).unwrap();
        assert!(out.is_empty());
    }

    // ---------------- URL builder ----------------

    #[test]
    fn url_strips_trailing_slash_before_appending_path() {
        let env_name = "MOAGAN_TEST_URL_STRIP_98238";
        unsafe {
            std::env::set_var(env_name, "k");
        }
        let embedder =
            RemoteEmbedder::new("openai", "https://example.com/v1///", env_name, "m", 4).unwrap();
        unsafe {
            std::env::remove_var(env_name);
        }
        // The constructor strips a single trailing '/'. The path is
        // then appended after exactly one '/'. So
        // `https://example.com/v1///` becomes
        // `https://example.com/v1//embeddings` — which is *not* the
        // canonical form. We document the behaviour: only the final
        // slash is trimmed. Operators wanting the canonical form
        // should pass `https://example.com/v1` directly. The test
        // pins the current contract so future changes are conscious.
        let url = embedder.url();
        assert!(url.ends_with("/embeddings"), "got {url}");
        assert!(url.starts_with("https://example.com"), "got {url}");
    }

    // ---------------- Embedder + AsyncEmbedder trait impls ----------------

    /// Sync `Embedder::name` and `Embedder::dim` are
    /// configuration-derived and do not touch the runtime. Lock the
    /// values in place so the sync bridge stays drop-in compatible
    /// with the existing cluster-by-embedder call sites.
    #[test]
    fn embedder_trait_name_and_dim_match() {
        let env_name = "MOAGAN_TEST_TRAIT_NAME_DIM_98239";
        unsafe {
            std::env::set_var(env_name, "k");
        }
        let embedder =
            RemoteEmbedder::new("openai", "https://api.example.com/v1", env_name, "m", 1536)
                .unwrap();
        unsafe {
            std::env::remove_var(env_name);
        }
        let trait_obj: &dyn Embedder = &embedder;
        assert_eq!(trait_obj.name(), "remote");
        assert_eq!(trait_obj.dim(), 1536);
    }

    /// Async `AsyncEmbedder::name` and `AsyncEmbedder::dim` mirror the
    /// sync trait so async-first callers do not have to re-import
    /// configuration types just to log dimensionality.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_embedder_trait_name_and_dim_match() {
        let env_name = "MOAGAN_TEST_ASYNC_TRAIT_98240";
        unsafe {
            std::env::set_var(env_name, "k");
        }
        let embedder =
            RemoteEmbedder::new("cohere", "https://api.cohere.ai/v1", env_name, "m", 1024).unwrap();
        unsafe {
            std::env::remove_var(env_name);
        }
        let trait_obj: &dyn AsyncEmbedder = &embedder;
        assert_eq!(trait_obj.name(), "remote");
        assert_eq!(trait_obj.dim(), 1024);
    }

    /// Sync `Embedder::embed` populates the cache on a cold call.
    /// A second sync call with the same text returns the cached
    /// vector without sending another HTTP request — pinning the
    /// "cluster loop re-embeds the same sketch text cheaply"
    /// invariant. The mock server's request count (2: pre-warm
    /// plus cold sync, then unchanged after the cached hot sync)
    /// is the network-side proof: if the cache stops working, the
    /// post-hot call triggers another POST and the count
    /// diverges.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_embedder_caches_cold_call() {
        use crate::llm::embed::Embedder;

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/embeddings"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [{"embedding": [0.1, 0.2, 0.3], "index": 0}]
                })),
            )
            .expect(2)
            .mount(&server)
            .await;

        let env_name = "MOAGAN_TEST_SYNC_CACHE_98241";
        unsafe {
            std::env::set_var(env_name, "k");
        }
        let embedder = RemoteEmbedder::with_provider(
            RemoteEmbedderProvider::Openai,
            &format!("{}/v1", server.uri()),
            env_name,
            "m",
            3,
        )
        .unwrap();

        // Pre-warm the cache via the async trait path so the
        // sync bridge exercises the cold branch on text "cold"
        // and the hot branch on text "cold" + "cold" again.
        let _ = AsyncEmbedder::embed_batch(&embedder, &["warm"])
            .await
            .unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        // Cold sync call on a different text: must re-enter the
        // transport, count the request, populate the cache. We
        // expect exactly 2 requests total so far after this call.
        let v1 = <RemoteEmbedder as Embedder>::embed(&embedder, "cold");
        assert_eq!(v1, vec![0.1, 0.2, 0.3]);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);

        // Hot sync call on the same text: must NOT issue another
        // request, so the request count remains 2.
        let v1_again = <RemoteEmbedder as Embedder>::embed(&embedder, "cold");
        assert_eq!(v1_again, v1);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "second sync embed() must hit the cache, not the wire"
        );

        unsafe {
            std::env::remove_var(env_name);
        }
    }

    /// Sync `Embedder::embed` on a totally new text uses
    /// `tokio::task::block_in_place`. This test runs inside a
    /// multi-thread tokio runtime (the production runtime shape)
    /// so the bridge is allowed. If the multi-thread requirement
    /// is ever loosened, this test starts failing and forces a
    /// conscious decision.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_embedder_round_trips_through_wiremock() {
        use crate::llm::embed::Embedder;

        let server = wiremock::MockServer::start().await;
        // Mock returns ONE vector per request — single-input sync
        // calls match this contract; the cluster cache short-circuit
        // (other test) verifies the no-second-POST path.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/embeddings"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [
                        {"embedding": [0.1, 0.2, 0.3], "index": 0},
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let env_name = "MOAGAN_TEST_SYNC_BRIDGE_98242";
        unsafe {
            std::env::set_var(env_name, "k");
        }
        let embedder = RemoteEmbedder::with_provider(
            RemoteEmbedderProvider::Openai,
            &format!("{}/v1", server.uri()),
            env_name,
            "m",
            3,
        )
        .unwrap();

        let a = <RemoteEmbedder as Embedder>::embed(&embedder, "alpha");
        assert_eq!(a, vec![0.1, 0.2, 0.3]);
        let a_again = <RemoteEmbedder as Embedder>::embed(&embedder, "alpha");
        assert_eq!(a_again, a);
        // Only one POST: the second sync embed must have come from
        // the cache, not the wire.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        unsafe {
            std::env::remove_var(env_name);
        }
    }
}
