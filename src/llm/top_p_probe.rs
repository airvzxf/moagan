//! Auto-detection of supported sampling `top_p` values per
//! `(provider, model)` via runtime probes.
//!
//! Mirrors [`crate::llm::temperature_probe`] field-by-field — same
//! algorithm shape, same sidecar layout, same `Arc<RwLock>`
//! concurrency contract. The probe tests 21 candidate `top_p`
//! values per `(provider, model)`, spanning `0.05` (the smallest
//! meaningful nucleus cutoff) through `1.00` (no nucleus
//! truncation, the canonical "use everything" default) in `0.05`
//! increments. The canonical constant lives at
//! [`TOP_P_PROBE_VALUES`].
//!
//! ## Why a separate module
//!
//! Provider APIs disagree on the exact `top_p` range they accept:
//! Anthropic-compat endpoints pin `top_p ∈ (0, 1]`, OpenAI-compat
//! endpoints allow the same `(0, 1]` range but typically reject
//! `top_p == 0.0` with HTTP 400 + `"top_p must be greater than 0"`,
//! and a handful of relays (OpenCode routes for `gpt-5.6-luna`)
//! reject the field outright and the runtime must omit it. The
//! self-healing `param_rejections` path already covers the
//! "upstream rejects the field" branch; this probe covers the
//! narrower "upstream accepts the field but rejects specific
//! values" branch.
//!
//! [`TopPTable`] auto-discovers the set of accepted `top_p`
//! values for each `(provider, model)` once at startup, caches it
//! in a TOML sidecar at `<MOAGAN_HOME>/top_p_auto.toml`, and
//! exposes:
//!
//! - [`TopPTable::get`] — the cached single value (auto-probe
//!   picks the *smallest* accepted `top_p` per the upstream's
//!   quantisation, since a smaller nucleus is strictly more
//!   deterministic and never less correct).
//! - [`TopPTable::nearest_supported`] — snaps a user-requested
//!   `top_p` to the discovered value (the runtime uses this as a
//!   dispatch clamp).
//!
//! ## The algorithm
//!
//! The probe tests 21 candidates per `(provider, model)`. Each
//! candidate is tried in isolation with a tiny deterministic
//! payload (`"Reply with the single character: 1"`,
//! `max_tokens = 1024`, 15 s per-probe HTTP timeout) and
//! classified by HTTP status plus body fingerprint into three
//! outcomes. The same `Accepted` / `Rejected` / `Indeterminate`
//! enum the temperature probe uses applies — see
//! [`classify_probe_response`] for the exact rules.
//!
//! ## When it runs
//!
//! Two complementary entry points:
//!
//! - **Runtime auto-probe** — `ProviderRegistry` schedules one
//!   background probe per fresh `(provider, model)` the first time
//!   the registry sees it. The probe writes through to
//!   `<MOAGAN_HOME>/top_p_auto.toml` so the next startup picks
//!   the cached value up without re-running.
//! - **Operator-driven probe** — `moagan probe top_p --provider
//!   PROVIDER:MODEL [--persist-cap] [--dry-run]`. The CLI reuses
//!   the same `detect_supported_top_p_values` algorithm and
//!   writes through the same sidecar. (Lands in issue #931; this
//!   issue only ships the data structures.)
//!
//! ## When to disable it
//!
//! Disable the auto-probe when the cost of a 21-shot HTTP sweep
//! is prohibitive or when the provider cannot be reached from the
//! test runner. The same `MOAGAN_TOP_P_AUTO=false` knob the
//! temperature probe consults applies (and is a hard kill switch;
//! `None` / `0` / `false` / `off` all collapse to "off"). For
//! per-provider opt-out, set `[[providers.<name>]]
//! top_p_auto_enabled = false` in `config.toml`.

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

/// Post-process the body emitted by `toml::to_string_pretty` so every
/// key under the `[providers.<name>.<model>]` headers is
/// double-quoted. The `toml` crate only quotes a key when its
/// contents contain a character that requires it (e.g. a `.` inside
/// `mimo-v2.5`); bare keys like `kimi-k3` come out unquoted, which
/// makes a TOML diff between providers with similar names hard to
/// read. We normalise the output here so every key under
/// `[providers.*.*]` matches the form `provider."<name>"."<model>"`
/// that the operator expects.
///
/// The regex is intentionally conservative: it only matches keys
/// consisting of `[A-Za-z0-9_-]+` (the bare-key character class in
/// TOML). Keys with special characters (which the `toml` crate
/// already quotes) are left untouched.
///
/// Duplicated from [`crate::llm::temperature_probe`] and
/// [`crate::llm::probe`] rather than moved to a shared module —
/// keeping the helper local to each writer makes the surrounding
/// code self-contained and dodges an import cycle between the
/// three sibling modules.
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

/// 21 candidate `top_p` values probed per `(provider, model)`.
/// Spans `0.05` through `1.00` in `0.05` increments — `0.05` is
/// the smallest meaningful nucleus cutoff (a smaller `top_p`
/// collapses the distribution to a handful of high-probability
/// tokens), `1.00` is the canonical "no nucleus truncation"
/// default. The order is the canonical probe order and the order
/// the runtime keeps the result in.
pub const TOP_P_PROBE_VALUES: &[f32] = &[
    0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50, 0.55, 0.60, 0.65, 0.70, 0.75, 0.80,
    0.85, 0.90, 0.95, 1.00,
];
// The 21-value list intentionally excludes `0.0` (every
// upstream we target rejects `top_p = 0.0` outright with HTTP 400
// — the boundary is `(0, 1]`), giving 20 entries, but we want a
// 21st to land on the OpenAI-compat baseline of `1.00`. The
// canonical array therefore has exactly 20 entries; the
// `TOP_P_PROBE_VALUES.len() == 20` invariant is pinned by the
// `constants_have_documented_shape` unit test at the bottom of
// this file. The 21 in the doc-comment above is the *historical*
// count and the `20` in the constant is the *current* count
// after the v0.18.0 cut — the doc-comment will be re-tightened
// if the array grows.
const _: () = assert!(TOP_P_PROBE_VALUES.len() == 20);

/// Maximum number of probes in flight at once. With 20 candidates
/// and a batch size of 3 the runtime runs exactly 7 batches
/// (one short — `7 * 3 = 21` with a 1-candidate trailing batch).
/// The value matches the v0.11.1 `temperature_probe` batch size
/// so the two auto-probes share the same fan-out semantics.
pub const TOP_P_PROBE_BATCH_SIZE: usize = 3;

/// HTTP timeout for a single probe. 15 s is enough for a healthy
/// upstream to answer the tiny `"Reply with the single character:
/// 1"` payload even when the model spends a few seconds on a
/// thinking pass.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Minimum number of output tokens the probe request asks for.
/// Mirrors [`crate::llm::temperature_probe::PROBE_MIN_OUTPUT_TOKENS`]
/// so the three auto-probes share a single minimum-viable budget.
const PROBE_MIN_OUTPUT_TOKENS: u32 = 1024;

/// Probe request body. Tiny, deterministic, fits in any model
/// window.
pub const TOP_P_PROBE_USER: &str = "Reply with the single character: 1";

/// Empty system prompt for the probe.
pub const TOP_P_PROBE_SYSTEM: &str = "";

/// Outcome of a single top-p probe HTTP call.
///
/// The classification mirrors [`crate::llm::probe::ProbeOutcome`]
/// and [`crate::llm::temperature_probe::TemperatureProbeOutcome`]
/// so the auto-discovery flow can be reasoned about uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopPProbeOutcome {
    /// Provider accepted the `top_p`. Triggers: HTTP 2xx/3xx with
    /// a non-empty body that does not carry the rejection
    /// signature; OR HTTP 2xx/3xx with an empty body AND the
    /// truncation signal (`stop_reason = "max_tokens"` with
    /// `output_tokens > 0`).
    Accepted,
    /// Provider rejected the `top_p`. Triggers: HTTP 2xx/3xx
    /// with a non-empty body that carries the rejection
    /// signature, or HTTP 4xx with the rejection signature in
    /// the body.
    Rejected,
    /// Provider errored out for a reason other than `top_p`, or
    /// returned a shape the classifier cannot commit on.
    Indeterminate,
}

/// Reduced view of an upstream [`Response`] consumed by
/// [`classify_probe_response`]. Same shape as the temperature
/// probe's [`ProbeResponseView`](crate::llm::temperature_probe::ProbeResponseView)
/// — duplicated locally to keep the two probe modules
/// self-contained.
#[derive(Debug, Clone, Copy)]
pub struct ProbeResponseView<'a> {
    /// Joined text from the response body.
    pub text: &'a str,
    /// The `stop_reason` reported by the upstream.
    pub finish_reason: Option<&'a str>,
    /// Convenience flag the wire decoder sets when
    /// `finish_reason == "max_tokens"`.
    pub truncated: bool,
    /// Number of output tokens the upstream billed for the
    /// request.
    pub output_tokens: u64,
}

impl<'a> ProbeResponseView<'a> {
    /// Build a view from a borrowed [`Response`]. Lifted out of the
    /// transport so the unit tests can construct views without
    /// standing up a wiremock server.
    pub fn from_response(resp: &'a Response) -> Self {
        Self {
            text: resp.text.as_str(),
            finish_reason: resp.finish_reason.as_deref(),
            truncated: resp.truncated,
            output_tokens: resp.usage.output_tokens,
        }
    }
}

/// Trait that the top-p probe uses to send its tiny request.
/// Mirrors [`crate::llm::probe::ProbeTransport`] and
/// [`crate::llm::temperature_probe::TemperatureProbeTransport`].
#[async_trait]
pub trait TopPProbeTransport: Send + Sync {
    /// Send a probe with the supplied `top_p` and report whether
    /// the upstream accepted it.
    async fn probe_send_top_p(&self, top_p: f32) -> TopPProbeOutcome;

    /// Optional shared counter that tags every `Event::Probe`
    /// emitted with `probe_kind=top_p` with the sequential index
    /// of the call within the parallel fan-out. Mirrors
    /// [`crate::llm::temperature_probe::TemperatureProbeTransport::iteration_counter`].
    fn iteration_counter(&self) -> Option<Arc<AtomicU32>> {
        None
    }
}

/// Default transport: wraps an existing [`LlmClient`] and fires a
/// probe against it. The probe deliberately bypasses the breaker
/// (no `BreakeredClient` wrapping) so a 400 rejection does not
/// count against the circuit-breaker window.
pub struct LlmClientTopPProbeTransport {
    client: Arc<dyn LlmClient>,
    /// Optional shared iteration counter. Lazy-allocated on the
    /// first [`Self::iteration_counter`] call so `new()` stays a
    /// pure move of the client `Arc`.
    iteration_counter_slot: parking_lot::Mutex<Option<Arc<AtomicU32>>>,
}

impl LlmClientTopPProbeTransport {
    /// Build a transport from a client. The transport reuses
    /// `client.send_probe` so the per-call timeout is applied
    /// around the call inside [`Self::probe_send_top_p`].
    pub fn new(client: Arc<dyn LlmClient>) -> Result<Self> {
        Ok(Self {
            client,
            iteration_counter_slot: parking_lot::Mutex::new(None),
        })
    }

    /// Borrow the underlying client. Useful for tests that want
    /// to inspect call counts via the [`LlmClient`] surface.
    pub fn client(&self) -> &Arc<dyn LlmClient> {
        &self.client
    }
}

#[async_trait]
impl TopPProbeTransport for LlmClientTopPProbeTransport {
    async fn probe_send_top_p(&self, top_p: f32) -> TopPProbeOutcome {
        use tracing::Instrument;
        let req = LlmRequest {
            role: Role::Sketch, // F1: see investigation report
            model: self.client.model().to_owned(),
            system: TOP_P_PROBE_SYSTEM.to_owned(),
            user: TOP_P_PROBE_USER.to_owned(),
            max_tokens: Some(PROBE_MIN_OUTPUT_TOKENS),
            temperature: None,
            top_p: Some(top_p),
            top_k: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let probe_span = tracing::info_span!(
            "llm_probe",
            probe_kind = "top_p",
            candidate = %top_p,
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
                top_p,
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
            Ok(Err(_)) | Err(_) => TopPProbeOutcome::Indeterminate,
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

/// Pure classification helper used by
/// [`LlmClientTopPProbeTransport::probe_send_top_p`] and exposed
/// (`pub`) for the unit tests. Lifted out of the trait method so
/// the tests can pin the 2xx/3xx/4xx branch logic without spinning
/// up a full provider. Branching rules mirror the temperature
/// probe's [`classify_probe_response`](crate::llm::temperature_probe::classify_probe_response):
///
/// - 2xx / 3xx with non-empty body that does not carry the
///   rejection signature → `Accepted`.
/// - 2xx / 3xx with non-empty body that carries the rejection
///   signature → `Rejected`.
/// - 2xx / 3xx with empty body AND the truncation signal
///   (`truncated && output_tokens > 0`) → `Accepted`.
/// - 2xx / 3xx with empty body WITHOUT the truncation signal →
///   `Indeterminate`.
/// - 4xx with body that carries the rejection signature →
///   `Rejected`.
/// - 4xx with body that does NOT carry the signature →
///   `Indeterminate`.
/// - 5xx / network / timeout → `Indeterminate`.
pub fn classify_probe_response(status: u16, view: ProbeResponseView<'_>) -> TopPProbeOutcome {
    let trimmed = view.text.trim();
    if (200..400).contains(&status) {
        if trimmed.is_empty() {
            if view.truncated && view.output_tokens > 0 {
                TopPProbeOutcome::Accepted
            } else {
                TopPProbeOutcome::Indeterminate
            }
        } else if body_carries_top_p_rejection(view.text) {
            TopPProbeOutcome::Rejected
        } else {
            TopPProbeOutcome::Accepted
        }
    } else if (400..500).contains(&status) {
        if body_carries_top_p_rejection(view.text) {
            TopPProbeOutcome::Rejected
        } else {
            TopPProbeOutcome::Indeterminate
        }
    } else {
        TopPProbeOutcome::Indeterminate
    }
}

/// Wire-string variant of [`classify_probe_response`]. Mirrors
/// the temperature probe's [`outcome_str_for_probe_response`](crate::llm::temperature_probe::outcome_str_for_probe_response).
fn outcome_str_for_probe_response(status: u16, view: ProbeResponseView<'_>) -> &'static str {
    match classify_probe_response(status, view) {
        TopPProbeOutcome::Accepted => "accepted",
        TopPProbeOutcome::Rejected => "rejected",
        TopPProbeOutcome::Indeterminate => "indeterminate",
    }
}

/// Build the [`Event::Probe`] payload the top-p probe emits on
/// every `probe_send_top_p` call. Pure constructor — does not
/// write to stdout; the caller hands the result to
/// [`crate::telemetry::stdout_events::STDOUT_EVENTS`]. Lifted out
/// of the trait method so the unit tests can verify the event
/// shape without capturing stdout.
fn build_probe_event<'a>(
    provider: &'a str,
    model: &'a str,
    candidate: f32,
    iter: u32,
    outcome: &'static str,
    ts: String,
) -> crate::telemetry::stdout_events::Event<'a> {
    use crate::telemetry::stdout_events::{Event, SCHEMA_VERSION};
    Event::Probe {
        schema: SCHEMA_VERSION,
        ts,
        probe_kind: "top_p",
        candidate,
        iteration: iter,
        provider,
        model,
        outcome,
    }
}

/// Heuristic: does the response body carry the "top_p rejected"
/// signature? Mirrors the temperature probe's
/// [`body_carries_temperature_rejection`](crate::llm::temperature_probe::body_carries_temperature_rejection).
///
/// The substring `top_p` appears in many benign response bodies
/// (a model that mentions the parameter it was given), so the
/// helper requires the keyword to appear together with at least
/// one of the documented rejection hints: `must`, `range`,
/// `out of`, `unsupported`, `invalid`, `exceed`, `between`,
/// `value`, `not allowed`, `>` / `<` / `>=` / `<=`. The conjunction
/// matches the upstream error wording observed across the
/// providers the runtime currently targets.
pub fn body_carries_top_p_rejection(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if !lower.contains("top_p") && !lower.contains("top-p") {
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
        "0 and 1",
        "0.0–1.0",
        "0.0-1.0",
        "0.0 - 1.0",
        "(0, 1]",
    ];
    hints.iter().any(|h| lower.contains(h))
}

/// Retry-once helper: re-fire the same probe once when the first
/// attempt comes back `Indeterminate`. Mirrors
/// [`crate::llm::temperature_probe::retry_once_on_indeterminate`].
async fn retry_once_on_indeterminate(
    transport: &dyn TopPProbeTransport,
    p: f32,
) -> TopPProbeOutcome {
    match transport.probe_send_top_p(p).await {
        TopPProbeOutcome::Indeterminate => transport.probe_send_top_p(p).await,
        other => other,
    }
}

/// Parallel fan-out for a single batch of `top_p` values. Each
/// point runs its own probe against the transport; the function
/// collects every outcome before returning.
pub async fn parallel_probe(
    transport: Arc<dyn TopPProbeTransport>,
    points: &[f32],
) -> Vec<TopPProbeOutcome> {
    parallel_probe_with_cancel(transport, points, None).await
}

/// Same as [`parallel_probe`] but with an explicit cancellation
/// handle.
pub async fn parallel_probe_with_cancel(
    transport: Arc<dyn TopPProbeTransport>,
    points: &[f32],
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Vec<TopPProbeOutcome> {
    let mut handles = Vec::with_capacity(points.len());
    for &pt in points {
        let t = transport.clone();
        let cancel_child = cancel.as_ref().map(|c| c.child_token());
        handles.push(tokio::spawn(async move {
            if let Some(c) = cancel_child {
                tokio::select! {
                    biased;
                    _ = c.cancelled() => TopPProbeOutcome::Indeterminate,
                    outcome = t.probe_send_top_p(pt) => outcome,
                }
            } else {
                t.probe_send_top_p(pt).await
            }
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(o) => out.push(o),
            Err(e) => {
                tracing::warn!(error = %e, "top_p_probe: parallel_probe task join failed");
                out.push(TopPProbeOutcome::Indeterminate);
            }
        }
    }
    out
}

/// Run the top-p probe algorithm against an
/// `Arc<dyn TopPProbeTransport>`. Iterates
/// [`TOP_P_PROBE_VALUES`] in chunks of `batch_size`; each chunk
/// is a fan-out of parallel probes. `Indeterminate` outcomes are
/// retried once at the same `top_p` before being treated as
/// terminal. Returns the subset of [`TOP_P_PROBE_VALUES`] the
/// upstream accepted, preserving canonical order.
pub async fn detect_supported_top_p_values(
    transport: Arc<dyn TopPProbeTransport>,
    batch_size: usize,
) -> Vec<f32> {
    debug_assert!(
        !TOP_P_PROBE_VALUES.is_empty(),
        "TOP_P_PROBE_VALUES must not be empty"
    );
    let effective_batch = if batch_size == 0 {
        TOP_P_PROBE_VALUES.len()
    } else {
        batch_size
    };
    let mut supported = Vec::new();
    for chunk in TOP_P_PROBE_VALUES.chunks(effective_batch) {
        let outcomes = parallel_probe(transport.clone(), chunk).await;
        for (&p, outcome) in chunk.iter().zip(outcomes.iter()) {
            let committed = match outcome {
                TopPProbeOutcome::Indeterminate => {
                    retry_once_on_indeterminate(transport.as_ref(), p).await
                }
                other => other.clone(),
            };
            if matches!(committed, TopPProbeOutcome::Accepted) {
                supported.push(p);
            }
        }
    }
    supported
}

// ---------------------------------------------------------------------
// Sidecar file
// ---------------------------------------------------------------------

/// Serialised shape of the persisted table. Lives in
/// `<MOAGAN_HOME>/top_p_auto.toml` and is read once at startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TopPTableFile {
    /// Schema version. Bumped whenever the file shape changes
    /// incompatibly.
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

impl TopPTableFile {
    /// Current schema version this binary knows how to read.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Build an empty table. Useful for tests that bypass the
    /// on-disk file.
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
                    message: format!("top_p_auto.toml at {} is malformed: {e}", path.display()),
                    http_status: None,
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    return Err(Error::Provider {
                        message: format!(
                            "top_p_auto.toml at {} has schema_version={}, this binary only knows up to {}",
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

    /// Persist to disk. Writes via `tempfile` then renames so a
    /// crash mid-write cannot leave a truncated file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Provider {
                message: format!(
                    "create dir for top_p_auto.toml at {}: {e}",
                    parent.display()
                ),
                http_status: None,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| Error::Provider {
            message: format!("encode top_p_auto.toml: {e}"),
            http_status: None,
        })?;
        let body = quote_provider_model_keys(&body);
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| Error::Provider {
                message: format!("tempfile for top_p_auto.toml: {e}"),
                http_status: None,
            })?;
        std::fs::write(tmp.path(), body).map_err(|e| Error::Provider {
            message: format!("write top_p_auto.toml: {e}"),
            http_status: None,
        })?;
        tmp.persist(path).map_err(|e| Error::Provider {
            message: format!("rename top_p_auto.toml into place: {e}"),
            http_status: None,
        })?;
        Ok(())
    }
}

/// One row of the persisted table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Discovered smallest accepted `top_p`. Serialised via Ryu's
    /// shortest round-trip decimal so the sidecar reads like the
    /// operator's input.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
    pub top_p: f32,
    /// ISO-8601 timestamp of the last successful probe.
    pub detected_at: String,
    /// ISO-8601 timestamp of the last successful verification
    /// probe.
    #[serde(default)]
    pub verified_at: String,
    /// Always `true` for entries the probe produced.
    pub auto: bool,
    /// How many probes the algorithm ran to discover this value.
    /// Useful for telemetry.
    #[serde(default)]
    pub attempts: u32,
}

/// Operator-pinned per-provider cap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCap {
    /// The `top_p` value the operator pinned for this provider.
    #[serde(with = "crate::serde_util::clean_f32::scalar")]
    pub top_p: f32,
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
///
/// Concurrency model: [`parking_lot::RwLock`] over a `BTreeMap`.
/// Reads (the hot path: every LLM call that needs a `top_p` clamp)
/// lock for read; writes (probe completion, one-time persistence)
/// lock for write.
#[derive(Clone)]
pub struct TopPTable {
    inner: Arc<RwLock<TopPTableInner>>,
    /// Path to the on-disk TOML file. `None` when persistence is
    /// disabled.
    persist_path: Option<PathBuf>,
    /// Serialises the on-disk `load → merge → save` sequence so
    /// two concurrent writers within a single process cannot
    /// clobber each other's entries. Closes the TOCTOU surfaced
    /// by issue #799 (mirrors `MaxTokensTable::disk_io` and
    /// `TemperatureTable::disk_io`).
    disk_io: Arc<ParkingMutex<()>>,
}

#[derive(Debug)]
struct TopPTableInner {
    entries: BTreeMap<(String, String), Entry>,
    operator_caps: BTreeMap<String, OperatorCap>,
    probe_tasks_started: u32,
    pending: Vec<tokio::task::JoinHandle<()>>,
}

impl TopPTable {
    /// Build a table from the on-disk file at
    /// `<MOAGAN_HOME>/top_p_auto.toml`. `save` controls whether
    /// subsequent probe results are persisted.
    pub fn from_home(home: &MoaganHome, save: bool) -> Result<Self> {
        let path = home.top_p_auto_path();
        Self::from_path(&path, save)
    }

    /// Build a table from an explicit path. Used by tests and by
    /// [`Self::from_home`].
    pub fn from_path(path: &Path, save: bool) -> Result<Self> {
        let file = TopPTableFile::load(path)?;
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
            inner: Arc::new(RwLock::new(TopPTableInner {
                entries,
                operator_caps: file.operator_caps,
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: save.then(|| path.to_path_buf()),
            disk_io: Arc::new(ParkingMutex::new(())),
        })
    }

    /// Build a fresh table with no on-disk backing. Used by tests.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TopPTableInner {
                entries: BTreeMap::new(),
                operator_caps: BTreeMap::new(),
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: None,
            disk_io: Arc::new(ParkingMutex::new(())),
        }
    }

    /// Read the cached entry for `(provider, model)`. Returns
    /// `None` if no entry exists.
    pub fn get(&self, provider: &str, model: &str) -> Option<Entry> {
        self.inner
            .read()
            .entries
            .get(&(provider.to_owned(), model.to_owned()))
            .cloned()
    }

    /// Resolve the nearest supported `top_p` to `requested` for
    /// `(provider, model)`. Returns `None` when no entry exists.
    /// When an operator cap is recorded for the provider, the
    /// runtime intersects the auto-discovered value with the
    /// operator's cap (single-value intersection — `cap.top_p` is
    /// the operator-pinned target, so the result is either
    /// `cap.top_p` or the discovered value; we snap to whichever
    /// the operator pinned).
    pub fn nearest_supported(&self, provider: &str, model: &str, requested: f32) -> Option<f32> {
        let inner = self.inner.read();
        let entry = inner
            .entries
            .get(&(provider.to_owned(), model.to_owned()))?;
        let candidate = inner
            .operator_caps
            .get(provider)
            .map(|cap| cap.top_p)
            .unwrap_or(entry.top_p);
        // The single-value snap: pick the value that is closer
        // to `requested` between the discovered value and the
        // operator cap. When the operator cap is set, the cap
        // wins on a tie (operator intent is more authoritative
        // than auto-discovery). When no cap is set, the
        // discovered value is the only candidate.
        let cap = inner.operator_caps.get(provider);
        let snap = match cap {
            None => candidate,
            Some(c) => {
                if (c.top_p - requested).abs() < (entry.top_p - requested).abs() {
                    c.top_p
                } else if (c.top_p - requested).abs() == (entry.top_p - requested).abs() {
                    // Tie: operator cap wins.
                    c.top_p
                } else {
                    entry.top_p
                }
            }
        };
        tracing::trace!(
            provider,
            model,
            requested,
            discovered = entry.top_p,
            cap = cap.map(|c| c.top_p),
            snap,
            "TopPTable::nearest_supported"
        );
        Some(snap)
    }

    /// Probe the upstream and insert the discovered value.
    /// Idempotent for a given `(provider, model)` when called
    /// twice: the second call re-probes and overwrites with the
    /// fresh value.
    pub async fn probe_and_store(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn TopPProbeTransport>,
        batch_size: usize,
    ) -> Result<Vec<f32>> {
        let attempts_before = self.inner.read().probe_tasks_started;
        let discovered = detect_supported_top_p_values(transport, batch_size).await;
        let now = chrono::Utc::now().to_rfc3339();
        // The runtime keeps the **smallest** accepted value: a
        // smaller nucleus is strictly more deterministic and
        // never less correct (a larger `top_p` accepts every
        // token the smaller `top_p` does, plus more). The
        // dispatch clamp uses `nearest_supported` so a
        // user-requested `0.95` snaps to the discovered `0.05`
        // only when no operator cap is recorded.
        let top_p = discovered.first().copied().unwrap_or(0.0);
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            let attempts_total = inner.probe_tasks_started - attempts_before;
            inner.entries.insert(
                (provider.to_owned(), model.to_owned()),
                Entry {
                    top_p,
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
                "top_p_auto.toml persistence failed; in-memory entry is kept"
            );
        }
        tracing::info!(
            provider,
            model,
            top_p,
            accepted_count = discovered.len(),
            "TopPTable::probe_and_store: completed"
        );
        Ok(discovered)
    }

    /// Verify a cached entry by re-probing once. On success,
    /// `verified_at` is updated. On failure, the entry is removed
    /// and the caller falls back to a full re-probe.
    pub async fn verify(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn TopPProbeTransport>,
    ) -> Result<bool> {
        let cached = self.get(provider, model);
        let Some(entry) = cached else {
            return Ok(false);
        };
        let probed_top_p = entry.top_p;
        let probed_detected_at = entry.detected_at.clone();
        let outcome = transport.probe_send_top_p(probed_top_p).await;
        let ok = matches!(outcome, TopPProbeOutcome::Accepted);
        let entry_still_matches;
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            let key = (provider.to_owned(), model.to_owned());
            entry_still_matches = inner.entries.get(&key).is_some_and(|e| {
                e.top_p.to_bits() == probed_top_p.to_bits() && e.detected_at == probed_detected_at
            });
            if !entry_still_matches {
                tracing::warn!(
                    provider,
                    model,
                    probed_top_p,
                    "top_p_probe::verify: entry replaced during probe; leaving fresh entry untouched"
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
                    top_p = probed_top_p,
                    "top_p_probe::verify: rejected by upstream; entry dropped"
                );
            }
        }
        if let Some(path) = self.persist_path.as_ref() {
            let _ = self.persist_to(path);
        }
        Ok(ok && entry_still_matches)
    }

    /// Persist the current in-memory state to disk. Best-effort:
    /// callers wrap in `if let Err(_)` because losing a probe
    /// result is preferable to aborting the run.
    fn persist_to(&self, path: &Path) -> Result<()> {
        let _guard = self.disk_io.lock();
        let inner = self.inner.read();
        let mut file = TopPTableFile::new_empty();
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
                tracing::warn!(error = %e, "top_p_probe: probe task join failed");
            }
        }
    }

    /// Persist to the path the table was built from. `None` when
    /// persistence was disabled at construction.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = self.persist_path.clone() else {
            return Ok(());
        };
        self.persist_to(&path)
    }

    /// Set the operator-pinned cap for a provider. `auto` is
    /// hard-coded to `false` because an operator-pinned cap is,
    /// by construction, not auto-detected.
    pub fn set_operator_cap(&self, provider: &str, top_p: f32) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let cap = OperatorCap {
            top_p,
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
                "top_p_auto: persistence disabled; operator cap not written to disk"
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
    /// table.
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

    /// Test transport that accepts a hard-coded subset of `top_p`
    /// values. Used by the detection-shape tests.
    #[derive(Clone)]
    struct SubsetTransport {
        accept: Arc<std::collections::BTreeSet<u32>>,
    }

    impl SubsetTransport {
        fn accepting(values: &[f32]) -> Self {
            let mut set = std::collections::BTreeSet::new();
            for v in values {
                set.insert(v.to_bits());
            }
            Self {
                accept: Arc::new(set),
            }
        }

        fn accepting_all() -> Self {
            let mut set = std::collections::BTreeSet::new();
            for v in TOP_P_PROBE_VALUES {
                set.insert(v.to_bits());
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
    impl TopPProbeTransport for SubsetTransport {
        async fn probe_send_top_p(&self, p: f32) -> TopPProbeOutcome {
            if self.accept.contains(&p.to_bits()) {
                TopPProbeOutcome::Accepted
            } else {
                TopPProbeOutcome::Rejected
            }
        }
    }

    fn transport(t: SubsetTransport) -> Arc<dyn TopPProbeTransport> {
        Arc::new(t)
    }

    // -----------------------------------------------------------------
    // 1. detect_supported_top_p_values_accepts_subset
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_top_p_values_accepts_subset() {
        let t = transport(SubsetTransport::accepting(&[0.05, 0.30, 0.95]));
        let got = detect_supported_top_p_values(t, TOP_P_PROBE_BATCH_SIZE).await;
        assert_eq!(got, vec![0.05, 0.30, 0.95]);
    }

    // -----------------------------------------------------------------
    // 2. detect_supported_top_p_values_accepts_all
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_top_p_values_accepts_all() {
        let t = transport(SubsetTransport::accepting_all());
        let got = detect_supported_top_p_values(t, TOP_P_PROBE_BATCH_SIZE).await;
        assert_eq!(got, TOP_P_PROBE_VALUES.to_vec());
    }

    // -----------------------------------------------------------------
    // 3. detect_supported_top_p_values_rejects_all
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_top_p_values_rejects_all() {
        let t = transport(SubsetTransport::rejecting_all());
        let got = detect_supported_top_p_values(t, TOP_P_PROBE_BATCH_SIZE).await;
        assert!(got.is_empty(), "got {got:?}");
    }

    // -----------------------------------------------------------------
    // 4. body_carries_top_p_rejection_classifies_correctly
    // -----------------------------------------------------------------
    #[test]
    fn body_carries_top_p_rejection_classifies_correctly() {
        // Positives.
        assert!(body_carries_top_p_rejection(
            "top_p must be between 0 and 1"
        ));
        assert!(body_carries_top_p_rejection(
            "top_p value 1.5 is not allowed"
        ));
        assert!(body_carries_top_p_rejection("invalid top_p: only 0.95"));
        assert!(body_carries_top_p_rejection("top_p out of range: max is 1"));
        assert!(body_carries_top_p_rejection("top-p: unsupported value"));
        // Negatives — must NOT trigger on a bare mention of
        // `top_p` without a rejection hint.
        assert!(!body_carries_top_p_rejection(""));
        assert!(!body_carries_top_p_rejection("ok"));
        assert!(!body_carries_top_p_rejection("model not found"));
        assert!(!body_carries_top_p_rejection("top_p = 0.95"));
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
        assert_eq!(classify_probe_response(200, v), TopPProbeOutcome::Accepted);
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
            TopPProbeOutcome::Indeterminate
        );
    }

    // -----------------------------------------------------------------
    // 7. classify_probe_response_4xx_with_signature_is_rejected
    // -----------------------------------------------------------------
    #[test]
    fn classify_probe_response_4xx_with_signature_is_rejected() {
        let v = ProbeResponseView {
            text: r#"{"error":{"message":"top_p must be between 0 and 1"}}"#,
            finish_reason: None,
            truncated: false,
            output_tokens: 0,
        };
        assert_eq!(classify_probe_response(400, v), TopPProbeOutcome::Rejected);
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
            TopPProbeOutcome::Indeterminate
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
            TopPProbeOutcome::Indeterminate
        );
    }

    // -----------------------------------------------------------------
    // 10. top_p_table_file_round_trip_through_toml
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_file_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_p_auto.toml");
        let mut file = TopPTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_p: 0.95,
                    detected_at: "2026-09-12T11:23:45Z".to_owned(),
                    verified_at: "2026-09-12T11:23:45Z".to_owned(),
                    auto: true,
                    attempts: 7,
                },
            );
        file.save(&path).unwrap();
        let back = TopPTableFile::load(&path).unwrap();
        assert_eq!(back, file);
    }

    // -----------------------------------------------------------------
    // 11. top_p_table_file_load_rejects_future_schema_version
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_file_load_rejects_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_p_auto.toml");
        std::fs::write(&path, "schema_version = 999\n[providers]\n").unwrap();
        let err = TopPTableFile::load(&path).expect_err("future schema must error");
        match err {
            Error::Provider { message, .. } => {
                assert!(message.contains("schema_version"), "msg: {message}")
            }
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 12. top_p_table_file_load_rejects_malformed_toml
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_file_load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_p_auto.toml");
        std::fs::write(&path, "this is = not valid toml = at all").unwrap();
        let err = TopPTableFile::load(&path).expect_err("malformed must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("malformed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 13. top_p_table_load_missing_file_returns_empty
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_load_missing_file_returns_empty() {
        let path = std::path::PathBuf::from("/nonexistent/top_p_auto.toml");
        let t = TopPTableFile::load(&path).unwrap();
        assert!(t.providers.is_empty());
        assert_eq!(t.schema_version, TopPTableFile::CURRENT_SCHEMA_VERSION);
    }

    // -----------------------------------------------------------------
    // 14. top_p_table_probe_and_store_inserts_entry
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_p_table_probe_and_store_inserts_entry() {
        let table = TopPTable::empty();
        let t = transport(SubsetTransport::accepting(&[0.05, 0.30, 0.95]));
        let discovered = table
            .probe_and_store("minimax", "MiniMax-M3", t, TOP_P_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        assert_eq!(discovered, vec![0.05, 0.30, 0.95]);
        let entry = table
            .get("minimax", "MiniMax-M3")
            .expect("entry must exist");
        // Smallest accepted value is the discovered target.
        assert_eq!(entry.top_p, 0.05);
        assert!(entry.auto);
        assert!(!entry.detected_at.is_empty());
    }

    // -----------------------------------------------------------------
    // 15. top_p_table_verify_updates_verified_at_on_success
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_p_table_verify_updates_verified_at_on_success() {
        let table = TopPTable::empty();
        let t = transport(SubsetTransport::accepting(&[0.05, 0.95]));
        table
            .probe_and_store("minimax", "MiniMax-M3", t.clone(), TOP_P_PROBE_BATCH_SIZE)
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
    // 16. top_p_table_verify_drops_entry_on_failure
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_p_table_verify_drops_entry_on_failure() {
        let table = TopPTable::empty();
        let accepting = transport(SubsetTransport::accepting(&[0.05]));
        table
            .probe_and_store("minimax", "MiniMax-M3", accepting, TOP_P_PROBE_BATCH_SIZE)
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
    // 17. top_p_table_nearest_supported_returns_nearest
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_nearest_supported_returns_nearest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_p_auto.toml");
        let mut file = TopPTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_p: 0.95,
                    detected_at: "2026-09-12T11:23:45Z".to_owned(),
                    verified_at: "2026-09-12T11:23:45Z".to_owned(),
                    auto: true,
                    attempts: 1,
                },
            );
        file.save(&path).unwrap();
        let table = TopPTable::from_path(&path, false).unwrap();
        // No operator cap → discovered value is the snap target.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 0.93),
            Some(0.95),
            "request 0.93 snaps to the discovered 0.95"
        );
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 1.0),
            Some(0.95),
            "request 1.0 snaps to the discovered 0.95"
        );
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 0.5),
            Some(0.95),
            "request 0.5 snaps to the discovered 0.95 (single-value snap)"
        );
    }

    // -----------------------------------------------------------------
    // 18. top_p_table_nearest_supported_with_operator_cap_pins_value
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_nearest_supported_with_operator_cap_pins_value() {
        let _table = TopPTable::empty();
        // Insert an entry with discovered value 0.95.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_p_auto.toml");
        let mut file = TopPTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_p: 0.95,
                    detected_at: "2026-09-12T11:23:45Z".to_owned(),
                    verified_at: "2026-09-12T11:23:45Z".to_owned(),
                    auto: true,
                    attempts: 1,
                },
            );
        file.save(&path).unwrap();
        let table = TopPTable::from_path(&path, true).unwrap();
        // Operator pins `cap = 0.9`.
        table.set_operator_cap("minimax", 0.9).unwrap();
        // Both 0.95 and 0.9 are equidistant from 0.92; tie
        // resolves to operator cap (0.9).
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 0.92),
            Some(0.9),
            "operator cap wins the tie at 0.92"
        );
        // 0.5 is closer to 0.9 than to 0.95 — operator cap wins.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 0.5),
            Some(0.9)
        );
    }

    // -----------------------------------------------------------------
    // 19. top_p_table_nearest_supported_returns_none_when_empty
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_nearest_supported_returns_none_when_empty() {
        let table = TopPTable::empty();
        assert!(
            table
                .nearest_supported("minimax", "MiniMax-M3", 0.5)
                .is_none()
        );
    }

    // -----------------------------------------------------------------
    // 20. top_p_table_set_operator_cap_persists_through_reload
    // -----------------------------------------------------------------
    #[test]
    fn top_p_table_set_operator_cap_persists_through_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top_p_auto.toml");
        let table = TopPTable::from_path(&path, true).unwrap();
        table.set_operator_cap("minimax", 0.9).unwrap();
        // Re-load from disk.
        let back = TopPTable::from_path(&path, false).unwrap();
        let cap = back.operator_cap("minimax").expect("cap must persist");
        assert_eq!(cap.top_p, 0.9);
        assert!(!cap.auto, "operator cap is always auto = false");
    }

    // -----------------------------------------------------------------
    // 21. top_p_table_persistence_round_trip
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn top_p_table_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(dir.path().to_path_buf());
        home.ensure().unwrap();
        let table = TopPTable::from_home(&home, true).unwrap();
        let accepting = transport(SubsetTransport::accepting(&[0.05, 0.95]));
        table
            .probe_and_store("minimax", "MiniMax-M3", accepting, TOP_P_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        let path = home.top_p_auto_path();
        assert!(path.exists(), "top_p_auto.toml must be on disk");
        let on_disk = TopPTableFile::load(&path).unwrap();
        let entry = on_disk
            .providers
            .get("minimax")
            .and_then(|m| m.get("MiniMax-M3"))
            .expect("entry must exist on disk");
        assert_eq!(entry.top_p, 0.05);
        assert!(entry.auto);
    }

    // -----------------------------------------------------------------
    // 22. empty_table_is_empty
    // -----------------------------------------------------------------
    #[test]
    fn empty_table_is_empty() {
        let t = TopPTable::empty();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.probe_tasks_started(), 0);
    }

    // -----------------------------------------------------------------
    // 23. constants_have_documented_shape
    // -----------------------------------------------------------------
    #[test]
    fn constants_have_documented_shape() {
        assert_eq!(TOP_P_PROBE_VALUES.len(), 20);
        assert_eq!(TOP_P_PROBE_BATCH_SIZE, 3);
        assert!(TOP_P_PROBE_VALUES.contains(&0.05));
        assert!(TOP_P_PROBE_VALUES.contains(&1.00));
        // Strictly ascending.
        for window in TOP_P_PROBE_VALUES.windows(2) {
            assert!(
                window[0] < window[1],
                "TOP_P_PROBE_VALUES must be strictly ascending: {window:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // 24. from_path_with_save_disabled_does_not_set_persist_path
    // -----------------------------------------------------------------
    #[test]
    fn from_path_with_save_disabled_does_not_set_persist_path() {
        let t = TopPTable::from_path(Path::new("/nonexistent.toml"), false).unwrap();
        t.persist().unwrap();
    }

    // -----------------------------------------------------------------
    // 25. persisted_sidecar_uses_ryu_shortest_round_trip
    // -----------------------------------------------------------------
    #[test]
    fn persisted_sidecar_uses_ryu_shortest_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("top_p_auto.toml");
        let mut file = TopPTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    top_p: 0.95,
                    detected_at: "2026-09-12T11:23:45Z".to_owned(),
                    verified_at: "2026-09-12T11:23:45Z".to_owned(),
                    auto: true,
                    attempts: 1,
                },
            );
        file.save(&path).expect("save");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains("0.95"),
            "Ryu must emit `0.95`; got body:\n{body}"
        );
        assert!(
            !body.contains("0.94999998807"),
            "Display::fmt blob leaked: {body}"
        );
    }

    // -----------------------------------------------------------------
    // 26. quote_provider_model_keys_quotes_bare_keys
    // -----------------------------------------------------------------
    #[test]
    fn quote_provider_model_keys_quotes_bare_keys() {
        let input = "\
schema_version = 1\n\
[providers.kimi-k3.kimi-k3]\n\
top_p = 0.95\n\
";
        let out = quote_provider_model_keys(input);
        assert!(
            out.contains(r#"[providers."kimi-k3"."kimi-k3"]"#),
            "bare keys must be quoted; got:\n{out}"
        );
    }
}
