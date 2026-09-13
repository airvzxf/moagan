//! Auto-detection of supported sampling `top_k` values per
//! `(provider, model)` via runtime probes.
//!
//! Mirrors [`crate::llm::temperature_probe`] and
//! [`crate::llm::top_p_probe`] field-by-field — same algorithm
//! shape, same sidecar layout, same `Arc<RwLock>` concurrency
//! contract. The probe tests 11 candidate `top_k` values per
//! `(provider, model)`, spanning `1` (greedy decoding) through
//! `1024` in powers of 2. Powers of 2 match the typical upstream
//! quantisation and keep the candidate set small. The canonical
//! constant lives at [`TOP_K_PROBE_VALUES`].
//!
//! ## Why a separate module
//!
//! Provider APIs disagree on which `top_k` values they accept:
//! Anthropic-compat endpoints accept `top_k ∈ [1, ∞)` for the
//! M-series, OpenAI-compat endpoints reject `top_k` outright (the
//! field is omitted from the wire via the self-healing
//! `param_rejections` path), and a handful of relays cap `top_k`
//! at small values (OpenAI Responses relays cap at 40; Anthropic
//! models accept anything but the runtime's documented behaviour
//! is `top_k = 40`).
//!
//! [`TopKTable`] auto-discovers the set of accepted `top_k`
//! values for each `(provider, model)` once at startup, caches it
//! in a TOML sidecar at `<MOAGAN_HOME>/top_k_auto.toml`, and
//! exposes:
//!
//! - [`TopKTable::get`] — the cached smallest accepted `top_k`
//!   value (a smaller `top_k` is strictly more deterministic
//!   than a larger one and never less correct).
//! - [`TopKTable::nearest_supported`] — snaps a user-requested
//!   `top_k` to the discovered value.
//!
//! ## The algorithm
//!
//! The probe tests 11 candidates per `(provider, model)`. Each
//! candidate is tried in isolation with a tiny deterministic
//! payload and classified by HTTP status plus body fingerprint
//! into three outcomes (`Accepted` / `Rejected` / `Indeterminate`).
//!
//! ## When it runs
//!
//! Two complementary entry points:
//!
//! - **Runtime auto-probe** — `ProviderRegistry` schedules one
//!   background probe per fresh `(provider, model)` the first time
//!   the registry sees it. The probe writes through to
//!   `<MOAGAN_HOME>/top_k_auto.toml` so the next startup picks
//!   the cached value up without re-running.
//! - **Operator-driven probe** — `moagan probe top_k --provider
//!   PROVIDER:MODEL [--persist-cap] [--dry-run]`. (Lands in
//!   issue #931.)
//!
//! ## When to disable it
//!
//! Disable the auto-probe when the cost of an 11-shot HTTP sweep
//! is prohibitive or when the provider cannot be reached from the
//! test runner. The same `MOAGAN_TOP_K_AUTO=false` knob applies
//! (and is a hard kill switch; `None` / `0` / `false` / `off` all
//! collapse to "off"). For per-provider opt-out, set
//! `[[providers.<name>]] top_k_auto_enabled = false` in
//! `config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex as ParkingMutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::llm::client::LlmClient;
use crate::llm::client::LlmRequest;
use crate::llm::role::Role;
use crate::llm::wire::Response;

/// Post-process the body emitted by `toml::to_string_pretty` so
/// every key under the `[providers.<name>.<model>]` headers is
/// double-quoted. Duplicated from the sibling probe modules to
/// keep each writer self-contained.
fn quote_provider_model_keys(body: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\[providers\.([A-Za-z0-9_-]+)\.([A-Za-z0-9_-]+)\]")
            .expect("quote_provider_model_keys regex compiles")
    });
    re.replace_all(body, |caps: &regex::Captures<'_>| {
        format!("[providers.\"{}\".\"{}\"]", &caps[1], &caps[2])
    })
    .into_owned()
}

/// 11 candidate `top_k` values probed per `(provider, model)`.
/// Powers of 2 spanning `1` (greedy decoding) through `1024`.
/// Powers of 2 match the typical upstream quantisation and keep
/// the candidate set small.
pub const TOP_K_PROBE_VALUES: &[u32] = &[1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];
const _: () = assert!(TOP_K_PROBE_VALUES.len() == 11);

/// Maximum number of probes in flight at once. With 11 candidates
/// and a batch size of 3 the runtime runs 4 batches (the last one
/// has 2 candidates). The value matches the temperature /
/// top-p probe batch size so the three auto-probes share the same
/// fan-out semantics.
pub const TOP_K_PROBE_BATCH_SIZE: usize = 3;

/// HTTP timeout for a single probe. Mirrors the temperature /
/// top-p probe timeout.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Minimum number of output tokens the probe request asks for.
const PROBE_MIN_OUTPUT_TOKENS: u32 = 1024;

/// Probe request body.
pub const TOP_K_PROBE_USER: &str = "Reply with the single character: 1";

/// Empty system prompt for the probe.
pub const TOP_K_PROBE_SYSTEM: &str = "";

/// Outcome of a single top-k probe HTTP call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopKProbeOutcome {
    /// Provider accepted the `top_k`.
    Accepted,
    /// Provider rejected the `top_k`.
    Rejected,
    /// Provider errored out for a reason other than `top_k`, or
    /// returned a shape the classifier cannot commit on.
    Indeterminate,
}

/// Reduced view of an upstream [`Response`] consumed by
/// [`classify_probe_response`].
#[derive(Debug, Clone, Copy)]
pub struct ProbeResponseView<'a> {
    /// Joined text from the response body.
    pub text: &'a str,
    /// The `stop_reason` reported by the upstream
    /// (`"end_turn"`, `"max_tokens"`, etc.). `None` when the
    /// upstream omitted the field.
    pub finish_reason: Option<&'a str>,
    /// Convenience flag the wire decoder sets when
    /// `finish_reason == "max_tokens"`.
    pub truncated: bool,
    /// Number of output tokens the upstream billed for the
    /// request. Combined with `truncated`, this is the
    /// unambiguous "ran out of budget mid-emit" signal.
    pub output_tokens: u64,
}

impl<'a> ProbeResponseView<'a> {
    /// Build a view from a borrowed [`Response`].
    pub fn from_response(resp: &'a Response) -> Self {
        Self {
            text: resp.text.as_str(),
            finish_reason: resp.finish_reason.as_deref(),
            truncated: resp.truncated,
            output_tokens: resp.usage.output_tokens,
        }
    }
}

/// Trait that the top-k probe uses to send its tiny request.
#[async_trait]
pub trait TopKProbeTransport: Send + Sync {
    /// Send a probe with the supplied `top_k` and report whether
    /// the upstream accepted it.
    async fn probe_send_top_k(&self, top_k: u32) -> TopKProbeOutcome;

    /// Optional shared counter for the parallel fan-out iteration
    /// index. Mirrors
    /// [`crate::llm::top_p_probe::TopPProbeTransport::iteration_counter`].
    fn iteration_counter(&self) -> Option<Arc<AtomicU32>> {
        None
    }
}

/// Default transport: wraps an existing [`LlmClient`] and fires a
/// probe against it. The probe deliberately bypasses the breaker
/// (no `BreakeredClient` wrapping) so a 400 rejection does not
/// count against the circuit-breaker window.
pub struct LlmClientTopKProbeTransport {
    client: Arc<dyn LlmClient>,
    /// Optional shared iteration counter. Lazy-allocated on the
    /// first [`Self::iteration_counter`] call so `new()` stays a
    /// pure move of the client `Arc`.
    iteration_counter_slot: parking_lot::Mutex<Option<Arc<AtomicU32>>>,
}

impl LlmClientTopKProbeTransport {
    /// Build a transport from a client.
    pub fn new(client: Arc<dyn LlmClient>) -> Result<Self> {
        Ok(Self {
            client,
            iteration_counter_slot: parking_lot::Mutex::new(None),
        })
    }

    /// Borrow the underlying client.
    pub fn client(&self) -> &Arc<dyn LlmClient> {
        &self.client
    }
}

#[async_trait]
impl TopKProbeTransport for LlmClientTopKProbeTransport {
    async fn probe_send_top_k(&self, top_k: u32) -> TopKProbeOutcome {
        use tracing::Instrument;
        let req = LlmRequest {
            role: Role::Sketch, // F1: see investigation report
            model: self.client.model().to_owned(),
            system: TOP_K_PROBE_SYSTEM.to_owned(),
            user: TOP_K_PROBE_USER.to_owned(),
            max_tokens: Some(PROBE_MIN_OUTPUT_TOKENS),
            temperature: None,
            top_p: None,
            top_k: Some(top_k),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let probe_span = tracing::info_span!(
            "llm_probe",
            probe_kind = "top_k",
            candidate = top_k,
            provider = %self.client.name(),
            model = %self.client.model(),
        );
        let res = timeout(
            PROBE_TIMEOUT,
            self.client.send_probe(&req).instrument(probe_span.clone()),
        )
        .await;

        let outcome_str: &'static str = match &res {
            Ok(Ok(resp)) => {
                let (status, body): (u16, Response) = resp.into();
                outcome_str_for_probe_response(status, ProbeResponseView::from_response(&body))
            }
            _ => "indeterminate",
        };

        let counter = self.iteration_counter();
        let iter = counter
            .as_ref()
            .map(|c| c.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        if crate::telemetry::stdout_events::resolve_event_format(
            crate::telemetry::stdout_events::EventFormat::Jsonl,
        ) {
            let event = build_probe_event(
                self.client.name(),
                self.client.model(),
                top_k,
                iter,
                outcome_str,
                crate::telemetry::stdout_events::now_rfc3339(),
            );
            crate::telemetry::stdout_events::STDOUT_EVENTS.emit(event);
        }

        match res {
            Ok(Ok(resp)) => {
                let (status, body): (u16, Response) = resp.into();
                classify_probe_response(status, ProbeResponseView::from_response(&body))
            }
            Ok(Err(_)) | Err(_) => TopKProbeOutcome::Indeterminate,
        }
    }

    fn iteration_counter(&self) -> Option<Arc<AtomicU32>> {
        let mut guard = self.iteration_counter_slot.lock();
        if let Some(c) = guard.as_ref() {
            return Some(c.clone());
        }
        let c = Arc::new(AtomicU32::new(0));
        *guard = Some(c.clone());
        Some(c)
    }
}

/// Pure classification helper. Mirrors the temperature probe's
/// [`classify_probe_response`](crate::llm::temperature_probe::classify_probe_response)
/// and [`crate::llm::top_p_probe::classify_probe_response`].
pub fn classify_probe_response(status: u16, view: ProbeResponseView<'_>) -> TopKProbeOutcome {
    let trimmed = view.text.trim();
    if (200..400).contains(&status) {
        if trimmed.is_empty() {
            if view.truncated && view.output_tokens > 0 {
                TopKProbeOutcome::Accepted
            } else {
                TopKProbeOutcome::Indeterminate
            }
        } else if body_carries_top_k_rejection(view.text) {
            TopKProbeOutcome::Rejected
        } else {
            TopKProbeOutcome::Accepted
        }
    } else if (400..500).contains(&status) {
        if body_carries_top_k_rejection(view.text) {
            TopKProbeOutcome::Rejected
        } else {
            TopKProbeOutcome::Indeterminate
        }
    } else {
        TopKProbeOutcome::Indeterminate
    }
}

fn outcome_str_for_probe_response(status: u16, view: ProbeResponseView<'_>) -> &'static str {
    match classify_probe_response(status, view) {
        TopKProbeOutcome::Accepted => "accepted",
        TopKProbeOutcome::Rejected => "rejected",
        TopKProbeOutcome::Indeterminate => "indeterminate",
    }
}

fn build_probe_event<'a>(
    provider: &'a str,
    model: &'a str,
    candidate: u32,
    iter: u32,
    outcome: &'static str,
    ts: String,
) -> crate::telemetry::stdout_events::Event<'a> {
    use crate::telemetry::stdout_events::{Event, SCHEMA_VERSION};
    Event::Probe {
        schema: SCHEMA_VERSION,
        ts,
        probe_kind: "top_k",
        candidate: candidate as f32,
        iteration: iter,
        provider,
        model,
        outcome,
    }
}

/// Heuristic: does the response body carry the "top_k rejected"
/// signature?
///
/// The substring `top_k` appears in many benign response bodies
/// (a model that mentions the parameter it was given), so the
/// helper requires the keyword to appear together with at least
/// one of the documented rejection hints. We accept the three
/// common spellings (`top_k`, `top-k`, `topk`) since upstreams
/// disagree on the punctuation.
pub fn body_carries_top_k_rejection(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let has_keyword = lower.contains("top_k") || lower.contains("top-k") || lower.contains("topk");
    if !has_keyword {
        return false;
    }
    let hints = [
        "must",
        "range",
        "out of",
        "unsupported",
        "invalid",
        "exceed",
        "between",
        "value",
        "not allowed",
        "is too large",
        "is too small",
        "must be a positive",
        "must be at least",
    ];
    hints.iter().any(|h| lower.contains(h))
}

/// Retry-once helper: re-fire the same probe once when the first
/// attempt comes back `Indeterminate`. Mirrors
/// [`crate::llm::top_p_probe::retry_once_on_indeterminate`].
async fn retry_once_on_indeterminate(
    transport: &dyn TopKProbeTransport,
    k: u32,
) -> TopKProbeOutcome {
    match transport.probe_send_top_k(k).await {
        TopKProbeOutcome::Indeterminate => transport.probe_send_top_k(k).await,
        other => other,
    }
}

/// Parallel fan-out for a single batch of `top_k` values.
/// Parallel fan-out for a single batch of `top_k` values.
pub async fn parallel_probe(
    transport: Arc<dyn TopKProbeTransport>,
    points: &[u32],
) -> Vec<TopKProbeOutcome> {
    parallel_probe_with_cancel(transport, points, None).await
}

/// Same as [`parallel_probe`] but with an explicit cancellation
/// handle.
pub async fn parallel_probe_with_cancel(
    transport: Arc<dyn TopKProbeTransport>,
    points: &[u32],
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Vec<TopKProbeOutcome> {
    let mut handles = Vec::with_capacity(points.len());
    for &pt in points {
        let t = transport.clone();
        let cancel_child = cancel.as_ref().map(|c| c.child_token());
        handles.push(tokio::spawn(async move {
            if let Some(c) = cancel_child {
                tokio::select! {
                    biased;
                    _ = c.cancelled() => TopKProbeOutcome::Indeterminate,
                    outcome = t.probe_send_top_k(pt) => outcome,
                }
            } else {
                t.probe_send_top_k(pt).await
            }
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(o) => out.push(o),
            Err(e) => {
                tracing::warn!(error = %e, "top_k_probe: parallel_probe task join failed");
                out.push(TopKProbeOutcome::Indeterminate);
            }
        }
    }
    out
}

/// Run the top-k probe algorithm against an
/// `Arc<dyn TopKProbeTransport>`.
pub async fn detect_supported_top_k_values(
    transport: Arc<dyn TopKProbeTransport>,
    batch_size: usize,
) -> Vec<u32> {
    debug_assert!(
        !TOP_K_PROBE_VALUES.is_empty(),
        "TOP_K_PROBE_VALUES must not be empty"
    );
    let effective_batch = if batch_size == 0 {
        TOP_K_PROBE_VALUES.len()
    } else {
        batch_size
    };
    let mut supported = Vec::new();
    for chunk in TOP_K_PROBE_VALUES.chunks(effective_batch) {
        let outcomes = parallel_probe(transport.clone(), chunk).await;
        for (&k, outcome) in chunk.iter().zip(outcomes.iter()) {
            let committed = match outcome {
                TopKProbeOutcome::Indeterminate => {
                    retry_once_on_indeterminate(transport.as_ref(), k).await
                }
                other => other.clone(),
            };
            if matches!(committed, TopKProbeOutcome::Accepted) {
                supported.push(k);
            }
        }
    }
    supported
}

// ---------------------------------------------------------------------
// Sidecar file
// ---------------------------------------------------------------------

/// Serialised shape of the persisted table. Lives in
/// `<MOAGAN_HOME>/top_k_auto.toml` and is read once at startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TopKTableFile {
    /// Schema version. Bumped whenever the file shape changes
    /// incompatibly so a future `moagan` refuses to read a stale
    /// file instead of silently misinterpreting it.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// `provider_name -> model_name -> entry`. The nested
    /// `BTreeMap` gives deterministic on-disk ordering.
    #[serde(default)]
    pub providers: BTreeMap<String, BTreeMap<String, Entry>>,
    /// Operator-pinned per-provider cap.
    #[serde(default)]
    pub operator_caps: BTreeMap<String, OperatorCap>,
}

fn default_schema_version() -> u32 {
    1
}

impl TopKTableFile {
    /// Current schema version this binary knows how to read.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Build an empty table.
    pub fn new_empty() -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            providers: BTreeMap::new(),
            operator_caps: BTreeMap::new(),
        }
    }

    /// Read from a TOML file. Missing file is `Ok(new_empty())`;
    /// malformed file is `Err(Error::Provider(...))`.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let parsed: Self = toml::from_str(&s).map_err(|e| Error::Provider {
                    message: format!("top_k_auto.toml at {} is malformed: {e}", path.display()),
                    http_status: None,
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    return Err(Error::Provider {
                        message: format!(
                            "top_k_auto.toml at {} has schema_version={}, this binary only knows up to {}",
                            path.display(),
                            parsed.schema_version,
                            Self::CURRENT_SCHEMA_VERSION
                        ),
                        http_status: None,
                    });
                }
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new_empty()),
            Err(e) => Err(Error::Io(crate::error::IoError::Raw(e))),
        }
    }

    /// Persist to disk. Writes via `tempfile` then renames.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Provider {
                message: format!(
                    "create dir for top_k_auto.toml at {}: {e}",
                    parent.display()
                ),
                http_status: None,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| Error::Provider {
            message: format!("encode top_k_auto.toml: {e}"),
            http_status: None,
        })?;
        let body = quote_provider_model_keys(&body);
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| Error::Provider {
                message: format!("tempfile for top_k_auto.toml: {e}"),
                http_status: None,
            })?;
        std::fs::write(tmp.path(), body).map_err(|e| Error::Provider {
            message: format!("write top_k_auto.toml: {e}"),
            http_status: None,
        })?;
        tmp.persist(path).map_err(|e| Error::Provider {
            message: format!("rename top_k_auto.toml into place: {e}"),
            http_status: None,
        })?;
        Ok(())
    }
}

/// One row of the persisted table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Discovered smallest accepted `top_k`. `u32` serialises
    /// natively via TOML/JSON without any helper.
    pub top_k: u32,
    /// ISO-8601 timestamp of the last successful probe.
    pub detected_at: String,
    /// ISO-8601 timestamp of the last successful verification
    /// probe.
    #[serde(default)]
    pub verified_at: String,
    /// Always `true` for entries the probe produced.
    pub auto: bool,
    /// How many probes the algorithm ran to discover this value.
    #[serde(default)]
    pub attempts: u32,
}

/// Operator-pinned per-provider cap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCap {
    /// The `top_k` value the operator pinned for this provider.
    pub top_k: u32,
    /// Always `false` for an operator-pinned entry.
    pub auto: bool,
    /// ISO-8601 timestamp the cap was written.
    pub detected_at: String,
}

// ---------------------------------------------------------------------
// In-memory table
// ---------------------------------------------------------------------

/// In-memory table of `(provider_name, model_name) -> Entry`.
/// Wrapped in an `Arc<RwLock>` so a single instance can be cloned
/// into every callsite.
#[derive(Clone)]
pub struct TopKTable {
    /// Shared inner state — `Arc<RwLock>` so a single instance
    /// can be cloned into every callsite.
    inner: Arc<RwLock<TopKTableInner>>,
    /// Path to the on-disk TOML file. `None` when persistence is
    /// disabled.
    persist_path: Option<PathBuf>,
    /// Serialises the on-disk `load → merge → save` sequence so
    /// two concurrent writers within a single process cannot
    /// clobber each other's entries.
    disk_io: Arc<ParkingMutex<()>>,
}

#[derive(Debug)]
struct TopKTableInner {
    /// Cached `(provider, model) -> Entry` map.
    entries: BTreeMap<(String, String), Entry>,
    /// Operator-pinned per-provider cap.
    operator_caps: BTreeMap<String, OperatorCap>,
    /// Total probe tasks started across all calls since startup.
    probe_tasks_started: u32,
    /// [`tokio::task::JoinHandle`]s for every background probe
    /// the registry fired at startup.
    pending: Vec<tokio::task::JoinHandle<()>>,
}

impl TopKTable {
    /// Build a table from the on-disk file at
    /// `<MOAGAN_HOME>/top_k_auto.toml`.
    pub fn from_home(home: &MoaganHome, save: bool) -> Result<Self> {
        let path = home.top_k_auto_path();
        Self::from_path(&path, save)
    }

    /// Build a table from an explicit path.
    pub fn from_path(path: &Path, save: bool) -> Result<Self> {
        let file = TopKTableFile::load(path)?;
        let entries = file
            .providers
            .into_iter()
            .flat_map(|(provider, models)| {
                models
                    .into_iter()
                    .map(move |(model, entry)| ((provider.clone(), model), entry))
            })
            .collect();
        Ok(Self {
            inner: Arc::new(RwLock::new(TopKTableInner {
                entries,
                operator_caps: file.operator_caps,
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: save.then(|| path.to_path_buf()),
            disk_io: Arc::new(ParkingMutex::new(())),
        })
    }

    /// Build a fresh table with no on-disk backing.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TopKTableInner {
                entries: BTreeMap::new(),
                operator_caps: BTreeMap::new(),
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: None,
            disk_io: Arc::new(ParkingMutex::new(())),
        }
    }

    /// Read the cached entry for `(provider, model)`.
    pub fn get(&self, provider: &str, model: &str) -> Option<Entry> {
        self.inner
            .read()
            .entries
            .get(&(provider.to_owned(), model.to_owned()))
            .cloned()
    }

    /// Resolve the nearest supported `top_k` to `requested` for
    /// `(provider, model)`. Returns `None` when no entry exists.
    pub fn nearest_supported(&self, provider: &str, model: &str, requested: u32) -> Option<u32> {
        let inner = self.inner.read();
        let entry = inner
            .entries
            .get(&(provider.to_owned(), model.to_owned()))?;
        let cap = inner.operator_caps.get(provider);
        let snap = match cap {
            None => entry.top_k,
            Some(c) => {
                let req_i = requested as i64;
                let entry_diff = (entry.top_k as i64 - req_i).abs();
                let cap_diff = (c.top_k as i64 - req_i).abs();
                if cap_diff < entry_diff {
                    c.top_k
                } else if cap_diff == entry_diff {
                    // Tie: operator cap wins.
                    c.top_k
                } else {
                    entry.top_k
                }
            }
        };
        tracing::trace!(
            provider,
            model,
            requested,
            discovered = entry.top_k,
            cap = cap.map(|c| c.top_k),
            snap,
            "TopKTable::nearest_supported"
        );
        Some(snap)
    }

    /// Probe the upstream and insert the discovered value.
    pub async fn probe_and_store(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn TopKProbeTransport>,
        batch_size: usize,
    ) -> Result<Vec<u32>> {
        let attempts_before = self.inner.read().probe_tasks_started;
        let discovered = detect_supported_top_k_values(transport, batch_size).await;
        let now = chrono::Utc::now().to_rfc3339();
        // Keep the smallest accepted value: smaller top_k is
        // strictly more deterministic.
        let top_k = discovered.first().copied().unwrap_or(0);
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            let attempts_total = inner.probe_tasks_started - attempts_before;
            inner.entries.insert(
                (provider.to_owned(), model.to_owned()),
                Entry {
                    top_k,
                    detected_at: now.clone(),
                    verified_at: now,
                    auto: true,
                    attempts: attempts_total,
                },
            );
        }
        if let Some(path) = self.persist_path.as_ref()
            && let Err(e) = self.persist_to(path)
        {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "top_k_auto.toml persistence failed; in-memory entry is kept"
            );
        }
        tracing::info!(
            provider,
            model,
            top_k,
            accepted_count = discovered.len(),
            "TopKTable::probe_and_store: completed"
        );
        Ok(discovered)
    }

    /// Verify a cached entry by re-probing once.
    pub async fn verify(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn TopKProbeTransport>,
    ) -> Result<bool> {
        let cached = self.get(provider, model);
        let Some(entry) = cached else {
            return Ok(false);
        };
        let probed_top_k = entry.top_k;
        let probed_detected_at = entry.detected_at.clone();
        let outcome = transport.probe_send_top_k(probed_top_k).await;
        let ok = matches!(outcome, TopKProbeOutcome::Accepted);
        let entry_still_matches;
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            let key = (provider.to_owned(), model.to_owned());
            entry_still_matches = inner
                .entries
                .get(&key)
                .is_some_and(|e| e.top_k == probed_top_k && e.detected_at == probed_detected_at);
            if !entry_still_matches {
                tracing::warn!(
                    provider,
                    model,
                    probed_top_k,
                    "top_k_probe::verify: entry replaced during probe; leaving fresh entry untouched"
                );
            } else if ok {
                if let Some(e) = inner.entries.get_mut(&key) {
                    e.verified_at = chrono::Utc::now().to_rfc3339();
                }
            } else {
                inner.entries.remove(&key);
                tracing::warn!(
                    provider,
                    model,
                    top_k = probed_top_k,
                    "top_k_probe::verify: rejected by upstream; entry dropped"
                );
            }
        }
        if let Some(path) = self.persist_path.as_ref() {
            let _ = self.persist_to(path);
        }
        Ok(ok && entry_still_matches)
    }

    /// Persist the current in-memory state to disk.
    fn persist_to(&self, path: &Path) -> Result<()> {
        let _guard = self.disk_io.lock();
        let inner = self.inner.read();
        let mut file = TopKTableFile::new_empty();
        for ((provider, model), entry) in &inner.entries {
            file.providers
                .entry(provider.clone())
                .or_default()
                .insert(model.clone(), entry.clone());
        }
        for (provider, cap) in &inner.operator_caps {
            file.operator_caps.insert(provider.clone(), cap.clone());
        }
        file.save(path)
    }

    /// Record the [`tokio::task::JoinHandle`] of a background
    /// probe the registry fired at startup.
    pub fn record_probe_join_handle(&self, handle: tokio::task::JoinHandle<()>) {
        let mut inner = self.inner.write();
        inner.pending.push(handle);
    }

    /// Wait for every probe the registry fired at startup to
    /// finish.
    pub async fn await_ready(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut inner = self.inner.write();
            std::mem::take(&mut inner.pending)
        };
        for h in handles {
            if let Err(e) = h.await {
                tracing::warn!(error = %e, "top_k_probe: probe task join failed");
            }
        }
    }

    /// Persist to the path the table was built from.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = self.persist_path.clone() else {
            return Ok(());
        };
        self.persist_to(&path)
    }

    /// Set the operator-pinned cap for a provider.
    pub fn set_operator_cap(&self, provider: &str, top_k: u32) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let cap = OperatorCap {
            top_k,
            auto: false,
            detected_at: now,
        };
        {
            let mut inner = self.inner.write();
            inner.operator_caps.insert(provider.to_owned(), cap);
        }
        if let Some(path) = self.persist_path.clone() {
            self.persist_to(&path)?;
        } else {
            tracing::warn!(
                provider = %provider,
                "top_k_auto: persistence disabled; operator cap not written to disk"
            );
        }
        Ok(())
    }

    /// Effective per-provider operator cap. `None` when no cap
    /// has been recorded for `provider`.
    pub fn operator_cap(&self, provider: &str) -> Option<OperatorCap> {
        self.inner.read().operator_caps.get(provider).cloned()
    }

    /// Total probe tasks started across the lifetime of this
    /// table. Mirrors
    /// [`crate::llm::temperature_probe::TemperatureTable::probe_tasks_started`].
    pub fn probe_tasks_started(&self) -> u32 {
        self.inner.read().probe_tasks_started
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.inner.read().entries.len()
    }

    /// `true` when no entries are cached.
    pub fn is_empty(&self) -> bool {
        self.inner.read().entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test transport that accepts a hard-coded subset of `top_k`
    /// values.
    #[derive(Clone)]
    struct SubsetTransport {
        accept: Arc<std::collections::BTreeSet<u32>>,
    }

    impl SubsetTransport {
        fn accepting(values: &[u32]) -> Self {
            let mut set = std::collections::BTreeSet::new();
            for v in values {
                set.insert(*v);
            }
            Self {
                accept: Arc::new(set),
            }
        }

        fn accepting_all() -> Self {
            let mut set = std::collections::BTreeSet::new();
            for v in TOP_K_PROBE_VALUES {
                set.insert(*v);
            }
            Self {
                accept: Arc::new(set),
            }
        }

        fn rejecting_all() -> Self {
            Self::accepting(&[])
        }
    }

    #[async_trait]
    impl TopKProbeTransport for SubsetTransport {
        async fn probe_send_top_k(&self, k: u32) -> TopKProbeOutcome {
            if self.accept.contains(&k) {
                TopKProbeOutcome::Accepted
            } else {
                TopKProbeOutcome::Rejected
            }
        }
    }

    fn transport(t: SubsetTransport) -> Arc<dyn TopKProbeTransport> {
        Arc::new(t)
    }

    // -----------------------------------------------------------------
    // 1. detect_supported_top_k_values_accepts_subset
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_top_k_values_accepts_subset() {
        let t = transport(SubsetTransport::accepting(&[1, 4, 16]));
        let got = detect_supported_top_k_values(t, TOP_K_PROBE_BATCH_SIZE).await;
        assert_eq!(got, vec![1, 4, 16]);
    }

    // -----------------------------------------------------------------
    // 2. detect_supported_top_k_values_accepts_all
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_top_k_values_accepts_all() {
        let t = transport(SubsetTransport::accepting_all());
        let got = detect_supported_top_k_values(t, TOP_K_PROBE_BATCH_SIZE).await;
        assert_eq!(got, TOP_K_PROBE_VALUES.to_vec());
    }

    // -----------------------------------------------------------------
    // 3. detect_supported_top_k_values_rejects_all
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_top_k_values_rejects_all() {
        let t = transport(SubsetTransport::rejecting_all());
        let got = detect_supported_top_k_values(t, TOP_K_PROBE_BATCH_SIZE).await;
        assert!(got.is_empty(), "got {got:?}");
    }

    // -----------------------------------------------------------------
    // 4. body_carries_top_k_rejection_classifies_correctly
    // -----------------------------------------------------------------
    #[test]
    fn body_carries_top_k_rejection_classifies_correctly() {
        // Positives — the conjunction of `top_k` and a rejection
        // hint must classify as Rejected.
        assert!(body_carries_top_k_rejection(
            "top_k must be a positive integer"
        ));
        assert!(body_carries_top_k_rejection(
            "top_k out of range: max is 40"
        ));
        assert!(body_carries_top_k_rejection(
            "invalid top_k: only 1 is allowed"
        ));
        assert!(body_carries_top_k_rejection(
            "top_k value 9999 is not allowed"
        ));
        assert!(body_carries_top_k_rejection("topk: unsupported value"));
        // Negatives — must NOT trigger on a bare mention of
        // `top_k` without a rejection hint.
        assert!(!body_carries_top_k_rejection(""));
        assert!(!body_carries_top_k_rejection("ok"));
        assert!(!body_carries_top_k_rejection("model not found"));
        assert!(!body_carries_top_k_rejection("top_k = 40"));
    }

    // -----------------------------------------------------------------
    // 5. classify_probe_response_truncation_is_accepted
    // -----------------------------------------------------------------
    #[test]
    fn classify_probe_response_truncation_is_accepted() {
        let v = ProbeResponseView {
            text: "",
            finish_reason: Some("max_tokens"),
            truncated: true,
            output_tokens: 5,
        };
        assert_eq!(classify_probe_response(200, v), TopKProbeOutcome::Accepted);
    }

    // -----------------------------------------------------------------
    // 6. classify_probe_response_empty_no_truncation_is_indeterminate
    // -----------------------------------------------------------------
    #[test]
    fn classify_probe_response_empty_no_truncation_is_indeterminate() {
        let v = ProbeResponseView {
            text: "",
            finish_reason: Some("end_turn"),
            truncated: false,
            output_tokens: 1,
        };
        assert_eq!(
            classify_probe_response(200, v),
            TopKProbeOutcome::Indeterminate
        );
    }

    // -----------------------------------------------------------------
    // 7. classify_probe_response_4xx_with_signature_is_rejected
    // -----------------------------------------------------------------
    #[test]
    fn classify_probe_response_4xx_with_signature_is_rejected() {
        let v = ProbeResponseView {
            text: r#"{"error":{"message":"top_k must be a positive integer"}}"#,
            finish_reason: None,
            truncated: false,
            output_tokens: 0,
        };
        assert_eq!(classify_probe_response(400, v), TopKProbeOutcome::Rejected);
    }

    // -----------------------------------------------------------------
    // 8. classify_probe_response_4xx_without_signature_is_indeterminate
    // -----------------------------------------------------------------
    #[test]
    fn classify_probe_response_4xx_without_signature_is_indeterminate() {
        let v = ProbeResponseView {
            text: "invalid api key",
            finish_reason: None,
            truncated: false,
            output_tokens: 0,
        };
        assert_eq!(
            classify_probe_response(401, v),
            TopKProbeOutcome::Indeterminate
        );
    }

    // -----------------------------------------------------------------
    // 9. classify_probe_response_5xx_is_indeterminate
    // -----------------------------------------------------------------
    #[test]
    fn classify_probe_response_5xx_is_indeterminate() {
        let v = ProbeResponseView {
            text: "upstream is on fire",
            finish_reason: None,
            truncated: false,
            output_tokens: 0,
        };
        assert_eq!(
            classify_probe_response(500, v),
            TopKProbeOutcome::Indeterminate
        );
    }

    // -----------------------------------------------------------------
    // 10. top_k_table_file_round_trip_through_toml
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_file_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_k_auto.toml");
        let mut file = TopKTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_k: 40,
                    detected_at: "2026-09-12T11:24:30Z".to_owned(),
                    verified_at: "2026-09-12T11:24:30Z".to_owned(),
                    auto: true,
                    attempts: 4,
                },
            );
        file.save(&path).unwrap();
        let back = TopKTableFile::load(&path).unwrap();
        assert_eq!(back, file);
    }

    // -----------------------------------------------------------------
    // 11. top_k_table_file_load_rejects_future_schema_version
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_file_load_rejects_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_k_auto.toml");
        std::fs::write(&path, "schema_version = 999\n[providers]\n").unwrap();
        let err = TopKTableFile::load(&path).expect_err("future schema must error");
        match err {
            Error::Provider { message, .. } => {
                assert!(message.contains("schema_version"), "msg: {message}")
            }
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 12. top_k_table_file_load_rejects_malformed_toml
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_file_load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_k_auto.toml");
        std::fs::write(&path, "this is = not valid toml = at all").unwrap();
        let err = TopKTableFile::load(&path).expect_err("malformed must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("malformed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 13. top_k_table_load_missing_file_returns_empty
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_load_missing_file_returns_empty() {
        let path = std::path::PathBuf::from("/nonexistent/top_k_auto.toml");
        let t = TopKTableFile::load(&path).unwrap();
        assert!(t.providers.is_empty());
        assert_eq!(t.schema_version, TopKTableFile::CURRENT_SCHEMA_VERSION);
    }

    // -----------------------------------------------------------------
    // 14. top_k_table_probe_and_store_inserts_entry
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_k_table_probe_and_store_inserts_entry() {
        let table = TopKTable::empty();
        let t = transport(SubsetTransport::accepting(&[1, 4, 16]));
        let discovered = table
            .probe_and_store("minimax", "MiniMax-M3", t, TOP_K_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        assert_eq!(discovered, vec![1, 4, 16]);
        let entry = table
            .get("minimax", "MiniMax-M3")
            .expect("entry must exist");
        // Smallest accepted value is the discovered target.
        assert_eq!(entry.top_k, 1);
        assert!(entry.auto);
        assert!(!entry.detected_at.is_empty());
    }

    // -----------------------------------------------------------------
    // 15. top_k_table_verify_updates_verified_at_on_success
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_k_table_verify_updates_verified_at_on_success() {
        let table = TopKTable::empty();
        let t = transport(SubsetTransport::accepting(&[1, 40]));
        table
            .probe_and_store("minimax", "MiniMax-M3", t.clone(), TOP_K_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        let detected_at = table.get("minimax", "MiniMax-M3").unwrap().detected_at;
        let verified_before = table.get("minimax", "MiniMax-M3").unwrap().verified_at;
        assert_eq!(
            detected_at, verified_before,
            "first probe: detected_at == verified_at"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ok = table.verify("minimax", "MiniMax-M3", t).await.unwrap();
        assert!(ok);
        let verified_after = table.get("minimax", "MiniMax-M3").unwrap().verified_at;
        assert!(
            verified_after >= verified_before,
            "verified_at must not regress: {verified_before} -> {verified_after}"
        );
    }

    // -----------------------------------------------------------------
    // 16. top_k_table_verify_drops_entry_on_failure
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_k_table_verify_drops_entry_on_failure() {
        let table = TopKTable::empty();
        let accepting = transport(SubsetTransport::accepting(&[1]));
        table
            .probe_and_store("minimax", "MiniMax-M3", accepting, TOP_K_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        assert!(table.get("minimax", "MiniMax-M3").is_some());
        // Provider now rejects the previously-accepted value.
        let rejecting = transport(SubsetTransport::rejecting_all());
        let ok = table
            .verify("minimax", "MiniMax-M3", rejecting)
            .await
            .unwrap();
        assert!(!ok);
        assert!(table.get("minimax", "MiniMax-M3").is_none());
    }

    // -----------------------------------------------------------------
    // 17. top_k_table_nearest_supported_returns_nearest
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_nearest_supported_returns_nearest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_k_auto.toml");
        let mut file = TopKTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_k: 40,
                    detected_at: "2026-09-12T11:24:30Z".to_owned(),
                    verified_at: "2026-09-12T11:24:30Z".to_owned(),
                    auto: true,
                    attempts: 1,
                },
            );
        file.save(&path).unwrap();
        let table = TopKTable::from_path(&path, false).unwrap();
        // No operator cap → discovered value is the snap target.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 50),
            Some(40),
            "request 50 snaps to the discovered 40"
        );
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 100),
            Some(40),
            "request 100 snaps to the discovered 40"
        );
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 20),
            Some(40),
            "request 20 snaps to the discovered 40 (single-value snap)"
        );
    }

    // -----------------------------------------------------------------
    // 18. top_k_table_nearest_supported_with_operator_cap_pins_value
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_nearest_supported_with_operator_cap_pins_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_k_auto.toml");
        let mut file = TopKTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_k: 40,
                    detected_at: "2026-09-12T11:24:30Z".to_owned(),
                    verified_at: "2026-09-12T11:24:30Z".to_owned(),
                    auto: true,
                    attempts: 1,
                },
            );
        file.save(&path).unwrap();
        let table = TopKTable::from_path(&path, true).unwrap();
        // Operator pins `cap = 32`.
        table.set_operator_cap("minimax", 32).unwrap();
        // Both 32 and 40 are equidistant from 36; tie resolves
        // to operator cap (32).
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 36),
            Some(32),
            "operator cap wins the tie at 36"
        );
        // 20 is closer to 32 than to 40 — operator cap wins.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 20),
            Some(32)
        );
    }

    // -----------------------------------------------------------------
    // 19. top_k_table_nearest_supported_returns_none_when_empty
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_nearest_supported_returns_none_when_empty() {
        let table = TopKTable::empty();
        assert!(
            table
                .nearest_supported("minimax", "MiniMax-M3", 40)
                .is_none()
        );
    }

    // -----------------------------------------------------------------
    // 20. top_k_table_set_operator_cap_persists_through_reload
    // -----------------------------------------------------------------
    #[test]
    fn top_k_table_set_operator_cap_persists_through_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_k_auto.toml");
        let table = TopKTable::from_path(&path, true).unwrap();
        table.set_operator_cap("minimax", 32).unwrap();
        let back = TopKTable::from_path(&path, false).unwrap();
        let cap = back.operator_cap("minimax").expect("cap must persist");
        assert_eq!(cap.top_k, 32);
        assert!(!cap.auto, "operator cap is always auto = false");
    }

    // -----------------------------------------------------------------
    // 21. top_k_table_persistence_round_trip
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_k_table_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(dir.path().to_path_buf());
        home.ensure().unwrap();
        let table = TopKTable::from_home(&home, true).unwrap();
        let accepting = transport(SubsetTransport::accepting(&[1, 40]));
        table
            .probe_and_store("minimax", "MiniMax-M3", accepting, TOP_K_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        let path = home.top_k_auto_path();
        assert!(path.exists(), "top_k_auto.toml must be on disk");
        let on_disk = TopKTableFile::load(&path).unwrap();
        let entry = on_disk
            .providers
            .get("minimax")
            .and_then(|m| m.get("MiniMax-M3"))
            .expect("entry must exist on disk");
        assert_eq!(entry.top_k, 1);
        assert!(entry.auto);
    }

    // -----------------------------------------------------------------
    // 22. empty_table_is_empty
    // -----------------------------------------------------------------
    #[test]
    fn empty_table_is_empty() {
        let t = TopKTable::empty();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.probe_tasks_started(), 0);
    }

    // -----------------------------------------------------------------
    // 23. constants_have_documented_shape
    // -----------------------------------------------------------------
    #[test]
    fn constants_have_documented_shape() {
        assert_eq!(TOP_K_PROBE_VALUES.len(), 11);
        assert_eq!(TOP_K_PROBE_BATCH_SIZE, 3);
        assert!(TOP_K_PROBE_VALUES.contains(&1));
        assert!(TOP_K_PROBE_VALUES.contains(&1024));
        // Strictly ascending.
        for window in TOP_K_PROBE_VALUES.windows(2) {
            assert!(
                window[0] < window[1],
                "TOP_K_PROBE_VALUES must be strictly ascending: {window:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // 24. from_path_with_save_disabled_does_not_set_persist_path
    // -----------------------------------------------------------------
    #[test]
    fn from_path_with_save_disabled_does_not_set_persist_path() {
        let t = TopKTable::from_path(Path::new("/nonexistent.toml"), false).unwrap();
        t.persist().unwrap();
    }

    // -----------------------------------------------------------------
    // 25. quote_provider_model_keys_quotes_bare_keys
    // -----------------------------------------------------------------
    #[test]
    fn quote_provider_model_keys_quotes_bare_keys() {
        let input = "\
schema_version = 1\n\
[providers.kimi-k3.kimi-k3]\n\
top_k = 40\n\
";
        let out = quote_provider_model_keys(input);
        assert!(
            out.contains(r#"[providers."kimi-k3"."kimi-k3"]"#),
            "bare keys must be quoted; got:\n{out}"
        );
    }
}
