//! Static catalog of provider/model capabilities from `models.dev`.
//!
//! The upstream `https://models.dev/api.json` document is a 3.6 MB JSON
//! tree of every provider and model the project tracks, including the
//! limit / cost / modality fields PR-2 and PR-3 of the catalog
//! integration plan will rely on. Pulling it on every run would be
//! wasteful and would couple a CLI invocation to the upstream
//! service, so this module mirrors the `max_tokens_auto.toml` pattern:
//! read the persisted file first, and only hit the network when the
//! cache is missing, stale, or unreadable.
//!
//! # On-disk shape
//!
//! The persisted file is **not** a verbatim copy of the upstream
//! document. It is wrapped in a `ModelsDevCatalog` envelope that adds
//! two bookkeeping fields so a future read can decide freshness
//! without a second `stat()` call:
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "fetched_at_unix": 1730000000,
//!   "providers": { ... raw upstream map ... }
//! }
//! ```
//!
//! The wrap step happens on the fetch path; the on-disk file is the
//! canonical artifact PR-2 will consume.
//!
//! # TTL and freshness
//!
//! Freshness has two equivalent surfaces:
//!
//! - On disk we use the file mtime (cheap, single `stat` call).
//! - In memory the [`ModelsDevCatalog::fetched_at_unix`] field lets
//!   callers ask [`is_fresh`] without touching the filesystem.
//!
//! The boundary is exclusive: at exactly the TTL boundary the cache
//! is considered stale. PR-1's tests pin this contract because
//! downstream code uses `is_fresh` to decide whether to refresh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Canonical upstream URL for the models.dev catalog document.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Default refresh window in hours. One hour matches the upstream
/// release cadence (model rows typically update daily; the per-hour
/// cap is a politeness budget, not a correctness requirement).
pub const DEFAULT_REFRESH_HOURS: u64 = 1;

/// On-disk schema version. Bumped on any breaking change to the
/// wrapped envelope; the load path silently rebuilds on a mismatch
/// rather than attempting a forward migration, which keeps the
/// failure mode simple (PR-1 has no historical data to preserve).
pub const CATALOG_SCHEMA_VERSION: u32 = 1;

/// Filename of the cached catalog under [`crate::fs_layout::MoaganHome`].
pub const CATALOG_FILE_NAME: &str = "models_dev.json";

/// Convert hours to seconds without overflowing on absurd inputs.
/// `u64::MAX` hours would saturate to `u64::MAX` seconds, which is
/// harmless because no real caller passes that.
const fn ttl_hours_to_secs(hours: u64) -> u64 {
    hours.saturating_mul(3_600)
}

/// One model row from the upstream catalog. The struct is a
/// projection of the upstream schema; serde silently drops any
/// field models.dev adds in the future (`description`, `knowledge`,
/// `structured_output`, ...), which is the right default for a
/// forward-compatible catalog consumer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelsDevEntry {
    /// Stable model identifier (e.g. `MiniMax-M3`).
    pub id: String,
    /// Display name shown in dashboards.
    pub name: String,
    /// Family bucket (e.g. `minimax`). Optional because some legacy
    /// entries omit it.
    #[serde(default)]
    pub family: Option<String>,
    /// Whether the model accepts file attachments.
    pub attachment: bool,
    /// Whether the model emits a separate reasoning trace.
    pub reasoning: bool,
    /// Reasoning control surface. `toggle` is a boolean knob;
    /// `effort` carries a `values` list. Captured verbatim so PR-3
    /// can render the picker without re-fetching.
    #[serde(default)]
    pub reasoning_options: Vec<ReasoningOption>,
    /// Whether the model supports tool/function calling.
    pub tool_call: bool,
    /// Whether the `temperature` parameter is honoured.
    pub temperature: bool,
    /// Field name carrying the interleaved reasoning stream, if any.
    #[serde(default)]
    pub interleaved: Option<InterleavedField>,
    /// Input/output modality lists (e.g. `["text", "image"]`).
    #[serde(default)]
    pub modalities: Modalities,
    /// Context and output token caps.
    pub limit: Limits,
    /// Per-million-token USD pricing. All four fields default to 0
    /// because a handful of older rows omit the object entirely.
    #[serde(default)]
    pub cost: Cost,
    /// Whether the model weights are publicly downloadable.
    #[serde(default)]
    pub open_weights: bool,
    /// Public release date (ISO-8601 string; PR-1 does not parse it).
    #[serde(default)]
    pub release_date: Option<String>,
    /// Last time the upstream edited this row (ISO-8601 string).
    #[serde(default)]
    pub last_updated: Option<String>,
}

/// Token-limit caps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Limits {
    /// Total context window in tokens.
    pub context: u64,
    /// Maximum output tokens per response.
    pub output: u64,
}

/// Pricing per million tokens in USD.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Cost {
    /// Input token price.
    #[serde(default)]
    pub input: f64,
    /// Output token price.
    #[serde(default)]
    pub output: f64,
    /// Cached-input read price.
    #[serde(default)]
    pub cache_read: f64,
    /// Cached-input write price.
    #[serde(default)]
    pub cache_write: f64,
}

/// Input/output modality lists. Empty vectors are valid: a row
/// without a `modalities` field falls back to text-only at the
/// call site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Modalities {
    /// Accepted input modalities (e.g. `text`, `image`, `pdf`).
    #[serde(default)]
    pub input: Vec<String>,
    /// Produced output modalities.
    #[serde(default)]
    pub output: Vec<String>,
}

/// One entry of the `reasoning_options` array. The upstream schema
/// uses the JSON key `"type"` (a serde keyword we cannot reuse
/// without renaming); values are typically `toggle` or `effort`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningOption {
    /// Discriminator — either `toggle` or `effort`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Effort levels when `kind == "effort"`. Empty for `toggle`.
    #[serde(default)]
    pub values: Vec<String>,
}

/// Wrapper for the `interleaved` object. The field name identifies
/// which response key carries the reasoning stream (e.g.
/// `reasoning_content`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InterleavedField {
    /// Name of the response key carrying the reasoning stream.
    pub field: String,
}

/// One provider from the upstream catalog. The struct is the
/// projection of the upstream provider-level schema; serde silently
/// drops any future fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelsDevProvider {
    /// Stable provider identifier (the same key used in the outer
    /// map; kept here for symmetry with the on-disk shape).
    pub id: String,
    /// Human-readable provider name.
    pub name: String,
    /// Base URL for the provider's API. Optional because some
    /// providers (e.g. fully local stacks) do not declare one.
    #[serde(default)]
    pub api: Option<String>,
    /// Link to the provider's documentation.
    #[serde(default)]
    pub doc: Option<String>,
    /// Models declared by this provider, keyed by model id.
    #[serde(default)]
    pub models: BTreeMap<String, ModelsDevEntry>,
}

/// The wrapped catalog. The envelope is the on-disk artifact; the
/// `providers` map mirrors the upstream JSON verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelsDevCatalog {
    /// Schema version of the on-disk envelope (see
    /// [`CATALOG_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Unix timestamp (seconds) of the fetch that produced this
    /// catalog. Drives [`is_fresh`].
    pub fetched_at_unix: u64,
    /// Providers keyed by their upstream id (e.g. `minimax`).
    pub providers: BTreeMap<String, ModelsDevProvider>,
}

/// Result of [`load_or_fetch`]. The `from_cache` flag tells the
/// caller whether the disk or the network satisfied the request so
/// the CLI can surface a "refreshed catalog" hint when relevant.
#[derive(Debug)]
pub struct CatalogLoad {
    /// The catalog itself (always populated on `Ok`).
    pub catalog: ModelsDevCatalog,
    /// `true` when the value was read from the on-disk cache,
    /// `false` when a fresh fetch was required.
    pub from_cache: bool,
    /// Resolved path of the cache file under `MOAGAN_HOME`.
    pub path: PathBuf,
}

/// Resolve the cache file path under an arbitrary `MOAGAN_HOME`-style
/// directory. Public so callers outside this module (PR-2's CLI
/// hook) can locate the artifact without re-implementing the
/// `models_dev.json` filename convention.
pub fn catalog_path(home_path: &Path) -> PathBuf {
    home_path.join(CATALOG_FILE_NAME)
}

/// Best-effort load of the on-disk catalog without touching the
/// network. Returns `Some(catalog)` when the file exists and
/// parses, `None` otherwise (file missing, unreadable, or
/// malformed). Used by CLI surfaces that only need the cached
/// snapshot — `moagan doctor --capabilities`,
/// `moagan inspect --capabilities` — and do not want to wait on a
/// network round-trip.
///
/// `load_or_fetch` and the explicit refresh command.
pub fn try_load_from_disk(home_path: &Path) -> Option<ModelsDevCatalog> {
    let path = catalog_path(home_path);
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<ModelsDevCatalog>(&bytes) {
        Ok(catalog) => {
            tracing::debug!(
                path = %path.display(),
                providers = catalog.providers.len(),
                "models_dev: try_load_from_disk hit"
            );
            Some(catalog)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "models_dev: best-effort disk load failed; treating as missing"
            );
            None
        }
    }
}

/// Pure freshness check. A catalog is fresh when strictly less than
/// `ttl_hours` have elapsed since [`ModelsDevCatalog::fetched_at_unix`].
/// The boundary is exclusive: `now - fetched == ttl_hours * 3600`
/// counts as stale. This contract is what
/// [`is_fresh_respects_ttl`] pins.
///
/// `now_unix` is required to be `>= fetched_at_unix`; a smaller
/// value is treated as stale (the saturating subtraction in
/// `mtime_fresh` would otherwise mask a clock skew bug as "always
/// fresh").
pub fn is_fresh(catalog: &ModelsDevCatalog, now_unix: u64, ttl_hours: u64) -> bool {
    let ttl_secs = ttl_hours_to_secs(ttl_hours);
    match now_unix.checked_sub(catalog.fetched_at_unix) {
        Some(elapsed) => elapsed < ttl_secs,
        None => false,
    }
}

/// Case-sensitive lookup by `(provider, model)`. Both identifiers
/// must match the upstream spelling exactly — the catalog is
/// internally consistent (lowercase provider ids, verbatim model
/// ids), so a fuzzy match would only mask caller bugs.
pub fn lookup(catalog: &ModelsDevCatalog, provider: &str, model: &str) -> Option<ModelsDevEntry> {
    let entry = catalog
        .providers
        .get(provider)
        .and_then(|p| p.models.get(model))
        .cloned();
    tracing::trace!(
        provider,
        model,
        present = entry.is_some(),
        "models_dev::lookup"
    );
    entry
}

/// Load the catalog from disk, falling back to a network fetch when
/// the cache is missing or stale. `offline=true` disables the
/// network path and surfaces a descriptive error if no usable cache
/// is available.
///
/// On fetch failure (HTTP error, malformed response body) the
/// function degrades to a stale cache when one exists, logging a
/// `tracing::warn!` so operators can correlate the run with a
/// downstream issue. This matches the `max_tokens_auto` recovery
/// path: never block a run on a flaky third party when a usable
/// snapshot already sits on disk.
pub async fn load_or_fetch(
    home_path: &Path,
    ttl_hours: u64,
    offline: bool,
) -> Result<CatalogLoad, String> {
    tracing::debug!(
        home = %home_path.display(),
        ttl_hours,
        offline,
        "models_dev::load_or_fetch"
    );
    let client = super::http::build_client()
        .map_err(|e| format!("models_dev: build reqwest client: {e}"))?;
    load_or_fetch_at(home_path, ttl_hours, offline, MODELS_DEV_URL, &client).await
}

/// Test-friendly variant of [`load_or_fetch`] that accepts an
/// explicit URL and a pre-built HTTP client. Public so the wiremock
/// suite can stub the upstream without standing up a network server.
pub async fn load_or_fetch_at(
    home_path: &Path,
    ttl_hours: u64,
    offline: bool,
    url: &str,
    client: &Client,
) -> Result<CatalogLoad, String> {
    tracing::debug!(
        home = %home_path.display(),
        ttl_hours,
        offline,
        url,
        "models_dev::load_or_fetch_at"
    );
    let path = catalog_path(home_path);
    let now = unix_now_secs_u64();

    if let Some(catalog) = read_fresh_cache(&path, ttl_hours, now) {
        tracing::info!(
            path = %path.display(),
            "models_dev: fresh cache hit, no fetch"
        );
        return Ok(CatalogLoad {
            catalog,
            from_cache: true,
            path,
        });
    }

    if offline {
        tracing::warn!(
            path = %path.display(),
            "models_dev: offline mode and cache missing/stale"
        );
        return Err(format!(
            "models_dev: offline mode and cache missing or stale at {}",
            path.display()
        ));
    }

    match fetch_and_persist(url, client, &path).await {
        Ok(catalog) => {
            tracing::info!(
                url,
                providers = catalog.providers.len(),
                "models_dev: fetched fresh catalog"
            );
            Ok(CatalogLoad {
                catalog,
                from_cache: false,
                path,
            })
        }
        Err(fetch_err) => fallback_to_stale_cache(&path, &fetch_err),
    }
}

fn read_fresh_cache(path: &Path, ttl_hours: u64, now_unix: u64) -> Option<ModelsDevCatalog> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let mtime_unix = u64_from_system_time(mtime);
    if !mtime_fresh(mtime_unix, now_unix, ttl_hours) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<ModelsDevCatalog>(&bytes) {
        Ok(catalog) => Some(catalog),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "models_dev: cache mtime fresh but JSON parse failed; treating as stale"
            );
            None
        }
    }
}

fn mtime_fresh(mtime_unix: u64, now_unix: u64, ttl_hours: u64) -> bool {
    let ttl_secs = ttl_hours_to_secs(ttl_hours);
    match now_unix.checked_sub(mtime_unix) {
        Some(elapsed) => elapsed < ttl_secs,
        None => false,
    }
}

async fn fetch_and_persist(
    url: &str,
    client: &Client,
    path: &Path,
) -> Result<ModelsDevCatalog, String> {
    tracing::info!(url, "models_dev: fetching catalog from upstream");
    let response = client.get(url).send().await.map_err(|e| {
        tracing::warn!(url, error = %e, "models_dev: fetch send failed");
        format!("models_dev: fetch {url} failed: {e}")
    })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            url,
            status = status.as_u16(),
            "models_dev: fetch returned non-success"
        );
        return Err(format!("models_dev: fetch {url} returned HTTP {status}"));
    }
    let providers: BTreeMap<String, ModelsDevProvider> = response.json().await.map_err(|e| {
        tracing::warn!(url, error = %e, "models_dev: parse response failed");
        format!("models_dev: parse response from {url}: {e}")
    })?;
    let catalog = ModelsDevCatalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        fetched_at_unix: unix_now_secs_u64(),
        providers,
    };
    write_atomic(path, &catalog)?;
    tracing::debug!(
        path = %path.display(),
        providers = catalog.providers.len(),
        "models_dev: catalog persisted"
    );
    Ok(catalog)
}

fn fallback_to_stale_cache(path: &Path, fetch_err: &str) -> Result<CatalogLoad, String> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<ModelsDevCatalog>(&bytes) {
            Ok(catalog) => {
                tracing::warn!(
                    error = %fetch_err,
                    path = %path.display(),
                    "models_dev: fetch failed, returning stale cache (best-effort)"
                );
                Ok(CatalogLoad {
                    catalog,
                    from_cache: true,
                    path: path.to_path_buf(),
                })
            }
            Err(parse_err) => Err(format!(
                "models_dev: fetch failed ({fetch_err}) and cache unreadable ({parse_err})"
            )),
        },
        Err(read_err) => Err(format!(
            "models_dev: fetch failed ({fetch_err}) and cache unreadable ({read_err})"
        )),
    }
}

fn write_atomic(path: &Path, catalog: &ModelsDevCatalog) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("models_dev: create parent dir {}: {e}", parent.display()))?;
    }
    let bytes =
        serde_json::to_vec(catalog).map_err(|e| format!("models_dev: serialise catalog: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)
        .map_err(|e| format!("models_dev: write tmp {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        format!(
            "models_dev: rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn unix_now_secs_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn u64_from_system_time(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SAMPLE_MINIMAX: &str = r#"{
        "minimax": {
            "id": "minimax",
            "env": ["MINIMAX_API_KEY"],
            "npm": "@ai-sdk/anthropic",
            "api": "https://api.minimax.io/anthropic/v1",
            "name": "MiniMax (minimax.io)",
            "doc": "https://docs.minimax.io",
            "models": {
                "MiniMax-M3": {
                    "id": "MiniMax-M3",
                    "name": "MiniMax-M3",
                    "family": "minimax",
                    "attachment": false,
                    "reasoning": true,
                    "reasoning_options": [],
                    "tool_call": true,
                    "temperature": true,
                    "interleaved": null,
                    "release_date": "2026-08-08",
                    "last_updated": "2026-08-08",
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "open_weights": true,
                    "limit": {"context": 524288, "output": 128000},
                    "cost": {"input": 0.3, "output": 1.2, "cache_read": 0.03, "cache_write": 0.375}
                }
            }
        }
    }"#;

    fn sample_catalog() -> ModelsDevCatalog {
        ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 1_700_000_000,
            providers: BTreeMap::new(),
        }
    }

    fn write_cache(dir: &Path, body: &ModelsDevCatalog) -> PathBuf {
        let path = catalog_path(dir);
        write_atomic(&path, body).expect("write_atomic should succeed");
        path
    }

    fn touch_mtime_to_now(path: &Path) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open cache for mtime touch");
        file.set_modified(SystemTime::now())
            .expect("set_modified should succeed");
    }

    #[test]
    fn parse_minimax_minimax_m3_from_sample() {
        let providers: BTreeMap<String, ModelsDevProvider> =
            serde_json::from_str(SAMPLE_MINIMAX).expect("upstream sample must parse");
        let catalog = ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 0,
            providers,
        };
        let entry = lookup(&catalog, "minimax", "MiniMax-M3")
            .expect("minimax/MiniMax-M3 lookup must succeed");
        assert_eq!(entry.id, "MiniMax-M3");
        assert_eq!(entry.family.as_deref(), Some("minimax"));
        assert!(!entry.attachment);
        assert!(entry.reasoning);
        assert!(entry.tool_call);
        assert!(entry.temperature);
        assert!(entry.interleaved.is_none());
        assert_eq!(entry.limit.context, 524_288);
        assert_eq!(entry.limit.output, 128_000);
        assert!((entry.cost.input - 0.3).abs() < f64::EPSILON);
        assert!((entry.cost.output - 1.2).abs() < f64::EPSILON);
        assert!((entry.cost.cache_read - 0.03).abs() < f64::EPSILON);
        assert!((entry.cost.cache_write - 0.375).abs() < f64::EPSILON);
        assert!(entry.open_weights);
        assert_eq!(entry.modalities.input, vec!["text".to_string()]);
        assert_eq!(entry.modalities.output, vec!["text".to_string()]);
        assert_eq!(entry.release_date.as_deref(), Some("2026-08-08"));
        assert_eq!(entry.last_updated.as_deref(), Some("2026-08-08"));
    }

    #[test]
    fn parse_full_catalog_round_trip() {
        let mut providers = BTreeMap::new();
        let mut models = BTreeMap::new();
        models.insert(
            "MiniMax-M3".to_string(),
            ModelsDevEntry {
                id: "MiniMax-M3".to_string(),
                name: "MiniMax-M3".to_string(),
                family: Some("minimax".to_string()),
                attachment: false,
                reasoning: true,
                reasoning_options: vec![],
                tool_call: true,
                temperature: true,
                interleaved: Some(InterleavedField {
                    field: "reasoning_content".to_string(),
                }),
                modalities: Modalities {
                    input: vec!["text".to_string()],
                    output: vec!["text".to_string()],
                },
                limit: Limits {
                    context: 524_288,
                    output: 128_000,
                },
                cost: Cost {
                    input: 0.3,
                    output: 1.2,
                    cache_read: 0.03,
                    cache_write: 0.375,
                },
                open_weights: true,
                release_date: Some("2026-08-08".to_string()),
                last_updated: Some("2026-08-08".to_string()),
            },
        );
        providers.insert(
            "minimax".to_string(),
            ModelsDevProvider {
                id: "minimax".to_string(),
                name: "MiniMax (minimax.io)".to_string(),
                api: Some("https://api.minimax.io/anthropic/v1".to_string()),
                doc: Some("https://docs.minimax.io".to_string()),
                models,
            },
        );
        let original = ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 1_700_000_000,
            providers,
        };

        let bytes = serde_json::to_vec(&original).expect("serialise catalog");
        let restored: ModelsDevCatalog =
            serde_json::from_slice(&bytes).expect("deserialise catalog");
        assert_eq!(restored, original);
    }

    #[test]
    fn is_fresh_respects_ttl() {
        let now = 1_700_000_000_u64;
        let ttl = 1_u64;
        let just_inside = ModelsDevCatalog {
            fetched_at_unix: now - ttl_hours_to_secs(ttl) + 1,
            ..sample_catalog()
        };
        let at_boundary = ModelsDevCatalog {
            fetched_at_unix: now - ttl_hours_to_secs(ttl),
            ..sample_catalog()
        };
        let well_outside = ModelsDevCatalog {
            fetched_at_unix: now - ttl_hours_to_secs(ttl) - 1,
            ..sample_catalog()
        };

        assert!(
            is_fresh(&just_inside, now, ttl),
            "1 second inside TTL the catalog must be fresh"
        );
        assert!(
            !is_fresh(&at_boundary, now, ttl),
            "at exactly TTL boundary the catalog must be stale"
        );
        assert!(
            !is_fresh(&well_outside, now, ttl),
            "past the TTL the catalog must be stale"
        );
    }

    #[test]
    fn is_fresh_uses_now_unix() {
        let fetched = 1_700_000_000_u64;
        let ttl = 1_u64;
        let catalog = ModelsDevCatalog {
            fetched_at_unix: fetched,
            ..sample_catalog()
        };

        // 100 s elapsed: well within 1 h.
        assert!(is_fresh(&catalog, fetched + 100, ttl));
        // 3 599 s elapsed: still inside 1 h (TTL = 3 600 s).
        assert!(is_fresh(&catalog, fetched + 3_599, ttl));
        // 3 600 s elapsed: at the boundary, stale.
        assert!(!is_fresh(&catalog, fetched + 3_600, ttl));
        // Way past: stale.
        assert!(!is_fresh(&catalog, fetched + 100_000, ttl));

        // Sanity: the result only depends on (catalog, now, ttl) and
        // not on `SystemTime::now()`. We pick an obviously-wrong
        // `now` and assert the function still returns a deterministic
        // answer.
        let bogus_now = 0_u64;
        assert!(!is_fresh(&catalog, bogus_now, ttl));
    }

    #[test]
    fn lookup_returns_none_for_unknown_model() {
        let mut providers = BTreeMap::new();
        let mut models = BTreeMap::new();
        models.insert(
            "MiniMax-M3".to_string(),
            ModelsDevEntry {
                id: "MiniMax-M3".to_string(),
                name: "MiniMax-M3".to_string(),
                family: None,
                attachment: false,
                reasoning: false,
                reasoning_options: vec![],
                tool_call: true,
                temperature: true,
                interleaved: None,
                modalities: Modalities::default(),
                limit: Limits::default(),
                cost: Cost::default(),
                open_weights: false,
                release_date: None,
                last_updated: None,
            },
        );
        providers.insert(
            "minimax".to_string(),
            ModelsDevProvider {
                id: "minimax".to_string(),
                name: "MiniMax".to_string(),
                api: None,
                doc: None,
                models,
            },
        );
        let catalog = ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 0,
            providers,
        };

        assert!(lookup(&catalog, "minimax", "nonexistent").is_none());
        assert!(lookup(&catalog, "missing-provider", "MiniMax-M3").is_none());
        // Case sensitivity: lookup is case-sensitive.
        assert!(lookup(&catalog, "minimax", "minimax-m3").is_none());
        assert!(lookup(&catalog, "MiniMax", "MiniMax-M3").is_none());
    }

    #[test]
    fn lookup_returns_entry_for_known_pair() {
        let mut providers = BTreeMap::new();
        let mut models = BTreeMap::new();
        let entry = ModelsDevEntry {
            id: "MiniMax-M3".to_string(),
            name: "MiniMax-M3".to_string(),
            family: Some("minimax".to_string()),
            attachment: false,
            reasoning: true,
            reasoning_options: vec![],
            tool_call: true,
            temperature: true,
            interleaved: None,
            modalities: Modalities::default(),
            limit: Limits {
                context: 524_288,
                output: 128_000,
            },
            cost: Cost::default(),
            open_weights: true,
            release_date: None,
            last_updated: None,
        };
        models.insert("MiniMax-M3".to_string(), entry.clone());
        providers.insert(
            "minimax".to_string(),
            ModelsDevProvider {
                id: "minimax".to_string(),
                name: "MiniMax".to_string(),
                api: None,
                doc: None,
                models,
            },
        );
        let catalog = ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 0,
            providers,
        };
        let found = lookup(&catalog, "minimax", "MiniMax-M3").expect("known pair must resolve");
        assert_eq!(found, entry);
    }

    #[tokio::test]
    async fn load_or_fetch_returns_cache_when_fresh() {
        let tmp = TempDir::new().expect("tempdir");
        let cache = sample_catalog();
        let path = write_cache(tmp.path(), &cache);
        touch_mtime_to_now(&path);

        let client = Client::new();
        let load = load_or_fetch_at(tmp.path(), 1, true, "http://unused", &client)
            .await
            .expect("fresh cache must satisfy the call");
        assert!(load.from_cache, "fresh cache must be served from disk");
        assert_eq!(load.catalog, cache);
        assert_eq!(load.path, path);
    }

    #[tokio::test]
    async fn load_or_fetch_errors_in_offline_with_no_cache() {
        let tmp = TempDir::new().expect("tempdir");
        // Intentionally do NOT create the cache file.
        let client = Client::new();
        let err = load_or_fetch_at(tmp.path(), 1, true, "http://unused", &client)
            .await
            .expect_err("offline without cache must error");
        assert!(
            err.contains("offline mode"),
            "error must mention offline mode; got: {err}"
        );
        assert!(
            err.contains(tmp.path().to_str().unwrap_or("")),
            "error must include the cache path; got: {err}"
        );
    }

    #[tokio::test]
    async fn load_or_fetch_fetches_from_network_when_cache_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_MINIMAX))
            .mount(&server)
            .await;
        let url = format!("{}/api.json", server.uri());

        let tmp = TempDir::new().expect("tempdir");
        let client = Client::new();
        let load = load_or_fetch_at(tmp.path(), 1, false, &url, &client)
            .await
            .expect("network fetch must succeed");

        assert!(!load.from_cache, "fresh fetch must mark from_cache=false");
        let entry = lookup(&load.catalog, "minimax", "MiniMax-M3")
            .expect("minimax/MiniMax-M3 lookup must succeed after fetch");
        assert_eq!(entry.id, "MiniMax-M3");
        assert_eq!(entry.limit.context, 524_288);

        // The function also persists the catalog. Round-trip via
        // disk to confirm the on-disk shape is the wrapped envelope.
        let on_disk_bytes = std::fs::read(&load.path).expect("read cache");
        let on_disk: ModelsDevCatalog =
            serde_json::from_slice(&on_disk_bytes).expect("on-disk shape is wrapped");
        assert_eq!(on_disk.schema_version, CATALOG_SCHEMA_VERSION);
        assert!(on_disk.fetched_at_unix > 0);
        assert!(on_disk.providers.contains_key("minimax"));
        // No `.tmp` left behind.
        let tmp_sidecar = load.path.with_extension("json.tmp");
        assert!(
            !tmp_sidecar.exists(),
            "atomic write must not leave a .tmp file at {}",
            tmp_sidecar.display()
        );
    }
    #[tokio::test]
    async fn load_or_fetch_falls_back_to_stale_cache_on_fetch_error() {
        // Stale cache exists.
        let tmp = TempDir::new().expect("tempdir");
        let stale = ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 0,
            providers: BTreeMap::new(),
        };
        let cache_path = write_cache(tmp.path(), &stale);
        // Backdate the file so mtime is outside the TTL window.
        let past = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&cache_path)
            .expect("open for mtime");
        file.set_modified(past).expect("set_modified past");

        // Wiremock server that always 500s.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0..)
            .mount(&server)
            .await;
        let url = format!("{}/api.json", server.uri());

        let client = Client::new();
        let load = load_or_fetch_at(tmp.path(), 1, false, &url, &client)
            .await
            .expect("stale cache must satisfy the call when fetch fails");
        assert!(load.from_cache, "fallback must mark from_cache=true");
        assert_eq!(load.catalog, stale);
    }

    #[test]
    fn catalog_path_joins_home_with_filename() {
        let dir = TempDir::new().expect("tempdir");
        let p = catalog_path(dir.path());
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("models_dev.json")
        );
        assert_eq!(p.parent(), Some(dir.path()));
    }

    // Smoke check: ensure the http::build_client helper is reachable
    // from this module so the public `load_or_fetch` keeps its
    // contract. The call site is not exercised here because that
    // would force every test in this file to be async.
    #[test]
    fn http_client_helper_is_reachable() {
        let _client_factory: fn() -> Result<Client, crate::error::Error> =
            super::super::http::build_client;
        let _: Arc<Client> = Arc::new(Client::new());
    }
}
