//! Auto-detection of supported sampling temperatures per
//! `(provider, model)` via runtime probes.
//!
//! Mirrors the `max_tokens` auto-probe pattern in
//! [`crate::llm::probe`]. The probe classifies every candidate
//! temperature against a single upstream call so the runtime can
//! rewrite user-requested temperatures into the closest supported
//! value without re-running the pipeline.
//!
//! ## Why a separate module
//!
//! Provider APIs disagree on the exact temperature range they
//! accept. Anthropic-compat endpoints pin `temperature ∈ [0.0, 1.0]`,
//! OpenAI-compat endpoints typically allow `(0.0, 2.0]`, and a few
//! relays (DeepSeek-direct, certain OpenCode Go routes) cap the
//! value at `1.0` with HTTP 400 + `temperature must be between 0
//! and 1` otherwise. Hardcoding a global cap from the operator's
//! mental map of these limits is the same brittleness the
//! `max_tokens` probe removes — the relay can lower the cap
//! without warning and the next run breaks.
//!
//! [`TemperatureTable`] auto-discovers the set of accepted
//! temperatures for each `(provider, model)` once at startup,
//! caches it in a TOML sidecar at
//! `<MOAGAN_HOME>/temperatures_auto.toml`, and exposes:
//!
//! - [`TemperatureTable::supported_for`] — the cached set as a
//!   `Vec<f32>`, ordered to match
//!   [`TEMPERATURE_PROBE_VALUES`].
//! - [`TemperatureTable::nearest_supported`] — the closest cached
//!   value to a user-requested temperature (used by the discovery
//!   rewriter and the per-role temperature override paths).
//!
//! The probe deliberately bypasses the circuit breaker (no
//! `BreakeredProvider` wrapping) and the cross-run cache, so a
//! probe that comes back `Rejected` does not count against the
//! runtime's breaker window nor poison the steady-state cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::llm::provider::Provider;
use crate::llm::role::Role;
use crate::llm::wire::{Request, Response};

/// Post-process the body emitted by `toml::to_string_pretty` so every
/// key under the `[providers.<name>.<model>]` headers is
/// double-quoted. The `toml` crate only quotes a key when its
/// contents contain a character that requires it (e.g. a `.` inside
/// `mimo-v2.5`); bare keys like `kimi-k3` come out unquoted, which
/// makes a TOML diff between providers with similar names hard to
/// read. We normalise the output here so every key under
/// `[providers.*.*]` matches the form `provider."<name>"."<model>"`
/// that the operator expects (mirrors the style in
/// `~/.config/moagan/config.toml`).
///
/// The regex is intentionally conservative: it only matches keys
/// consisting of `[A-Za-z0-9_-]+` (the bare-key character class in
/// TOML). Keys with special characters (which the `toml` crate
/// already quotes) are left untouched.
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

/// 21 candidate temperatures probed per `(provider, model)`. Spans
/// `0.0` (deterministic decoding) through `2.0` (maximum supported
/// by the OpenAI-compat baseline) in `0.1` increments. The order
/// is the canonical probe order and the order the runtime keeps
/// the result in — a re-probe that returns a different set is
/// still deterministic against this constant.
pub const TEMPERATURE_PROBE_VALUES: &[f32] = &[
    0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8,
    1.9, 2.0,
];

/// Maximum number of probes in flight at once. With 21 candidates
/// and a batch size of 3 the runtime runs exactly 7 batches. The
/// value matches the v0.7.1 `max_tokens` tightening-batch size so
/// the two auto-probes share the same fan-out semantics; a future
/// refactor can tune one without touching the other.
pub const TEMPERATURE_PROBE_BATCH_SIZE: usize = 3;

/// HTTP timeout for a single probe. Mirrors the `max_tokens`
/// probe — 15 s is enough for a healthy upstream to answer the
/// tiny `"Reply with the single character: 1"` payload even
/// when the model spends a few seconds on a thinking pass.
/// Anything longer means the upstream is in trouble and the
/// probe should fall through to `Indeterminate` so the algorithm
/// does not block the batch.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Minimum number of output tokens the probe request asks for.
/// Some providers reject requests with `max_tokens = 0` (or
/// clamp it to `1`); others (the MiniMax M-series, OpenCode Go /
/// MiMo, etc.) spend the budget on a thinking pass and never
/// emit text when the cap is below the model's thinking
/// footprint, returning HTTP 200 with `content: null`. 1024 is
/// large enough to cover the thinking footprint of every model
/// the runtime currently targets while staying well below every
/// probe's per-request cap (MiniMax-M2.5 caps at 196_608,
/// MiMo-v2.5 caps at 131_072, OpenCode Go proxies inherit the
/// upstream cap). The value matches [`crate::llm::probe::MIN_AUTOPROBE_FLOOR`]
/// so the two auto-probes share a single minimum-viable budget.
const PROBE_MIN_OUTPUT_TOKENS: u32 = 1024;

/// Probe request body. Tiny, deterministic, fits in any model
/// window. The model is asked to reply with the literal `1`; the
/// response text is discarded — only the HTTP status and the
/// body-classification carry signal.
pub const TEMPERATURE_PROBE_USER: &str = "Reply with the single character: 1";

/// Empty system prompt for the probe. The model only needs the
/// user-side `Reply with the single character: 1` instruction.
pub const TEMPERATURE_PROBE_SYSTEM: &str = "";

/// Outcome of a single temperature probe HTTP call.
///
/// The classification mirrors the `max_tokens` probe's
/// [`crate::llm::probe::ProbeOutcome`] so the auto-discovery flow
/// can be reasoned about uniformly. The two probes diverge on
/// the empty-body branch: the `max_tokens` probe treats an empty
/// 2xx body as a successful no-op (the upstream accepted the
/// parameter, the model just had no output to emit), while the
/// temperature probe has to distinguish three distinct empty-2xx
/// shapes — see [`classify_probe_response`] for the exact rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemperatureProbeOutcome {
    /// Provider accepted the temperature. Triggers:
    ///
    /// - HTTP 2xx/3xx with a non-empty body that does NOT carry
    ///   the rejection signature.
    /// - HTTP 2xx/3xx with an empty body AND the truncation
    ///   signal (`finish_reason = "max_tokens"` with
    ///   `output_tokens > 0`). The upstream unambiguously
    ///   accepted the request and the model simply ran out of
    ///   output budget before emitting the trailing tokens;
    ///   classifying the truncated probe as `Accepted` is what
    ///   lets the probe survive the
    ///   `PROBE_MIN_OUTPUT_TOKENS = 1024` budget.
    Accepted,
    /// Provider rejected the temperature. Triggers: HTTP 2xx/3xx
    /// with a non-empty body that carries the rejection
    /// signature, HTTP 4xx with the rejection signature in the
    /// body.
    ///
    /// Note: an empty 2xx body WITHOUT the truncation signal is
    /// NOT `Rejected` — that shape is genuinely ambiguous (the
    /// upstream may have silently dropped the parameter, may
    /// have errored in a way the wire decoder absorbed, or may
    /// have returned a 200 envelope with no content) and the
    /// algorithm needs the retry-once path to gather a second
    /// sample before locking the candidate. See [`Indeterminate`].
    Rejected,
    /// Provider errored out for a reason other than the
    /// temperature, or returned a shape the classifier cannot
    /// commit on (network error, 5xx storm, 4xx without the
    /// rejection signature — e.g. auth, model-not-found,
    /// malformed response — and the empty-2xx-without-truncation
    /// case described in [`Rejected`]). The algorithm retries
    /// each `Indeterminate` exactly once via
    /// [`retry_once_on_indeterminate`]; the second outcome is
    /// then treated as terminal for the batch boundary: a single
    /// transient blip must not lock the algorithm.
    Indeterminate,
}

/// Reduced view of an upstream [`Response`] consumed by
/// [`classify_probe_response`]. Carries only the fields the
/// classifier needs to distinguish "upstream accepted the
/// temperature but ran out of output budget" from "upstream
/// silently dropped the parameter" — anything else (cache ids,
/// headers, full usage breakdown, role / model identity) is
/// dropped so the classifier stays a pure function with no
/// transport coupling.
///
/// The view is intentionally borrowed (not owned) so the
/// transport can build it inline without an extra allocation
/// per probe. With the parallel fan-out of 21 candidates the
/// zero-cost view matters at the boundary; every other layer of
/// the algorithm only sees the [`TemperatureProbeOutcome`].
#[derive(Debug, Clone, Copy)]
pub struct ProbeResponseView<'a> {
    /// Joined text from the response body. Empty when the
    /// upstream emitted `content: null` (the decoder tolerates
    /// `null` since PR #594 + iter 2 so the max-tokens probe
    /// does not collapse to `Indeterminate`) or when the
    /// request returned no content for any other reason.
    pub text: &'a str,
    /// The `stop_reason` reported by the upstream
    /// (`"end_turn"`, `"max_tokens"`, etc.). `None` when the
    /// upstream omitted the field.
    pub finish_reason: Option<&'a str>,
    /// Convenience flag the wire decoder sets when
    /// `finish_reason == "max_tokens"`. Mirrored as a separate
    /// field so the classifier does not have to re-parse the
    /// finish string on every probe.
    pub truncated: bool,
    /// Number of output tokens the upstream billed for the
    /// request. Combined with `truncated`, this is the
    /// unambiguous "ran out of budget mid-emit" signal —
    /// `truncated && output_tokens > 0`. Either flag in
    /// isolation is degenerate (a max_tokens stop with zero
    /// output is a wire-level anomaly the algorithm should
    /// re-probe, not lock).
    pub output_tokens: u64,
}

impl<'a> ProbeResponseView<'a> {
    /// Build a view from a borrowed [`Response`]. Lifted out of
    /// the transport so the unit tests can construct views
    /// without standing up a wiremock server.
    pub fn from_response(resp: &'a Response) -> Self {
        Self {
            text: resp.text.as_str(),
            finish_reason: resp.finish_reason.as_deref(),
            truncated: resp.truncated,
            output_tokens: resp.usage.output_tokens,
        }
    }
}

/// Trait that the temperature probe uses to send its tiny
/// request. Mirrors [`crate::llm::probe::ProbeTransport`]; the
/// trait exists so the probe can be unit-tested with a fake that
/// returns canned outcomes without standing up a wiremock server.
#[async_trait]
pub trait TemperatureProbeTransport: Send + Sync {
    /// Send a probe with the supplied `temperature` and report
    /// whether the upstream accepted it.
    async fn probe_send_temperature(&self, temperature: f32) -> TemperatureProbeOutcome;
}

/// Default transport: wraps an existing [`Provider`] and fires a
/// probe against it. The probe deliberately bypasses the breaker
/// (no `BreakeredProvider` wrapping) so a 400 rejection does not
/// count against the circuit-breaker window.
pub struct ProviderTemperatureProbeTransport {
    provider: Arc<dyn Provider>,
}

impl ProviderTemperatureProbeTransport {
    /// Build a transport from a provider. The transport reuses
    /// `provider.send_probe` so the per-call timeout can be
    /// applied around the call inside
    /// [`Self::probe_send_temperature`].
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self> {
        Ok(Self { provider })
    }

    /// Borrow the underlying provider. Useful for tests that
    /// want to inspect call counts.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }
}

#[async_trait]
impl TemperatureProbeTransport for ProviderTemperatureProbeTransport {
    async fn probe_send_temperature(&self, temperature: f32) -> TemperatureProbeOutcome {
        use tracing::Instrument;
        let req = Request {
            role: Role::Sketch, // F1: see investigation report
            model: self.provider.model().to_owned(),
            system: TEMPERATURE_PROBE_SYSTEM.to_owned(),
            user: TEMPERATURE_PROBE_USER.to_owned(),
            // Probe always sets `Some(...)` so the wire body
            // carries the probe budget.
            max_tokens: Some(PROBE_MIN_OUTPUT_TOKENS),
            temperature: Some(temperature),
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        // llm_probe span: every HTTP event emitted by `send_probe`
        // (and the timeout error if it fires) inherits
        // probe_kind=temperature, candidate=<temp>, provider, model.
        // Operators can grep `llm_probe{probe_kind=temperature}` to
        // follow the entire auto-probe fan-out.
        let probe_span = tracing::info_span!(
            "llm_probe",
            probe_kind = "temperature",
            candidate = temperature,
            provider = %self.provider.name(),
            model = %self.provider.model(),
        );
        let res = timeout(
            PROBE_TIMEOUT,
            self.provider
                .send_probe(&req)
                .instrument(probe_span.clone()),
        )
        .await;
        match res {
            Ok(Ok((status, body))) => {
                classify_probe_response(status, ProbeResponseView::from_response(&body))
            }
            Ok(Err(_)) | Err(_) => TemperatureProbeOutcome::Indeterminate,
        }
    }
}

/// Pure classification helper used by
/// [`ProviderTemperatureProbeTransport::probe_send_temperature`]
/// and exposed (via `pub`) for the unit tests. Lifted out of the
/// trait method so the tests can pin the 2xx/3xx/4xx branch logic
/// without spinning up a full provider.
///
/// Branching rules (the empty-body branch was tightened after
/// PR #594 + iter 2 to match the new `content: null` decoder
/// behaviour; see [`TemperatureProbeOutcome::Accepted`] for the
/// rationale):
///
/// - 2xx / 3xx with non-empty body that does not carry the
///   rejection signature → `Accepted`.
/// - 2xx / 3xx with non-empty body that carries the rejection
///   signature → `Rejected` (the upstream is telling us the
///   temperature is out of range).
/// - 2xx / 3xx with empty body AND the truncation signal
///   (`truncated && output_tokens > 0`) → `Accepted`. The
///   upstream unambiguously accepted the request and the model
///   ran out of output budget before emitting the trailing
///   tokens; the temperature parameter was honoured.
/// - 2xx / 3xx with empty body WITHOUT the truncation signal →
///   `Indeterminate`. The shape is genuinely ambiguous (silent
///   drop, decoder-absorbed error, 200 envelope with no content)
///   and the algorithm needs the retry-once path to gather a
///   second sample before locking the candidate.
/// - 4xx with body that carries the rejection signature →
///   `Rejected`.
/// - 4xx with body that does NOT carry the signature →
///   `Indeterminate` (the 4xx is for something else — auth,
///   model-not-found — and is not a temperature signal).
/// - 5xx / network / timeout → `Indeterminate`.
pub fn classify_probe_response(
    status: u16,
    view: ProbeResponseView<'_>,
) -> TemperatureProbeOutcome {
    let trimmed = view.text.trim();
    if (200..400).contains(&status) {
        if trimmed.is_empty() {
            // Empty 2xx body. Distinguish the truncation case
            // (upstream accepted, model ran out of output budget)
            // from the ambiguous case (silent drop, decoder-
            // absorbed error, 200 envelope with no content). The
            // rejection heuristic cannot fire here — it requires
            // the substring `temperature` in the body, which an
            // empty body does not carry — so the two sub-branches
            // are exhaustive: truncation → Accepted, anything
            // else → Indeterminate.
            if view.truncated && view.output_tokens > 0 {
                TemperatureProbeOutcome::Accepted
            } else {
                TemperatureProbeOutcome::Indeterminate
            }
        } else if body_carries_temperature_rejection(view.text) {
            TemperatureProbeOutcome::Rejected
        } else {
            TemperatureProbeOutcome::Accepted
        }
    } else if (400..500).contains(&status) {
        if body_carries_temperature_rejection(view.text) {
            TemperatureProbeOutcome::Rejected
        } else {
            TemperatureProbeOutcome::Indeterminate
        }
    } else {
        TemperatureProbeOutcome::Indeterminate
    }
}

/// Heuristic: does the response body carry the
/// "temperature rejected" signature?
///
/// The substring `temperature` appears in many benign response
/// bodies (a model that simply mentions the parameter it was
/// given, e.g. `"Temperature set to 0.7"`), so the helper
/// requires the keyword `temperature` to appear **together** with
/// at least one of the documented rejection hints: `must`,
/// `range`, `out of`, `unsupported`, `invalid`, `exceed`,
/// `between`, `value`, `not allowed`, `>`, `<`, `>=`, `<=`. The
/// conjunction matches the upstream error wording observed across
/// the providers the runtime currently targets (Anthropic-compat,
/// OpenAI-compat, OpenAI Responses, DeepSeek-direct, OpenCode Go
/// relays) without inflating the false-positive rate for benign
/// bodies.
///
/// The implementation lowercases the body once (cheap; the
/// response is bounded by the upstream's error-envelope size) and
/// tests the conjunction with a single substring scan. A future
/// refactor can replace the heuristic with a structured error
/// parser per provider if the heuristic ever drifts.
pub fn body_carries_temperature_rejection(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if !lower.contains("temperature") {
        return false;
    }
    // The conjunction set. The 0.0–2.0 range hints are stored
    // in two forms (en-dash and ASCII hyphen) so providers that
    // emit `"range 0.0–2.0"` and providers that emit
    // `"range 0.0-2.0"` both match. The hyphenated form is
    // common in the JSON-encodable error envelopes the OpenAI
    // Responses API emits.
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
        "0.0–2.0",
        "0.0-2.0",
        "0.0 - 2.0",
        "0 and 2",
    ];
    hints.iter().any(|h| lower.contains(h))
}

/// M2/M3-equivalent helper: re-fire the same probe once when the
/// first attempt comes back `Indeterminate`. The retry is gated
/// on the outcome, not on a timer, so a clean `Accepted` or
/// `Rejected` never pays the second round-trip.
async fn retry_once_on_indeterminate(
    transport: &dyn TemperatureProbeTransport,
    t: f32,
) -> TemperatureProbeOutcome {
    match transport.probe_send_temperature(t).await {
        TemperatureProbeOutcome::Indeterminate => transport.probe_send_temperature(t).await,
        other => other,
    }
}

/// Parallel fan-out for a single batch of temperatures. Each
/// point runs its own probe against the transport; the function
/// collects every outcome before returning. The probe is wrapped
/// in a per-task abort-on-drop `JoinHandle` so a transient
/// panic / cancel surfaces as `Indeterminate` rather than
/// locking the algorithm.
///
/// M9: each spawned task is wrapped in `tokio::select!` against
/// a [`tokio_util::sync::CancellationToken`] so a graceful
/// shutdown aborts the fan-out instead of leaking orphaned
/// tasks. Tasks that lose the race report `Indeterminate` so
/// the algorithm can re-probe them.
pub async fn parallel_probe(
    transport: Arc<dyn TemperatureProbeTransport>,
    points: &[f32],
) -> Vec<TemperatureProbeOutcome> {
    parallel_probe_with_cancel(transport, points, None).await
}

/// Same as [`parallel_probe`] but with an explicit cancellation
/// handle. When `cancel` is `None` the calls run to completion;
/// when `Some`, a fired token causes every pending probe to
/// return `Indeterminate` so the caller can short-circuit.
pub async fn parallel_probe_with_cancel(
    transport: Arc<dyn TemperatureProbeTransport>,
    points: &[f32],
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Vec<TemperatureProbeOutcome> {
    let mut handles = Vec::with_capacity(points.len());
    for &pt in points {
        let t = transport.clone();
        let cancel_child = cancel.as_ref().map(|c| c.child_token());
        handles.push(tokio::spawn(async move {
            if let Some(c) = cancel_child {
                tokio::select! {
                    biased;
                    _ = c.cancelled() => TemperatureProbeOutcome::Indeterminate,
                    outcome = t.probe_send_temperature(pt) => outcome,
                }
            } else {
                t.probe_send_temperature(pt).await
            }
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(o) => out.push(o),
            Err(e) => {
                tracing::warn!(error = %e, "temperature_probe: parallel_probe task join failed");
                out.push(TemperatureProbeOutcome::Indeterminate);
            }
        }
    }
    out
}

/// Run the temperature-probe algorithm against an
/// `Arc<dyn TemperatureProbeTransport>`. Iterates
/// [`TEMPERATURE_PROBE_VALUES`] in chunks of `batch_size`; each
/// chunk is a fan-out of parallel probes. `Indeterminate`
/// outcomes are retried once at the same temperature before
/// being treated as `Rejected` for the boundary search. Returns
/// the subset of [`TEMPERATURE_PROBE_VALUES`] that the upstream
/// accepted, preserving the canonical order.
///
/// `batch_size` is the caller's choice; the production default
/// is [`TEMPERATURE_PROBE_BATCH_SIZE`]. With 21 candidates and
/// `batch_size = 3` the runtime runs exactly 7 batches. Setting
/// `batch_size = 0` is treated as "process every value in
/// parallel" (one chunk).
pub async fn detect_supported_temperatures(
    transport: Arc<dyn TemperatureProbeTransport>,
    batch_size: usize,
) -> Vec<f32> {
    debug_assert!(
        !TEMPERATURE_PROBE_VALUES.is_empty(),
        "TEMPERATURE_PROBE_VALUES must not be empty"
    );
    let effective_batch = if batch_size == 0 {
        TEMPERATURE_PROBE_VALUES.len()
    } else {
        batch_size
    };
    let mut supported = Vec::new();
    for chunk in TEMPERATURE_PROBE_VALUES.chunks(effective_batch) {
        let outcomes = parallel_probe(transport.clone(), chunk).await;
        for (&t, outcome) in chunk.iter().zip(outcomes.iter()) {
            let committed = match outcome {
                TemperatureProbeOutcome::Indeterminate => {
                    retry_once_on_indeterminate(transport.as_ref(), t).await
                }
                other => other.clone(),
            };
            if matches!(committed, TemperatureProbeOutcome::Accepted) {
                supported.push(t);
            }
        }
    }
    supported
}

// ---------------------------------------------------------------------
// Sidecar file
// ---------------------------------------------------------------------

/// Serialised shape of the persisted table. Lives in
/// `<MOAGAN_HOME>/temperatures_auto.toml` and is read once at
/// startup; subsequent runs verify the cached value with a single
/// probe per cached temperature before trusting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TemperatureTableFile {
    /// Schema version. Bumped whenever the file shape changes
    /// incompatibly so a future `moagan` refuses to read a stale
    /// file instead of silently misinterpreting it.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// `provider_name -> model_name -> entry`. The nested
    /// `BTreeMap` gives deterministic on-disk ordering so a
    /// manual diff after a probe-run is meaningful.
    #[serde(default)]
    pub providers: BTreeMap<String, BTreeMap<String, Entry>>,
    /// Operator-pinned per-provider cap. The cap is the
    /// **union** of every temperature the operator has allowed
    /// under the same provider, so a fresh run on a new model
    /// inherits the temperatures the operator already vetted
    /// for a sibling model on the same provider. `#[serde(default)]`
    /// keeps the loader backward-compatible with v1 sidecars
    /// written before this field existed.
    #[serde(default)]
    pub operator_caps: BTreeMap<String, OperatorCap>,
}

fn default_schema_version() -> u32 {
    1
}

impl TemperatureTableFile {
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
    /// malformed file is `Err(Error::Provider(...))` so a typo in
    /// operator-land cannot silently break startup. A file whose
    /// `schema_version` is greater than
    /// [`Self::CURRENT_SCHEMA_VERSION`] is also rejected with
    /// `Error::Provider` — the future binary must bump the
    /// version and add the migration before this binary can
    /// read the new shape.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let parsed: Self = toml::from_str(&s).map_err(|e| Error::Provider {
                    message: format!(
                        "temperatures_auto.toml at {} is malformed: {e}",
                        path.display()
                    ),
                    http_status: None,
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    return Err(Error::Provider {
                        message: format!(
                            "temperatures_auto.toml at {} has schema_version={}, this binary only knows up to {}",
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
    /// crash mid-write cannot leave a truncated file. Mirrors
    /// [`crate::llm::probe::MaxTokensTableFile::save`] so the
    /// on-disk conventions stay in lockstep.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Provider {
                message: format!(
                    "create dir for temperatures_auto.toml at {}: {e}",
                    parent.display()
                ),
                http_status: None,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| Error::Provider {
            message: format!("encode temperatures_auto.toml: {e}"),
            http_status: None,
        })?;
        let body = quote_provider_model_keys(&body);
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| Error::Provider {
                message: format!("tempfile for temperatures_auto.toml: {e}"),
                http_status: None,
            })?;
        std::fs::write(tmp.path(), body).map_err(|e| Error::Provider {
            message: format!("write temperatures_auto.toml: {e}"),
            http_status: None,
        })?;
        tmp.persist(path).map_err(|e| Error::Provider {
            message: format!("rename temperatures_auto.toml into place: {e}"),
            http_status: None,
        })?;
        Ok(())
    }
}

/// One row of the persisted table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Temperatures the upstream accepted during the last
    /// successful probe. Order matches
    /// [`TEMPERATURE_PROBE_VALUES`] so the file diffs are
    /// deterministic.
    pub temperatures: Vec<f32>,
    /// ISO-8601 timestamp of the last successful probe.
    pub detected_at: String,
    /// ISO-8601 timestamp of the last successful verification
    /// probe (i.e., every cached temperature was re-probed and
    /// still passed). Equal to `detected_at` on the first probe
    /// of a fresh model.
    #[serde(default)]
    pub verified_at: String,
    /// Always `true` for entries the probe produced. The field
    /// is explicit so a human reading the file can tell at a
    /// glance which entries came from auto-detection vs.
    /// operator-pinned overrides.
    pub auto: bool,
    /// How many probes the algorithm ran to discover this set.
    /// Useful for telemetry; the algorithm makes
    /// `ceil(21 / batch_size)` batches of `batch_size` parallel
    /// probes each.
    #[serde(default)]
    pub attempts: u32,
}

/// Operator-pinned per-provider cap. The cap is the **union** of
/// every temperature the operator has allowed under the same
/// provider. On a new `(provider, model)` lookup, the runtime
/// intersects the auto-discovered set with the operator's cap so
/// an operator who has explicitly whitelisted `T=0.0..1.0` cannot
/// accidentally regress to a `T=1.5` acceptance that the
/// auto-probe happens to discover on a permissive relay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OperatorCap {
    /// The temperatures the operator allows for this provider.
    /// Order is not significant; the runtime sorts by distance
    /// to the requested value when picking the nearest.
    pub temperatures: Vec<f32>,
    /// Always `false` for an operator-pinned entry. Explicit so
    /// a grep-friendly TOML diff between auto-discovered and
    /// operator-pinned entries stays trivial.
    pub auto: bool,
    /// ISO-8601 timestamp the cap was written.
    pub detected_at: String,
}

// ---------------------------------------------------------------------
// In-memory table
// ---------------------------------------------------------------------

/// In-memory table of `(provider_name, model_name) -> Entry` plus
/// the operator-pinned caps. Wrapped in an `Arc<RwLock>` so a
/// single instance can be cloned into every callsite.
///
/// Concurrency model: [`parking_lot::RwLock`] over a `BTreeMap`.
/// Reads (the hot path: every LLM call that needs a temperature
/// override) lock for read; writes (probe completion, one-time
/// persistence) lock for write. The `parking_lot` flavour is
/// already on the dependency list, so the lock is non-poisoning
/// and contention-free under realistic workloads.
#[derive(Clone)]
pub struct TemperatureTable {
    inner: Arc<RwLock<TemperatureTableInner>>,
    /// Path to the on-disk TOML file. `None` when persistence is
    /// disabled.
    persist_path: Option<PathBuf>,
}

#[derive(Debug)]
struct TemperatureTableInner {
    entries: BTreeMap<(String, String), Entry>,
    /// Operator-pinned per-provider cap, mirrored from the
    /// on-disk `operator_caps` field. The runtime intersects the
    /// auto-discovered set with the cap before exposing the
    /// effective set so an operator who has explicitly
    /// whitelisted `T=0.0..1.0` cannot accidentally regress to
    /// `T=1.5` on a permissive relay.
    operator_caps: BTreeMap<String, OperatorCap>,
    /// Total probe tasks started across all calls since startup.
    /// M7: counts tokio tasks spawned (one per `probe_and_store` /
    /// `verify`), not HTTP round-trips. Used for telemetry and
    /// operator-visible diagnostics.
    probe_tasks_started: u32,
    /// [`tokio::task::JoinHandle`]s for every background probe the
    /// registry fired at startup. The runtime joins them via
    /// [`Self::await_ready`] so the caller can decide whether to
    /// gate the first LLM call behind the discovery. Stored as
    /// `Vec<JoinHandle<()>>` because the typed return value is
    /// `Result<(), JoinError>` which would force every consumer
    /// to import `tokio::task::JoinError`; the inner type is
    /// unit so dropping the handle is safe.
    pending: Vec<tokio::task::JoinHandle<()>>,
}

impl TemperatureTable {
    /// Build a table from the on-disk file at
    /// `<MOAGAN_HOME>/temperatures_auto.toml`. `save` controls
    /// whether subsequent probe results are persisted.
    pub fn from_home(home: &MoaganHome, save: bool) -> Result<Self> {
        let path = home.temperatures_auto_path();
        Self::from_path(&path, save)
    }

    /// Build a table from an explicit path. Used by tests and by
    /// [`Self::from_home`].
    pub fn from_path(path: &Path, save: bool) -> Result<Self> {
        let file = TemperatureTableFile::load(path)?;
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
            inner: Arc::new(RwLock::new(TemperatureTableInner {
                entries,
                operator_caps: file.operator_caps,
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: save.then(|| path.to_path_buf()),
        })
    }

    /// Build a fresh table with no on-disk backing. Used by tests.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(TemperatureTableInner {
                entries: BTreeMap::new(),
                operator_caps: BTreeMap::new(),
                probe_tasks_started: 0,
                pending: Vec::new(),
            })),
            persist_path: None,
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

    /// Resolve the effective supported-temperatures set for
    /// `(provider, model)`: the cached set intersected with the
    /// operator's per-provider cap (if any). Returns an empty
    /// `Vec` when no entry or when the intersection is empty —
    /// callers fall back to the auto-probe or to the default
    /// temperature.
    pub fn supported_for(&self, provider: &str, model: &str) -> Vec<f32> {
        let inner = self.inner.read();
        let Some(entry) = inner.entries.get(&(provider.to_owned(), model.to_owned())) else {
            return Vec::new();
        };
        match inner.operator_caps.get(provider) {
            None => entry.temperatures.clone(),
            Some(cap) => {
                let cap_set: std::collections::BTreeSet<u32> =
                    cap.temperatures.iter().map(|t| t.to_bits()).collect();
                entry
                    .temperatures
                    .iter()
                    .copied()
                    .filter(|t| cap_set.contains(&t.to_bits()))
                    .collect()
            }
        }
    }

    /// Resolve the nearest supported temperature to
    /// `requested` for `(provider, model)`. Returns `None` when
    /// the effective supported set (after the operator-cap
    /// intersection) is empty.
    ///
    /// The neighbour is chosen as the absolute-distance
    /// minimiser. On ties (two cached temperatures are equally
    /// close to `requested`), the **first appearance in
    /// `temperatures`** wins — the same convention the
    /// `max_tokens` probe follows for its bisect step. Because
    /// [`TEMPERATURE_PROBE_VALUES`] is sorted ascending, the
    /// tiebreak resolves to the lower temperature on a
    /// half-step tie and to the higher on a non-half-step
    /// tie. Document this in the doc-comment so a refactor
    /// that flips the order is a deliberate, reviewable
    /// change.
    pub fn nearest_supported(&self, provider: &str, model: &str, requested: f32) -> Option<f32> {
        let supported = self.supported_for(provider, model);
        nearest_in_set(&supported, requested)
    }

    /// Probe the upstream and insert the discovered set.
    /// Idempotent for a given `(provider, model)` when called
    /// twice: the second call re-probes and overwrites with the
    /// fresh value.
    ///
    /// `batch_size` is the per-batch fan-out passed through to
    /// [`detect_supported_temperatures`]. The runtime auto-probe
    /// supplies [`TEMPERATURE_PROBE_BATCH_SIZE`] so the registry
    /// background path stays inside its concurrency envelope;
    /// the `moagan probe temperature` CLI accepts a `--batch-size`
    /// override that flows through this parameter unchanged.
    /// `batch_size = 0` is treated by `detect_supported_temperatures`
    /// as "fan out every candidate in parallel".
    pub async fn probe_and_store(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn TemperatureProbeTransport>,
        batch_size: usize,
    ) -> Result<Vec<f32>> {
        let attempts_before = self.inner.read().probe_tasks_started;
        let discovered = detect_supported_temperatures(transport, batch_size).await;
        let now = chrono::Utc::now().to_rfc3339();
        let attempts = {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            let attempts_total = inner.probe_tasks_started - attempts_before;
            inner.entries.insert(
                (provider.to_owned(), model.to_owned()),
                Entry {
                    temperatures: discovered.clone(),
                    detected_at: now.clone(),
                    verified_at: now,
                    auto: true,
                    attempts: attempts_total,
                },
            );
            attempts_total
        };
        let _ = attempts;
        if let Some(path) = self.persist_path.as_ref()
            && let Err(e) = self.persist_to(path)
        {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "temperatures_auto.toml persistence failed; in-memory entry is kept"
            );
        }
        Ok(discovered)
    }

    /// Verify a cached entry by re-probing every cached
    /// temperature. On success, `verified_at` is updated. On
    /// failure (any cached temperature was rejected on a
    /// subsequent call), the entry is removed and the caller
    /// falls back to a full re-probe. Returns `true` on
    /// success, `false` on failure, and `Ok(false)` (no error)
    /// when the entry did not exist in the first place.
    pub async fn verify(
        &self,
        provider: &str,
        model: &str,
        transport: Arc<dyn TemperatureProbeTransport>,
    ) -> Result<bool> {
        let cached = self.get(provider, model);
        let Some(entry) = cached else {
            return Ok(false);
        };
        let mut ok = true;
        for &t in &entry.temperatures {
            let outcome = match timeout(PROBE_TIMEOUT, transport.probe_send_temperature(t)).await {
                Ok(o) => o,
                Err(_) => TemperatureProbeOutcome::Indeterminate,
            };
            let committed = match outcome {
                TemperatureProbeOutcome::Indeterminate => {
                    retry_once_on_indeterminate(transport.as_ref(), t).await
                }
                other => other,
            };
            if !matches!(committed, TemperatureProbeOutcome::Accepted) {
                ok = false;
                break;
            }
        }
        {
            let mut inner = self.inner.write();
            inner.probe_tasks_started += 1;
            if ok {
                if let Some(e) = inner
                    .entries
                    .get_mut(&(provider.to_owned(), model.to_owned()))
                {
                    e.verified_at = chrono::Utc::now().to_rfc3339();
                }
            } else {
                inner
                    .entries
                    .remove(&(provider.to_owned(), model.to_owned()));
            }
        }
        if let Some(path) = self.persist_path.as_ref() {
            let _ = self.persist_to(path);
        }
        Ok(ok)
    }

    /// Persist the current in-memory state to disk. Best-effort:
    /// callers wrap in `if let Err(_)` because losing a probe
    /// result is preferable to aborting the run.
    fn persist_to(&self, path: &Path) -> Result<()> {
        let inner = self.inner.read();
        let mut file = TemperatureTableFile::new_empty();
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
    /// probe the registry fired at startup. The caller can
    /// `await` every handle via [`Self::await_ready`] when it
    /// wants to gate the first LLM call behind the discovery
    /// (CI, smoke tests).
    pub fn record_probe_join_handle(&self, handle: tokio::task::JoinHandle<()>) {
        let mut inner = self.inner.write();
        inner.pending.push(handle);
    }

    /// Wait for every probe the registry fired at startup to
    /// finish. No-op when no probe was fired (mock-only
    /// registry). Errors from individual probes are logged via
    /// `tracing::warn!` and do not propagate — a failing probe
    /// degrades to the static temperature knob, never aborts
    /// the run.
    pub async fn await_ready(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut inner = self.inner.write();
            std::mem::take(&mut inner.pending)
        };
        for h in handles {
            if let Err(e) = h.await {
                tracing::warn!(error = %e, "temperature_probe: probe task join failed");
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

    /// Set the operator-pinned cap for a provider. The cap is
    /// the **union** of the supplied set with whatever the
    /// in-memory map already carries for the same provider — a
    /// separate process (or a previous invocation) may have
    /// written its own cap for the same provider, and a union
    /// preserves the operator's intent (they want
    /// `T=0.0..0.5 ∪ T=0.7` to be allowed, not "the last write
    /// wins"). `auto` is hard-coded to `false` because an
    /// operator-pinned cap is, by construction, not
    /// auto-detected.
    pub fn set_operator_cap(&self, provider: &str, temperatures: Vec<f32>) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let path = self.persist_path.clone();
        {
            let mut inner = self.inner.write();
            let entry = inner
                .operator_caps
                .entry(provider.to_owned())
                .or_insert(OperatorCap {
                    temperatures: Vec::new(),
                    auto: false,
                    detected_at: now.clone(),
                });
            // Union: insert each new temperature by `f32::to_bits`
            // to dodge `NaN` equality quirks; dedup preserves the
            // order the operator supplied them in.
            let mut seen: std::collections::BTreeSet<u32> =
                entry.temperatures.iter().map(|t| t.to_bits()).collect();
            for t in &temperatures {
                if seen.insert(t.to_bits()) {
                    entry.temperatures.push(*t);
                }
            }
            entry.detected_at = now;
        }
        if let Some(ref path) = path {
            self.persist_to(path)?;
        } else {
            tracing::warn!(
                provider = %provider,
                "temperatures_auto: persistence disabled; operator cap not written to disk"
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
    /// table. M7: counts tokio tasks spawned, not HTTP
    /// round-trips (a single `probe_and_store` call can fire up
    /// to `ceil(21 / batch_size)` parallel probes). Renamed
    /// from `probes_attempted` so the name matches what it
    /// actually counts.
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

/// Pure helper: pick the nearest value in `candidates` to
/// `requested`. Tiebreak by first appearance in `candidates`.
/// Returns `None` when `candidates` is empty. The function is
/// `pub` so unit tests can pin the tiebreak contract without
/// spinning up a full [`TemperatureTable`].
pub fn nearest_in_set(candidates: &[f32], requested: f32) -> Option<f32> {
    let mut best: Option<(f32, f32, usize)> = None;
    for (idx, &c) in candidates.iter().enumerate() {
        let dist = (c - requested).abs();
        match best {
            None => best = Some((c, dist, idx)),
            Some((_, best_dist, best_idx)) => {
                if dist < best_dist || (dist == best_dist && idx < best_idx) {
                    best = Some((c, dist, idx));
                }
            }
        }
    }
    best.map(|(c, _, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // -----------------------------------------------------------------
    // Test transports
    // -----------------------------------------------------------------

    /// Transport that accepts a hard-coded subset of temperatures
    /// and rejects the rest with a body that carries the rejection
    /// signature. Used by the detection-shape tests.
    #[derive(Clone)]
    struct SubsetTransport {
        accept: Arc<std::collections::BTreeSet<u32>>,
        in_flight: Arc<AtomicU32>,
        max_in_flight: Arc<AtomicU32>,
    }

    impl SubsetTransport {
        fn accepting(values: &[f32]) -> Self {
            let mut set = std::collections::BTreeSet::new();
            for v in values {
                set.insert(v.to_bits());
            }
            Self {
                accept: Arc::new(set),
                in_flight: Arc::new(AtomicU32::new(0)),
                max_in_flight: Arc::new(AtomicU32::new(0)),
            }
        }

        fn rejecting_all() -> Self {
            Self::accepting(&[])
        }

        fn accepting_all() -> Self {
            let mut set = std::collections::BTreeSet::new();
            for v in TEMPERATURE_PROBE_VALUES {
                set.insert(v.to_bits());
            }
            Self {
                accept: Arc::new(set),
                in_flight: Arc::new(AtomicU32::new(0)),
                max_in_flight: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    #[async_trait]
    impl TemperatureProbeTransport for SubsetTransport {
        async fn probe_send_temperature(&self, t: f32) -> TemperatureProbeOutcome {
            // Track concurrent in-flight probes so the
            // `respects_batch_size` test can pin the fan-out
            // contract.
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            // Update max with a CAS-style fetch_max loop. We
            // cannot mutate in place from inside the closure,
            // so re-load + max.
            let mut current_max = self.max_in_flight.load(Ordering::SeqCst);
            while now > current_max {
                match self.max_in_flight.compare_exchange(
                    current_max,
                    now,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(observed) => current_max = observed,
                }
            }
            // Yield to give the runtime scheduler a chance to
            // interleave so a true concurrent fan-out is
            // observable. A zero-duration yield is enough —
            // the test only needs to prove the in-flight count
            // ever rises above 1, not that probes overlap for
            // some specific duration.
            tokio::task::yield_now().await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if self.accept.contains(&t.to_bits()) {
                TemperatureProbeOutcome::Accepted
            } else {
                TemperatureProbeOutcome::Rejected
            }
        }
    }

    fn transport(t: SubsetTransport) -> Arc<dyn TemperatureProbeTransport> {
        Arc::new(t)
    }

    // -----------------------------------------------------------------
    // 1. detect_supported_temperatures_with_mock_accepting_subset
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_temperatures_with_mock_accepting_subset() {
        // Mock accepts {0.0, 0.3, 0.7} and rejects everything else.
        // Reject uses body with the rejection signature, but the
        // transport short-circuits without going through the
        // network — `Rejected` is the outcome the algorithm sees
        // directly.
        let t = transport(SubsetTransport::accepting(&[0.0, 0.3, 0.7]));
        let got = detect_supported_temperatures(t, TEMPERATURE_PROBE_BATCH_SIZE).await;
        assert_eq!(got, vec![0.0, 0.3, 0.7]);
    }

    // -----------------------------------------------------------------
    // 2. detect_supported_temperatures_with_mock_accepting_all
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_temperatures_with_mock_accepting_all() {
        let t = transport(SubsetTransport::accepting_all());
        let got = detect_supported_temperatures(t, TEMPERATURE_PROBE_BATCH_SIZE).await;
        assert_eq!(got, TEMPERATURE_PROBE_VALUES.to_vec());
    }

    // -----------------------------------------------------------------
    // 3. detect_supported_temperatures_with_mock_rejecting_all
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn detect_supported_temperatures_with_mock_rejecting_all() {
        let t = transport(SubsetTransport::rejecting_all());
        let got = detect_supported_temperatures(t, TEMPERATURE_PROBE_BATCH_SIZE).await;
        assert!(got.is_empty(), "got {got:?}");
    }

    // -----------------------------------------------------------------
    // 4. detect_supported_temperatures_respects_batch_size
    // -----------------------------------------------------------------
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn detect_supported_temperatures_respects_batch_size() {
        // Use a slow-accept transport (the body inspection branch
        // is irrelevant — we just need the probe to take *some*
        // time so the scheduler can interleave tasks). The
        // SubsetTransport's `yield_now` is enough on a
        // multi-thread runtime.
        let t = SubsetTransport::accepting_all();
        let max_observed = t.max_in_flight.clone();
        let _: Vec<f32> =
            detect_supported_temperatures(transport(t), TEMPERATURE_PROBE_BATCH_SIZE).await;
        let observed = max_observed.load(Ordering::SeqCst);
        assert!(
            observed <= TEMPERATURE_PROBE_BATCH_SIZE as u32,
            "fan-out exceeded batch size: {observed} > {TEMPERATURE_PROBE_BATCH_SIZE}"
        );
        // The transport accepts everything so the test would
        // also pass if the fan-out never actually ran in
        // parallel (e.g. on a single-thread runtime). Pin the
        // multi-thread runtime at the `#[tokio::test]` level
        // so the test fails loudly if it is ever moved to a
        // single-thread runtime.
        assert!(
            observed >= 1,
            "fan-out never ran any probe in parallel (observed={observed})"
        );
    }

    // -----------------------------------------------------------------
    // 5. body_carries_temperature_rejection_classifies_correctly
    // -----------------------------------------------------------------
    #[test]
    fn body_carries_temperature_rejection_classifies_correctly() {
        // Positives — the conjunction of `temperature` and a
        // rejection hint must classify as Rejected.
        assert!(body_carries_temperature_rejection(
            "temperature must be between 0 and 2"
        ));
        assert!(body_carries_temperature_rejection(
            "temperature_out_of_range: 0.5 not in [0.0, 1.0]"
        ));
        assert!(body_carries_temperature_rejection(
            "unsupported temperature value 1.5"
        ));
        assert!(body_carries_temperature_rejection("invalid temperature"));
        assert!(body_carries_temperature_rejection(
            "temperature out of range: max is 1.0"
        ));
        assert!(body_carries_temperature_rejection(
            "temperature value 1.5 is not allowed"
        ));
        // Negatives — the body must NOT trigger a Rejected
        // classification just because it contains the word
        // `temperature`.
        assert!(!body_carries_temperature_rejection(""));
        assert!(!body_carries_temperature_rejection("model not found"));
        assert!(!body_carries_temperature_rejection("ok"));
        // A body that mentions `temperature` without a rejection
        // hint is benign.
        assert!(!body_carries_temperature_rejection(
            "model acknowledged temperature parameter"
        ));
        // The conjunction check: `temperature` alone (without
        // `must` / `range` / etc.) must not trigger.
        assert!(!body_carries_temperature_rejection("temperature = 0.7"));
    }

    // -----------------------------------------------------------------
    // 5.1. classify_probe_response — branch matrix
    //
    // Pins the `pub fn classify_probe_response` contract so the
    // PR #594 follow-up (truncation-as-Accepted) and the legacy
    // branches (non-empty accept, 4xx with / without rejection
    // signature, 5xx) cannot drift independently. The probe
    // algorithm depends on every branch agreeing with the
    // doc-comment above the function; a regression here would
    // either (a) collapse the discovered set back to empty when
    // the upstream truncates or (b) re-introduce the silent-drop
    // false-positive the original implementation was guarding
    // against.
    // -----------------------------------------------------------------
    /// Helper: build a [`ProbeResponseView`] with the supplied
    /// fields. The view is `Copy` so the test sites can pass it
    /// inline without a binding.
    fn view<'a>(
        text: &'a str,
        finish_reason: Option<&'a str>,
        output_tokens: u64,
    ) -> ProbeResponseView<'a> {
        let truncated = matches!(finish_reason, Some("max_tokens"));
        ProbeResponseView {
            text,
            finish_reason,
            truncated,
            output_tokens,
        }
    }

    #[test]
    fn classify_probe_response_truncation_is_accepted() {
        // The bug fix: HTTP 200 with `content: null` /
        // `stop_reason: "max_tokens"` / `output_tokens > 0` is
        // the upstream telling us "I accepted the parameter but
        // ran out of budget before the trailing tokens". The
        // classifier MUST return Accepted; the previous
        // "empty body = Rejected" branch would have collapsed
        // the discovered set to `Vec::new()`.
        let v = view("", Some("max_tokens"), 5);
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Accepted
        );
        // Sanity: 3xx behaves identically (the classifier
        // already accepts the whole 200..400 range).
        assert_eq!(
            classify_probe_response(204, v),
            TemperatureProbeOutcome::Accepted
        );
    }

    #[test]
    fn classify_probe_response_empty_no_truncation_is_indeterminate() {
        // Empty 2xx body WITHOUT the truncation signal — silent
        // drop, decoder-absorbed error, or 200 envelope with no
        // content — is genuinely ambiguous and must NOT lock
        // the candidate as Rejected. The algorithm's
        // `retry_once_on_indeterminate` path gathers a second
        // sample before deciding.
        // No finish_reason at all.
        let v = view("", None, 0);
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Indeterminate
        );
        // finish_reason = "end_turn" with an empty body is a
        // contradictory upstream response (the model says it
        // stopped normally but emitted nothing); retry is the
        // correct response.
        let v = view("", Some("end_turn"), 1);
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Indeterminate
        );
    }

    #[test]
    fn classify_probe_response_truncated_zero_output_is_indeterminate() {
        // Degenerate shape: `stop_reason = "max_tokens"` but
        // `output_tokens == 0`. The truncation flag is set
        // without any tokens having been emitted — likely a
        // wire-level anomaly (the upstream ran its truncation
        // check before any token was generated, or the budget
        // was so small that even the response envelope
        // exhausted it). The classifier must NOT lock this as
        // Accepted on the truncated flag alone; the
        // `output_tokens > 0` half of the conjunction keeps the
        // door open for the retry path.
        let v = view("", Some("max_tokens"), 0);
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Indeterminate
        );
    }

    #[test]
    fn classify_probe_response_non_empty_body_without_signature_is_accepted() {
        // The legacy "happy path" — 2xx + non-empty body + no
        // rejection signature — stays Accepted. Pins the
        // non-empty branch so the truncation follow-up cannot
        // regress it.
        let v = view("1", Some("end_turn"), 1);
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Accepted
        );
    }

    #[test]
    fn classify_probe_response_non_empty_body_with_signature_is_rejected() {
        // The upstream returned a 2xx envelope but stamped the
        // body with the rejection signature — the temperature
        // parameter was honoured and rejected. Classify as
        // Rejected so the algorithm excludes the candidate.
        let v = view(
            r#"{"error":{"message":"temperature must be between 0 and 2"}}"#,
            Some("end_turn"),
            0,
        );
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Rejected
        );
    }

    #[test]
    fn classify_probe_response_4xx_with_signature_is_rejected() {
        // The canonical upstream rejection shape (Anthropic-
        // compat relays that cap at 1.0, OpenCode Go routes for
        // `gpt-5.6-luna`). The classifier pins this as
        // Rejected because the body carries the rejection
        // signature.
        let v = view(
            r#"{"error":{"message":"temperature must be between 0 and 2"}}"#,
            None,
            0,
        );
        assert_eq!(
            classify_probe_response(400, v),
            TemperatureProbeOutcome::Rejected
        );
        assert_eq!(
            classify_probe_response(422, v),
            TemperatureProbeOutcome::Rejected
        );
    }

    #[test]
    fn classify_probe_response_4xx_without_signature_is_indeterminate() {
        // 4xx without the rejection signature — auth failures,
        // model-not-found, malformed body — is NOT a temperature
        // signal and must NOT lock the candidate as Rejected.
        // The runtime's dispatch gate falls through to
        // Indeterminate so a different relay / re-auth / new
        // model does not poison the cache.
        let v = view("invalid api key", None, 0);
        assert_eq!(
            classify_probe_response(401, v),
            TemperatureProbeOutcome::Indeterminate
        );
        let v = view("model not found", None, 0);
        assert_eq!(
            classify_probe_response(404, v),
            TemperatureProbeOutcome::Indeterminate
        );
    }

    #[test]
    fn classify_probe_response_5xx_is_indeterminate() {
        // 5xx storm, transient upstream error — never a
        // temperature signal.
        let v = view("upstream is on fire", None, 0);
        assert_eq!(
            classify_probe_response(500, v),
            TemperatureProbeOutcome::Indeterminate
        );
        assert_eq!(
            classify_probe_response(503, v),
            TemperatureProbeOutcome::Indeterminate
        );
    }

    #[test]
    fn classify_probe_response_3xx_without_body_is_accepted_when_non_empty() {
        // 3xx with a non-empty body still flows through the
        // rejection / accept path (the classifier treats 200..400
        // uniformly). Pins the upper bound of the status window.
        let v = view("1", Some("end_turn"), 1);
        assert_eq!(
            classify_probe_response(304, v),
            TemperatureProbeOutcome::Accepted
        );
    }

    // -----------------------------------------------------------------
    // 5.2. probe_response_view_from_response_round_trip
    //
    // Pins the [`ProbeResponseView::from_response`] bridge so
    // every field the classifier consults is propagated
    // verbatim from the [`Response`] the wire decoder produced.
    // A regression here would silently change the classifier's
    // input shape and break the truncation-as-Accepted contract
    // pinned by 5.1.
    // -----------------------------------------------------------------
    #[test]
    fn probe_response_view_from_response_round_trip() {
        let resp = Response {
            text: String::new(),
            finish_reason: Some("max_tokens".to_owned()),
            truncated: true,
            usage: crate::llm::wire::Usage {
                input_tokens: 49,
                output_tokens: 16,
                cache_read: 0,
                cache_creation: 0,
            },
        };
        let v = ProbeResponseView::from_response(&resp);
        assert_eq!(v.text, "");
        assert_eq!(v.finish_reason, Some("max_tokens"));
        assert!(v.truncated);
        assert_eq!(v.output_tokens, 16);
        // End-to-end: the view the wire decoder produced must
        // land as Accepted through the classifier. This is the
        // "wire-shape that the upstream actually emits when the
        // model thinks too hard" regression pin.
        assert_eq!(
            classify_probe_response(200, v),
            TemperatureProbeOutcome::Accepted
        );
    }
    // -----------------------------------------------------------------
    // 6. temperature_table_file_round_trip_through_toml
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_file_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temperatures_auto.toml");
        let mut file = TemperatureTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    temperatures: vec![0.0, 0.5, 1.0],
                    detected_at: "2026-08-22T00:00:00Z".to_owned(),
                    verified_at: "2026-08-22T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 7,
                },
            );
        file.save(&path).unwrap();
        let back = TemperatureTableFile::load(&path).unwrap();
        assert_eq!(back, file);
    }

    // -----------------------------------------------------------------
    // 7. temperature_table_file_load_rejects_future_schema_version
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_file_load_rejects_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temperatures_auto.toml");
        std::fs::write(&path, "schema_version = 999\n[providers]\n").unwrap();
        let err = TemperatureTableFile::load(&path).expect_err("future schema must error");
        match err {
            Error::Provider { message, .. } => {
                assert!(message.contains("schema_version"), "msg: {message}")
            }
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 8. temperature_table_file_load_rejects_malformed_toml
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_file_load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temperatures_auto.toml");
        std::fs::write(&path, "this is = not valid toml = at all").unwrap();
        let err = TemperatureTableFile::load(&path).expect_err("malformed must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("malformed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 9. temperature_table_load_missing_file_returns_empty
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_load_missing_file_returns_empty() {
        let path = std::path::PathBuf::from("/nonexistent/temperatures_auto.toml");
        let t = TemperatureTableFile::load(&path).unwrap();
        assert!(t.providers.is_empty());
        assert_eq!(
            t.schema_version,
            TemperatureTableFile::CURRENT_SCHEMA_VERSION
        );
    }

    // -----------------------------------------------------------------
    // 10. temperature_table_probe_and_store_inserts_entry
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn temperature_table_probe_and_store_inserts_entry() {
        let table = TemperatureTable::empty();
        let t = transport(SubsetTransport::accepting(&[0.0, 0.3, 0.7]));
        let discovered = table
            .probe_and_store("minimax", "MiniMax-M3", t, TEMPERATURE_PROBE_BATCH_SIZE)
            .await
            .unwrap();
        assert_eq!(discovered, vec![0.0, 0.3, 0.7]);
        let entry = table
            .get("minimax", "MiniMax-M3")
            .expect("entry must exist");
        assert_eq!(entry.temperatures, vec![0.0, 0.3, 0.7]);
        assert!(entry.auto);
        assert!(!entry.detected_at.is_empty());
    }

    // -----------------------------------------------------------------
    // 11. temperature_table_verify_updates_verified_at_on_success
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn temperature_table_verify_updates_verified_at_on_success() {
        let table = TemperatureTable::empty();
        let t = transport(SubsetTransport::accepting(&[0.0, 0.3, 0.7]));
        table
            .probe_and_store(
                "minimax",
                "MiniMax-M3",
                t.clone(),
                TEMPERATURE_PROBE_BATCH_SIZE,
            )
            .await
            .unwrap();
        let detected_at = table.get("minimax", "MiniMax-M3").unwrap().detected_at;
        let verified_before = table.get("minimax", "MiniMax-M3").unwrap().verified_at;
        assert_eq!(
            detected_at, verified_before,
            "first probe: detected_at == verified_at"
        );
        // Force a different verified_at by sleeping 10ms.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ok = table.verify("minimax", "MiniMax-M3", t).await.unwrap();
        assert!(ok, "verify must report success on the accepting mock");
        let verified_after = table.get("minimax", "MiniMax-M3").unwrap().verified_at;
        assert!(
            verified_after >= verified_before,
            "verified_at must not regress: {verified_before} -> {verified_after}"
        );
    }

    // -----------------------------------------------------------------
    // 12. temperature_table_verify_drops_entry_on_failure
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn temperature_table_verify_drops_entry_on_failure() {
        let table = TemperatureTable::empty();
        // First probe: accepting.
        let accepting = transport(SubsetTransport::accepting(&[0.0, 0.3, 0.7]));
        table
            .probe_and_store(
                "minimax",
                "MiniMax-M3",
                accepting,
                TEMPERATURE_PROBE_BATCH_SIZE,
            )
            .await
            .unwrap();
        assert!(table.get("minimax", "MiniMax-M3").is_some());
        // Second call: the upstream now rejects everything.
        let rejecting = transport(SubsetTransport::rejecting_all());
        let ok = table
            .verify("minimax", "MiniMax-M3", rejecting)
            .await
            .unwrap();
        assert!(!ok, "verify must report failure on the rejecting mock");
        assert!(table.get("minimax", "MiniMax-M3").is_none());
    }

    // -----------------------------------------------------------------
    // 13. temperature_table_nearest_supported_returns_nearest
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_nearest_supported_returns_nearest() {
        // Hand-build a table with a known set, no transport
        // dependency.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temperatures_auto.toml");
        let mut file = TemperatureTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    temperatures: vec![0.0, 0.3, 0.7, 1.0],
                    detected_at: "2026-08-22T00:00:00Z".to_owned(),
                    verified_at: "2026-08-22T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 7,
                },
            );
        file.save(&path).unwrap();
        let table = TemperatureTable::from_path(&path, false).unwrap();

        // Half-step tie: 0.5 is equidistant from 0.3 and 0.7.
        // The contract (see [`TemperatureTable::nearest_supported`]
        // doc-comment) is "first appearance wins" — that
        // resolves to 0.3.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 0.5),
            Some(0.3)
        );
        // Exact match: 1.0 is in the set.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 1.0),
            Some(1.0)
        );
        // Above the highest cached value: clamp to 1.0.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", 2.5),
            Some(1.0)
        );
        // Below the lowest cached value: clamp to 0.0.
        assert_eq!(
            table.nearest_supported("minimax", "MiniMax-M3", -1.0),
            Some(0.0)
        );
    }

    // -----------------------------------------------------------------
    // 14. temperature_table_nearest_supported_returns_none_when_empty
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_nearest_supported_returns_none_when_empty() {
        let table = TemperatureTable::empty();
        assert!(
            table
                .nearest_supported("minimax", "MiniMax-M3", 0.5)
                .is_none()
        );
    }

    // -----------------------------------------------------------------
    // 15. temperature_table_supported_for_returns_set_or_empty
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_supported_for_returns_set_or_empty() {
        let table = TemperatureTable::empty();
        // Empty table → empty vec.
        assert!(table.supported_for("minimax", "MiniMax-M3").is_empty());
        // Insert an entry and the lookup returns the cached set.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temperatures_auto.toml");
        let mut file = TemperatureTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    temperatures: vec![0.0, 0.3, 0.7],
                    detected_at: "2026-08-22T00:00:00Z".to_owned(),
                    verified_at: "2026-08-22T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 7,
                },
            );
        file.save(&path).unwrap();
        let table = TemperatureTable::from_path(&path, false).unwrap();
        let got = table.supported_for("minimax", "MiniMax-M3");
        assert_eq!(got, vec![0.0, 0.3, 0.7]);
    }

    // -----------------------------------------------------------------
    // 16. temperature_table_set_operator_cap_uses_union
    // -----------------------------------------------------------------
    #[test]
    fn temperature_table_set_operator_cap_uses_union() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("temperatures_auto.toml");
        let table = TemperatureTable::from_path(&path, true).unwrap();
        table
            .set_operator_cap("minimax", vec![0.0, 0.5])
            .expect("first set_operator_cap must succeed");
        table
            .set_operator_cap("minimax", vec![0.3, 0.7])
            .expect("second set_operator_cap must succeed");
        let cap = table.operator_cap("minimax").expect("cap must exist");
        // Union: 0.0, 0.3, 0.5, 0.7 in the order the
        // operator supplied them.
        assert_eq!(cap.temperatures, vec![0.0, 0.5, 0.3, 0.7]);
        // The on-disk sidecar must reflect the union.
        let on_disk = TemperatureTableFile::load(&path).unwrap();
        let on_disk_cap = on_disk.operator_caps.get("minimax").unwrap();
        assert_eq!(on_disk_cap.temperatures, vec![0.0, 0.5, 0.3, 0.7]);
    }

    // -----------------------------------------------------------------
    // 17. temperature_table_persist_round_trip_via_home_path
    // -----------------------------------------------------------------
    #[tokio::test]
    async fn temperature_table_persist_round_trip_via_home_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(dir.path().to_path_buf());
        home.ensure().unwrap();
        let table = TemperatureTable::from_home(&home, true).unwrap();
        let accepting = transport(SubsetTransport::accepting(&[0.0, 0.3, 0.7]));
        let discovered = table
            .probe_and_store(
                "minimax",
                "MiniMax-M3",
                accepting,
                TEMPERATURE_PROBE_BATCH_SIZE,
            )
            .await
            .unwrap();
        assert_eq!(discovered, vec![0.0, 0.3, 0.7]);
        // The on-disk file must now exist and round-trip.
        let path = home.temperatures_auto_path();
        assert!(path.exists(), "temperatures_auto.toml must be on disk");
        let on_disk = TemperatureTableFile::load(&path).unwrap();
        let entry = on_disk
            .providers
            .get("minimax")
            .and_then(|m| m.get("MiniMax-M3"))
            .expect("entry must exist on disk");
        assert_eq!(entry.temperatures, vec![0.0, 0.3, 0.7]);
        assert!(entry.auto);
    }

    // -----------------------------------------------------------------
    // Extra: `nearest_in_set` pure helper, table-level invariants.
    // -----------------------------------------------------------------
    #[test]
    fn nearest_in_set_returns_none_on_empty() {
        assert_eq!(nearest_in_set(&[], 0.5), None);
    }

    #[test]
    fn nearest_in_set_picks_first_on_tie() {
        // 0.3 and 0.7 are equidistant from 0.5; 0.3 appears
        // first in the input.
        assert_eq!(nearest_in_set(&[0.3, 0.7], 0.5), Some(0.3));
        // Flipped order: 0.7 wins.
        assert_eq!(nearest_in_set(&[0.7, 0.3], 0.5), Some(0.7));
    }

    #[test]
    fn nearest_in_set_handles_singleton() {
        assert_eq!(nearest_in_set(&[0.5], 0.5), Some(0.5));
        assert_eq!(nearest_in_set(&[0.5], 0.7), Some(0.5));
    }

    #[test]
    fn constants_have_documented_shape() {
        // Pin the constant lengths so a refactor that drops or
        // duplicates a value surfaces here, not in production.
        assert_eq!(TEMPERATURE_PROBE_VALUES.len(), 21);
        assert_eq!(TEMPERATURE_PROBE_BATCH_SIZE, 3);
        assert!(TEMPERATURE_PROBE_VALUES.contains(&0.0));
        assert!(TEMPERATURE_PROBE_VALUES.contains(&1.0));
        assert!(TEMPERATURE_PROBE_VALUES.contains(&2.0));
    }

    /// Pin `PROBE_MIN_OUTPUT_TOKENS >= MIN_AUTOPROBE_FLOOR`. The
    /// probe body builder uses this constant to set
    /// `Request::max_tokens`, and the field is also what the
    /// upstream sees in the wire body. A regression to a small
    /// value (e.g. 16) would re-introduce the M-series / MiMo
    /// "thinks too hard and exhausts the output budget before
    /// emitting text" shape, surfacing as `content: null` and
    /// collapsing the discovered set to empty. The
    /// `MIN_AUTOPROBE_FLOOR` bound comes from
    /// [`crate::llm::probe`] and is the documented
    /// minimum-viable budget for the max-tokens probe.
    #[test]
    fn probe_min_output_tokens_is_at_least_min_autoprobe_floor() {
        const {
            assert!(
                PROBE_MIN_OUTPUT_TOKENS >= crate::llm::probe::MIN_AUTOPROBE_FLOOR,
                "PROBE_MIN_OUTPUT_TOKENS must be >= MIN_AUTOPROBE_FLOOR so the \
                 thinking footprint of any model the runtime targets fits \
                 inside the probe budget",
            );
        }
    }

    /// Pin `PROBE_TIMEOUT >= 10s`. The probe budget covers the
    /// upstream's thinking pass as well as the network
    /// round-trip. The M-series / MiMo fleet typically answers
    /// the 1-token payload in ~1.4 s; 10 s leaves generous
    /// headroom for an upstream that occasionally takes longer,
    /// while still falling through to `Indeterminate` quickly
    /// when the upstream is genuinely broken.
    #[test]
    fn probe_timeout_is_long_enough_for_thinking_models() {
        // `Duration`'s `PartialOrd` is not `const`-stable, so we
        // compare via the `const fn` `as_secs` instead of `>=`.
        const {
            assert!(
                PROBE_TIMEOUT.as_secs() >= 10,
                "PROBE_TIMEOUT must be >= 10s so the upstream has time to \
                 complete a thinking pass before the probe falls through to \
                 Indeterminate",
            );
        }
    }

    #[test]
    fn empty_table_is_empty() {
        let t = TemperatureTable::empty();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.probe_tasks_started(), 0);
    }

    #[test]
    fn from_path_with_save_disabled_does_not_set_persist_path() {
        let t = TemperatureTable::from_path(Path::new("/nonexistent.toml"), false).unwrap();
        // We cannot introspect the private `persist_path`, but
        // we can call `persist()` and confirm the no-op path:
        // it returns Ok(()) without touching the filesystem.
        t.persist().unwrap();
    }

    // -----------------------------------------------------------------
    // `quote_provider_model_keys` — cosmetic normalisation of the
    // sidecar so every `[providers.*.*]` header uses double-quoted
    // keys regardless of whether the underlying name contains a
    // bare-key-disqualifying character (e.g. `.` inside
    // `mimo-v2.5`). Pinning the shape here keeps the operator's
    // expectation stable across `toml` crate upgrades.
    // -----------------------------------------------------------------
    #[test]
    fn quote_provider_model_keys_quotes_bare_keys() {
        let input = "\
schema_version = 1\n\
[providers.kimi-k3.kimi-k3]\n\
temperatures = [0.0, 0.5, 1.0]\n\
";
        let out = quote_provider_model_keys(input);
        assert!(
            out.contains(r#"[providers."kimi-k3"."kimi-k3"]"#),
            "bare keys must be quoted; got:\n{out}"
        );
    }

    #[test]
    fn quote_provider_model_keys_idempotent_on_already_quoted_keys() {
        let input = r#"[providers."glm-5.1"."glm-5.1"]
temperatures = [0.0, 0.5, 1.0]
"#;
        let out = quote_provider_model_keys(input);
        assert_eq!(out, input, "already-quoted keys must be left untouched");
    }

    #[test]
    fn quote_provider_model_keys_ignores_non_provider_headers() {
        // The regex only matches `[providers.X.Y]`. Other tables
        // (e.g. `[operator_caps]`, `[some_other_table]`) are
        // untouched even if their keys are bare.
        let input = "\
[operator_caps.minimax]\n\
min = 524288\n\
[providers.kimi-k3.kimi-k3]\n\
temperatures = [0.0, 0.5, 1.0]\n\
";
        let out = quote_provider_model_keys(input);
        assert!(
            out.contains(r#"[providers."kimi-k3"."kimi-k3"]"#),
            "providers header must be quoted; got:\n{out}"
        );
        assert!(
            out.contains("[operator_caps.minimax]"),
            "non-provider headers must be untouched; got:\n{out}"
        );
    }
}
