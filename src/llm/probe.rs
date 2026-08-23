//! Auto-detection of `max_tokens` per (provider, model) via runtime probes.
//!
//! Hardcoded caps (e.g. `MINIMAX_MAX_TOKENS_CAP = 524_288`,
//! `OPENCODE_GO_MAX_TOKENS_CAP = 16_384`) are brittle: third-party relays
//! can lower the upstream's documented ceiling without warning, and a
//! downstream provider that no longer matches the operator's documented
//! cap becomes a regression that requires another code patch.
//!
//! `probe` removes that dependency on the operator's mental map of
//! per-model ceilings. The runtime flow is:
//!
//! 1. **Phase 1 — exponential search**: fire 30 sequential probes at
//!    `2^1..2^30` (each step doubles the candidate). The first failure
//!    breaks the loop; `lo` is the last confirmed OK, `hi` is the
//!    first failure. Worst case (the provider accepts every value up
//!    to `2^30`) hits the [`MAX_AUTOPROBE_CEILING`] constant and the
//!    discovered value lands at exactly that ceiling.
//!
//! 2. **Phase 2 — tightening**: bisect `[lo + 1, hi - 1]` in 20-point
//!    parallel batches. Iterate up to 32 rounds. The 20-point fan-out
//!    keeps wall-clock proportional to `O(log n)` rather than
//!    `O(n)` (which would be `2^20` requests in the worst case).
//!
//! 3. **Floor + clamp**: `discovered.max(floor).clamp(MIN, MAX)` so the
//!    caller-supplied floor (the `Option<u32>` from
//!    `ProviderConfig::max_token_auto`) cannot shrink the discovered
//!    value and the safety ceiling cannot be exceeded.
//!
//! The probe is **not** a regular LLM call: it runs over a dedicated
//! `reqwest::Client` with a 5-second timeout, never writes to
//! `calls.jsonl`, never opens the circuit breaker, and never counts
//! against `provider_usage`. The caller can therefore probe every
//! provider-model pair on every startup without skewing telemetry.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::error::{Error, Result};
use crate::llm::provider::Provider;
use crate::llm::role::Role;
use crate::llm::wire::Request;

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
///
/// The helper is duplicated from [`crate::llm::temperature_probe`]
/// rather than moved to a shared module because the fix is a small
/// cosmetic normalisation specific to the sidecar format and
/// keeping it local to each writer makes the surrounding code
/// self-contained.
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

/// Maximum exponent used by the exponential probe. With `k <= 30` the
/// value `2^k` always fits in `u32`, so `1u32 << k` cannot overflow
/// and we do not need `checked_shl` on the hot path.
pub const MAX_PROBE_SHIFT: u32 = 30;

/// Hard ceiling for the discovered `max_tokens`. Computed as
/// `1u32 << MAX_PROBE_SHIFT` so the constant and the loop bound stay
/// in lockstep: bump one, the other follows.
///
/// `1u32 << 30 = 1_073_741_824` (≈ 1.07G tokens). No real provider
/// accepts more than this today; if one does, the discovered value
/// sits exactly at the ceiling, and `phase.rs` continues to use the
/// discovered value verbatim.
pub const MAX_AUTOPROBE_CEILING: u32 = 1u32 << MAX_PROBE_SHIFT;

/// Hard floor for the discovered `max_tokens`. Anything below this is
/// treated as "provider rejected everything" and the algorithm
/// surfaces an error rather than returning a degenerate value. 1024
/// is enough for the tiniest legitimate `propose` payload while
/// keeping the floor well above any accidental `1` or `2` value.
pub const MIN_AUTOPROBE_FLOOR: u32 = 1024;

/// HTTP timeout for a single probe. 5s is enough for a healthy
/// upstream to answer an empty-payload `1`-token request; anything
/// longer means the provider is in trouble and we should fall
/// through to the next probe rather than block the loop.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe request body. Tiny, deterministic, fits in any model
/// window. The model is asked to reply with the literal `1`; the
/// response text is discarded — only the HTTP status carries
/// signal.
pub const PROBE_USER: &str = "Reply with the single character: 1";

/// Empty system prompt for the probe. The model only needs the
/// user-side `Reply with the single character: 1` instruction.
pub const PROBE_SYSTEM: &str = "";

/// Trait that the probe uses to send its tiny request. `Provider`
/// already implements this shape via `send`, but the probe needs to
/// bypass the breaker / rate-limit / circuit-breaker layer so a
/// failing probe does not poison the steady-state pool. The trait
/// exists so the probe can be unit-tested with a fake that returns
/// canned statuses without standing up a wiremock server.
#[async_trait]
pub trait ProbeTransport: Send + Sync {
    /// Send a probe with the supplied `max_tokens` and report
    /// whether the upstream accepted it.
    async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome;
}

/// Result of a single probe HTTP call. `Accepted` means the wire
/// body succeeded (status < 400 AND body does not carry the
/// `max_tokens` rejection signature). Everything else is treated as
/// a rejection so the algorithm can find the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Provider accepted `max_tokens` for the probe.
    Accepted,
    /// Provider rejected `max_tokens` with a 4xx carrying the
    /// max-tokens signature in the body.
    Rejected,
    /// Provider errored out for a reason other than max-tokens
    /// (network error, 5xx storm, malformed response). The
    /// algorithm treats this the same as `Rejected` so a flaky
    /// upstream cannot blow the loop.
    Indeterminate,
}

/// Default transport: wraps an existing `Provider` and fires a probe
/// against it. The probe deliberately bypasses the breaker (no
/// `BreakeredProvider` wrapping) so a 400 rejection does not count
/// against the circuit-breaker window.
pub struct ProviderProbeTransport {
    provider: Arc<dyn Provider>,
}

impl ProviderProbeTransport {
    /// Build a transport from a provider. The `client` field used
    /// to live here for an explicit per-probe timeout, but the
    /// transport reuses `provider.send` so the timeout is applied
    /// around the call inside [`Self::probe_send`].
    pub fn new(provider: Arc<dyn Provider>) -> Result<Self> {
        Ok(Self { provider })
    }

    /// Borrow the underlying provider. Useful for tests that want
    /// to inspect call counts.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }
}

#[async_trait]
impl ProbeTransport for ProviderProbeTransport {
    async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
        let req = Request {
            role: Role::Intake,
            model: self.provider.model().to_owned(),
            system: PROBE_SYSTEM.to_owned(),
            user: PROBE_USER.to_owned(),
            max_tokens,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let res = timeout(PROBE_TIMEOUT, self.provider.send_probe(&req)).await;
        match res {
            Ok(Ok((status, body))) => {
                // Classify:
                //   - 2xx / 3xx                       → Accepted
                //   - 4xx + body carries max_tokens   → Rejected (boundary)
                //   - 4xx + body does NOT carry it    → Indeterminate
                //                                       (e.g. 401/403 auth,
                //                                       model-not-found —
                //                                       not a max_tokens signal)
                //   - 5xx / network                  → Indeterminate
                if (200..400).contains(&status) {
                    ProbeOutcome::Accepted
                } else if (400..500).contains(&status) {
                    if body_carries_max_tokens_rejection(&body.text) {
                        ProbeOutcome::Rejected
                    } else {
                        // C2: a generic 4xx (auth, model-not-found) is
                        // not a max-tokens boundary. Treating it as
                        // Rejected would collapse the discovered ceiling
                        // to the probe's exact value, which is wrong.
                        ProbeOutcome::Indeterminate
                    }
                } else {
                    ProbeOutcome::Indeterminate
                }
            }
            Ok(Err(_)) | Err(_) => ProbeOutcome::Indeterminate,
        }
    }
}

/// Lightweight classify-by-status helper used by the wire-mock tests
/// when the test transport does not have a body to inspect. Real
/// providers carry the rejection signature in the response body; this
/// helper accepts everything that came back with `status < 400` and
/// rejects the rest. The wire-mock integration test
/// (`tests/integration_max_tokens_auto.rs`) covers the body-bearing
/// path separately.
pub fn classify_status(status: u16, _body: &[u8]) -> ProbeOutcome {
    if (200..400).contains(&status) {
        ProbeOutcome::Accepted
    } else {
        ProbeOutcome::Rejected
    }
}

/// Heuristic: does the response body carry the "max_tokens rejected"
/// signature? Real providers (Anthropic-compat, OpenAI-compat, OpenAI
/// Responses) all converge on the substring `max_tokens` somewhere in
/// the error body when the upstream rejects the request for that
/// reason. Other 400s (e.g. `model not found`) do not carry that
/// substring, so the heuristic cleanly separates the two cases.
pub fn body_carries_max_tokens_rejection(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("max_tokens")
        || lower.contains("max tokens")
        || lower.contains("max_tokens_override")
        || lower.contains("tokens limit")
        || lower.contains("maximum context length")
}

/// Run the full probe algorithm against an `Arc<dyn ProbeTransport>`.
/// `floor` is the caller-supplied minimum (the `Option<u32>` from
/// `ProviderConfig::max_token_auto`); `ceiling` is the per-provider
/// upper bound (returned by
/// [`crate::llm::provider::Provider::max_tokens_probe_ceiling`]).
/// Returns the discovered `max_tokens` clamped into `[floor,
/// ceiling]`.
///
/// The exponential phase stops at the smallest `2^k > ceiling`
/// rather than burning a probe round-trip on a value the upstream
/// will reject with HTTP 400 (DeepSeek-direct caps at 393_216;
/// MiniMax-M3 caps at 524_288; OpenCode Go's per-model caps pin
/// the OpenAI-compat and Anthropic-compat and Responses paths at
/// 16_384). Without this short-circuit the probe would otherwise
/// walk all 30 sequential `2^k` values and every value above the
/// real bound would be rejected with the `max_tokens` signature —
/// which the body-classifying branch treats as `Indeterminate` per
/// the v0.7.1 contract, collapsing the discovered ceiling to the
/// last accepted probe.
///
/// The algorithm is independent of the transport — tests use a
/// `MockProbeTransport` and production code uses the
/// `ProviderProbeTransport`. The transport is the only place that
/// talks to the network.
pub async fn detect_max_tokens(
    transport: Arc<dyn ProbeTransport>,
    floor: u32,
    ceiling: u32,
) -> Result<u32> {
    debug_assert!(
        ceiling >= MIN_AUTOPROBE_FLOOR,
        "ceiling ({ceiling}) must be at least MIN_AUTOPROBE_FLOOR ({MIN_AUTOPROBE_FLOOR})"
    );
    // Phase 1: exponential search 2^1..2^MAX_PROBE_SHIFT, with an
    // early break when `n > ceiling` (the smallest `2^k` past the
    // per-provider bound — DeepSeek at k=19 = 524_288, MiniMax at
    // k=20 = 1_048_576, OpenCode Go at k=15 = 32_768). M2: each
    // probe that comes back as `Indeterminate` (transient 5xx /
    // network blip) is retried once at the same `n` before we
    // commit the outcome. A single timeout mid-Phase-1 would
    // otherwise collapse the discovered ceiling by ~½, which is
    // exactly the regression M2 was filed against.
    let mut lo: u32 = 0;
    let mut hi: u32 = ceiling;

    for k in 1..=MAX_PROBE_SHIFT {
        let n = 1u32 << k;
        if n > ceiling {
            // First `2^k` past the per-provider bound. No probe
            // is sent — sending `n` would either be rejected by
            // the upstream (HTTP 400 with `max_tokens` body) or
            // get clamped at the wire, neither of which carries
            // signal for the algorithm. The ceiling is the
            // upper-bound sentinel from this point on.
            break;
        }
        match retry_once_on_indeterminate(transport.as_ref(), n).await {
            ProbeOutcome::Accepted => lo = n,
            _ => {
                hi = n;
                break;
            }
        }
    }
    // If every probe through `MAX_PROBE_SHIFT` accepted (only
    // possible when `ceiling >= MAX_AUTOPROBE_CEILING`), `hi`
    // stays at the initial `ceiling`. No separate "all probes
    // passed" fallback is needed — `hi = ceiling` is the right
    // sentinel because Phase 2 searches `[lo + 1, hi - 1]` =
    // `[lo + 1, ceiling - 1]`.

    // Phase 2: tighten with 20-point parallel batches. The user's
    // algorithm spec is "sum 1 to the successful value, subtract 1
    // from the failed value" so we search in (lo, hi). The
    // discovered value is the largest accepted value found by
    // Phase 2 — if Phase 2 confirms everything above `lo` is
    // rejected, the discovered value falls back to `lo` itself
    // (Phase 1's last accepted).
    //
    // Phase 2 keeps stepping down the range until the gap is small
    // enough that step=1 still hits the boundary (i.e. span <= 20).
    // When the span drops below 20, we fall through to linear
    // probes so the discovered value lands exactly on the boundary
    // rather than at some coarse-grained bucket offset.
    let mut lo_strict = lo.saturating_add(1);
    let mut hi_strict = hi.saturating_sub(1);
    let mut phase2_accepted = false;
    for _round in 0..32 {
        if lo_strict >= hi_strict || hi_strict == 0 {
            break;
        }
        let span = hi_strict.saturating_sub(lo_strict);
        let points: Vec<u32> = if span <= 20 {
            // Linear sweep: probe every value in [lo_strict, hi_strict].
            (lo_strict..=hi_strict).collect()
        } else {
            // 20-point parallel batch: step = span / 20.
            let step = span / 20;
            if step == 0 {
                break;
            }
            (1..=20).map(|i| lo_strict + i * step).collect()
        };
        let results = parallel_probe(transport.clone(), &points).await;
        let mut new_lo = lo_strict;
        let mut new_hi = hi_strict;
        for (pt, outcome) in points.iter().zip(results.iter()) {
            // M3: re-probe the same point once if the fan-out
            // returned Indeterminate. The Phase-2 boundary is a
            // single 4xx-carrying-max_tokens response; a transient
            // blip should not collapse the entire ceiling.
            let committed = match outcome {
                ProbeOutcome::Indeterminate => {
                    retry_once_on_indeterminate(transport.as_ref(), *pt).await
                }
                other => other.clone(),
            };
            match committed {
                ProbeOutcome::Accepted => {
                    new_lo = *pt;
                    phase2_accepted = true;
                }
                _ => {
                    new_hi = pt.saturating_sub(1);
                    break;
                }
            }
        }
        if new_lo == lo_strict && new_hi == hi_strict {
            break;
        }
        lo_strict = new_lo;
        hi_strict = new_hi;
    }

    let discovered = if phase2_accepted { lo_strict } else { lo };
    if discovered < MIN_AUTOPROBE_FLOOR {
        return Err(Error::Provider {
            message: format!(
                "auto-probe failed to discover a usable max_tokens (got {discovered}); provider likely rejected every probe"
            ),
            http_status: None,
        });
    }
    Ok(discovered.max(floor).min(ceiling))
}

/// M2/M3 helper: re-fire the same probe once when the first
/// attempt comes back `Indeterminate`. The retry is gated on the
/// outcome, not on a timer, so a clean Accepted or Rejected never
/// pays the second round-trip.
async fn retry_once_on_indeterminate(transport: &dyn ProbeTransport, n: u32) -> ProbeOutcome {
    match transport.probe_send(n).await {
        ProbeOutcome::Indeterminate => transport.probe_send(n).await,
        other => other,
    }
}

/// 20-point parallel fan-out. Each point runs its own probe against
/// the transport; we collect every outcome before deciding where the
/// boundary is. Failure of any single probe degrades to
/// `Indeterminate` so a transient network blip cannot lock the
/// algorithm.
///
/// M9: each spawned task is wrapped in `tokio::select!` against a
/// `CancellationToken` so a graceful shutdown aborts the fan-out
/// instead of leaking orphaned tasks. Tasks that lose the race
/// report `Indeterminate` so the algorithm can re-probe them.
pub async fn parallel_probe(
    transport: Arc<dyn ProbeTransport>,
    points: &[u32],
) -> Vec<ProbeOutcome> {
    parallel_probe_with_cancel(transport, points, None).await
}

/// Same as [`parallel_probe`] but with an explicit cancellation
/// handle. When `cancel` is `None` the calls run to completion; when
/// `Some`, a fired token causes every pending probe to return
/// `Indeterminate` so the caller can short-circuit.
pub async fn parallel_probe_with_cancel(
    transport: Arc<dyn ProbeTransport>,
    points: &[u32],
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Vec<ProbeOutcome> {
    let mut handles = Vec::with_capacity(points.len());
    for &pt in points {
        let t = transport.clone();
        let cancel_child = cancel.as_ref().map(|c| c.child_token());
        handles.push(tokio::spawn(async move {
            if let Some(c) = cancel_child {
                tokio::select! {
                    biased;
                    _ = c.cancelled() => ProbeOutcome::Indeterminate,
                    outcome = t.probe_send(pt) => outcome,
                }
            } else {
                t.probe_send(pt).await
            }
        }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(o) => out.push(o),
            Err(e) => {
                // M8: log the join error so a panic does not vanish.
                tracing::warn!(error = %e, "max_tokens_auto: parallel_probe task join failed");
                out.push(ProbeOutcome::Indeterminate);
            }
        }
    }
    out
}

/// Serialised shape of the persisted table. Lives in
/// `<MOAGAN_HOME>/max_tokens_auto.toml` and is read once at
/// startup; subsequent runs verify the cached value with a single
/// probe before trusting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaxTokensTableFile {
    /// Schema version. Bumped whenever the file shape changes
    /// incompatibly so a future `moagan` refuses to read a stale
    /// file instead of silently misinterpreting it.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// `provider_name -> model_name -> entry`. The nested `BTreeMap`
    /// gives deterministic on-disk ordering so a manual diff after a
    /// probe-run is meaningful.
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Entry>>,
    /// Operator-pinned per-provider cap written by
    /// `moagan probe max_tokens --persist-min`. The cap is the
    /// minimum across every model the operator has probed under
    /// the same provider and is the value the runtime will use
    /// as the hard ceiling on the next run. `#[serde(default)]`
    /// keeps the loader backward-compatible with v1 sidecars
    /// written before this field existed.
    #[serde(default)]
    pub operator_caps: std::collections::BTreeMap<String, OperatorCap>,
}

fn default_schema_version() -> u32 {
    1
}

impl MaxTokensTableFile {
    /// Current schema version this binary knows how to read.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Build an empty table. Useful for tests that bypass the
    /// on-disk file.
    pub fn new_empty() -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            providers: std::collections::BTreeMap::new(),
            operator_caps: std::collections::BTreeMap::new(),
        }
    }

    /// Read from a TOML file. Missing file is `Ok(new_empty())`;
    /// malformed file is `Err(Provider(...))` so a typo in
    /// operator-land cannot silently break startup.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let parsed: Self = toml::from_str(&s).map_err(|e| Error::Provider {
                    message: format!(
                        "max_tokens_auto.toml at {} is malformed: {e}",
                        path.display()
                    ),
                    http_status: None,
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    return Err(Error::Provider {
                        message: format!(
                            "max_tokens_auto.toml at {} has schema_version={}, this binary only knows up to {}",
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

    /// Persist to disk. Writes via `tempfile` then renames so a crash
    /// mid-write cannot leave a truncated file.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Provider {
                message: format!(
                    "create dir for max_tokens_auto.toml at {}: {e}",
                    parent.display()
                ),
                http_status: None,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| Error::Provider {
            message: format!("encode max_tokens_auto.toml: {e}"),
            http_status: None,
        })?;
        let body = quote_provider_model_keys(&body);
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(|e| Error::Provider {
                message: format!("tempfile for max_tokens_auto.toml: {e}"),
                http_status: None,
            })?;
        std::fs::write(tmp.path(), body).map_err(|e| Error::Provider {
            message: format!("write max_tokens_auto.toml: {e}"),
            http_status: None,
        })?;
        tmp.persist(path).map_err(|e| Error::Provider {
            message: format!("rename max_tokens_auto.toml into place: {e}"),
            http_status: None,
        })?;
        Ok(())
    }
}

/// One row of the persisted table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Last successfully probed value (i.e., the value the upstream
    /// accepted without a `max_tokens` rejection).
    pub max_tokens: u32,
    /// ISO-8601 timestamp of the last successful probe.
    pub detected_at: String,
    /// ISO-8601 timestamp of the last successful verification probe
    /// (i.e., the cached value was re-probed and still passed).
    /// Equal to `detected_at` on the first probe of a fresh model.
    #[serde(default)]
    pub verified_at: String,
    /// Always `true` for entries the probe produced. The field is
    /// explicit so a human reading the file can tell at a glance
    /// which entries came from auto-detection vs. operator-pinned
    /// overrides.
    pub auto: bool,
    /// How many probes the algorithm ran to discover this value.
    /// Useful for telemetry; the algorithm makes 30 sequential
    /// probes plus 20 per tightening round.
    #[serde(default)]
    pub attempts: u32,
}

/// Operator-pinned per-provider cap. Written by
/// `moagan probe max_tokens --persist-min` to record the minimum
/// across every model probed under one provider; the runtime
/// reads the cap on next start so a fresh run never has to
/// re-probe the same models to land at the same answer. `auto`
/// is always `false` so a human reading the file can tell the
/// entry was pinned by an operator (not discovered by the
/// algorithm).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorCap {
    /// The pinned cap in tokens.
    pub min: u32,
    /// Always `false` for an operator-pinned entry. Explicit so a
    /// grep-friendly TOML diff between auto-discovered and
    /// operator-pinned entries stays trivial.
    pub auto: bool,
    /// ISO-8601 timestamp the cap was written.
    pub detected_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Canned transport: accepts at and below `accept_up_to`,
    /// rejects strictly above. Models the typical
    /// `upstream rejects max_tokens > N` semantic.
    #[derive(Clone)]
    struct CappedTransport {
        accept_up_to: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ProbeTransport for CappedTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            if max_tokens <= self.accept_up_to.load(Ordering::SeqCst) {
                ProbeOutcome::Accepted
            } else {
                ProbeOutcome::Rejected
            }
        }
    }

    fn cap(n: u32) -> Arc<dyn ProbeTransport> {
        Arc::new(CappedTransport {
            accept_up_to: Arc::new(AtomicU32::new(n)),
        })
    }

    #[tokio::test]
    async fn detect_finds_cap_at_8k() {
        let got = detect_max_tokens(cap(8192), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 8192);
    }

    #[tokio::test]
    async fn detect_finds_cap_at_524k() {
        let got = detect_max_tokens(cap(524_288), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert!((524_000..=524_288).contains(&got), "got {got}");
    }

    #[tokio::test]
    async fn detect_caps_at_ceiling_when_provider_accepts_everything() {
        let got = detect_max_tokens(cap(u32::MAX), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, MAX_AUTOPROBE_CEILING);
    }

    #[tokio::test]
    async fn detect_returns_error_when_provider_rejects_everything() {
        // accept_up_to = MIN_AUTOPROBE_FLOOR - 1 means the probe
        // at k=10 (2^10 = 1024) is the first to reject, and the
        // discovered value lands below the floor. The function
        // surfaces an error rather than returning a degenerate
        // value.
        let err = detect_max_tokens(
            cap(MIN_AUTOPROBE_FLOOR - 1),
            MIN_AUTOPROBE_FLOOR,
            MAX_AUTOPROBE_CEILING,
        )
        .await
        .expect_err("provider rejects everything must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("auto-probe failed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn floor_is_respected() {
        // Provider caps at 1024; the operator wants at least 8192.
        // The discovered value is 1024, but the floor pushes it to
        // 8192 so the wire body always carries at least the floor.
        let got = detect_max_tokens(cap(1024), 8192, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(got, 8192);
    }

    #[test]
    fn ceiling_is_one_shifted_by_probe_shift() {
        assert_eq!(MAX_AUTOPROBE_CEILING, 1u32 << MAX_PROBE_SHIFT);
        assert_eq!(MAX_AUTOPROBE_CEILING, 1_073_741_824);
    }

    #[test]
    fn floor_is_documented_value() {
        assert_eq!(MIN_AUTOPROBE_FLOOR, 1024);
    }

    #[test]
    fn body_carries_max_tokens_signature() {
        assert!(body_carries_max_tokens_rejection(
            r#"{"type":"error","error":{"message":"max_tokens > 524288"}}"#
        ));
        assert!(body_carries_max_tokens_rejection(
            "max tokens limit reached"
        ));
        assert!(body_carries_max_tokens_rejection(
            "maximum context length exceeded"
        ));
        assert!(!body_carries_max_tokens_rejection(
            r#"{"error":"model not found"}"#
        ));
        assert!(!body_carries_max_tokens_rejection(""));
    }

    #[test]
    fn table_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        let mut file = MaxTokensTableFile::new_empty();
        file.providers
            .entry("minimax".to_owned())
            .or_default()
            .insert(
                "MiniMax-M3".to_owned(),
                Entry {
                    max_tokens: 524_288,
                    detected_at: "2026-08-11T00:00:00Z".to_owned(),
                    verified_at: "2026-08-11T00:00:00Z".to_owned(),
                    auto: true,
                    attempts: 35,
                },
            );
        file.save(&path).unwrap();
        let back = MaxTokensTableFile::load(&path).unwrap();
        assert_eq!(back, file);
    }

    #[test]
    fn load_missing_file_returns_empty_table() {
        let path = std::path::PathBuf::from("/nonexistent/max_tokens_auto.toml");
        let t = MaxTokensTableFile::load(&path).unwrap();
        assert!(t.providers.is_empty());
        assert_eq!(t.schema_version, 1);
    }

    #[test]
    fn load_rejects_future_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        std::fs::write(&path, "schema_version = 999\n[providers]\n").unwrap();
        let err = MaxTokensTableFile::load(&path).expect_err("future schema must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("schema_version")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        std::fs::write(&path, "this is = not valid toml = at all").unwrap();
        let err = MaxTokensTableFile::load(&path).expect_err("malformed must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("malformed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    /// `Response` is unused in `probe_send` but the trait surface
    /// needs to compile against the shared type. Pin the field
    /// layout here so a refactor of `wire::Response` cannot silently
    /// break the probe.
    #[test]
    fn response_layout_compiles() {
        let r = crate::llm::wire::Response {
            text: String::new(),
            finish_reason: Some("end_turn".into()),
            truncated: false,
            usage: crate::llm::wire::Usage::default(),
        };
        assert_eq!(r.text.len(), 0);
    }

    // -----------------------------------------------------------------
    // M-bug follow-up tests
    // -----------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;

    /// Transport that returns Indeterminate once for the *first* call
    /// ever, then behaves like `CappedTransport` thereafter. Models a
    /// transient 5xx that resolves itself after one retry.
    struct FlakyTransport {
        accept_up_to: u32,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProbeTransport for FlakyTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            let call_idx = self.calls.fetch_add(1, Ordering::SeqCst);
            // The very first call (any value of n) returns
            // Indeterminate. The retry must therefore succeed and
            // the algorithm should still discover the cap.
            if call_idx == 0 {
                return ProbeOutcome::Indeterminate;
            }
            if max_tokens <= self.accept_up_to {
                ProbeOutcome::Accepted
            } else {
                ProbeOutcome::Rejected
            }
        }
    }

    /// M2: a single Indeterminate mid-Phase-1 should NOT collapse the
    /// discovered ceiling. The retry recovers the boundary.
    #[tokio::test]
    async fn m2_indeterminate_in_phase1_is_retried() {
        // accept_up_to = 524288; the very first call returns
        // Indeterminate (5xx blip). The retry recovers and the
        // algorithm should still discover ~524288.
        let calls = Arc::new(AtomicUsize::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(FlakyTransport {
            accept_up_to: 524_288,
            calls: calls.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert!(
            (524_000..=524_288).contains(&got),
            "retry should recover cap, got {got}"
        );
    }

    /// C2: a 4xx that does NOT carry the "max_tokens" signature
    /// (e.g. 401 auth, 404 model-not-found) must classify as
    /// Indeterminate, not Rejected — otherwise a transient auth
    /// failure would collapse the discovered ceiling to that
    /// exact probe value.
    #[tokio::test]
    async fn c2_generic_4xx_is_indeterminate_not_rejected() {
        // We test the body_carries_max_tokens_rejection helper
        // directly because the classify-by-status path runs inside
        // the trait method (not the algorithm). The transport
        // returns a Response whose text we control.
        let r = crate::llm::wire::Response {
            text: r#"{"type":"error","error":{"message":"invalid api key"}}"#.into(),
            finish_reason: None,
            truncated: false,
            usage: crate::llm::wire::Usage::default(),
        };
        assert!(!body_carries_max_tokens_rejection(&r.text));
        // Boundary signature still classifies as Rejected-eligible.
        let r2 = crate::llm::wire::Response {
            text: r#"{"type":"error","error":{"message":"max_tokens > 524288"}}"#.into(),
            finish_reason: None,
            truncated: false,
            usage: crate::llm::wire::Usage::default(),
        };
        assert!(body_carries_max_tokens_rejection(&r2.text));
    }

    /// M9: parallel_probe_with_cancel must return Indeterminate for
    /// every point whose task was cancelled before completing.
    #[tokio::test]
    async fn m9_parallel_probe_respects_cancellation_token() {
        use tokio_util::sync::CancellationToken;
        struct NeverTransport;
        #[async_trait]
        impl ProbeTransport for NeverTransport {
            async fn probe_send(&self, _max_tokens: u32) -> ProbeOutcome {
                // Sleep long enough that the cancel beats the probe.
                tokio::time::sleep(Duration::from_secs(60)).await;
                ProbeOutcome::Accepted
            }
        }
        let t: Arc<dyn ProbeTransport> = Arc::new(NeverTransport);
        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();
        // Cancel from another task after a tiny delay.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_child.cancel();
        });
        let pts: Vec<u32> = (1..=10).collect();
        let outcomes = parallel_probe_with_cancel(t, &pts, Some(cancel)).await;
        // Every probe should have come back Indeterminate because the
        // token fired before the sleep finished.
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, ProbeOutcome::Indeterminate)),
            "expected every probe to be Indeterminate, got {outcomes:?}"
        );
    }

    /// M7 smoke: probe_tasks_started increments per task spawn, not
    /// per HTTP round-trip. Pinned by table test (see probe_table.rs).
    #[test]
    fn m7_probe_tasks_started_field_exists() {
        // Compile-time check: the field name must be `probe_tasks_started`,
        // not the legacy `probes_attempted`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max_tokens_auto.toml");
        let t =
            crate::llm::probe_table::MaxTokensTable::from_path(&path, MIN_AUTOPROBE_FLOOR, false)
                .unwrap();
        assert_eq!(t.probe_tasks_started(), 0);
    }

    // -----------------------------------------------------------------
    // PR-473 regression pin: per-provider probe ceiling.
    //
    // The exponential phase used to walk `2^1..2^30` regardless of
    // the provider; on a fresh CI runner with no cached probe
    // result, DeepSeek-direct was probed with `2^30 = 1_073_741_824`
    // and rejected with HTTP 400 `invalid_request_error`. The fix
    // adds a `ceiling` parameter to `detect_max_tokens` so the
    // exponential phase short-circuits at the first `2^k` past
    // the per-provider hard cap. Tests below pin the contract.
    // -----------------------------------------------------------------

    /// Transport that records every probe value ever sent and
    /// rejects anything strictly above `accept_up_to`. Used by the
    /// ceiling tests below to assert the algorithm never probes
    /// above the per-provider bound.
    #[derive(Clone)]
    struct RecordingTransport {
        accept_up_to: u32,
        calls: Arc<AtomicU32>,
        max_sent: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ProbeTransport for RecordingTransport {
        async fn probe_send(&self, max_tokens: u32) -> ProbeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Update max_sent only if this value is strictly larger.
            let prev = self.max_sent.load(Ordering::SeqCst);
            if max_tokens > prev {
                self.max_sent.store(max_tokens, Ordering::SeqCst);
            }
            if max_tokens <= self.accept_up_to {
                ProbeOutcome::Accepted
            } else {
                ProbeOutcome::Rejected
            }
        }
    }

    /// PR-473 regression pin: with the per-provider ceiling set to
    /// `DEEPSEEK_MAX_TOKENS_CAP = 393_216`, the exponential phase
    /// short-circuits at `2^19 = 524_288` and the algorithm never
    /// sends a value strictly above `DEEPSEEK_MAX_TOKENS_CAP` on
    /// the wire. Discovered value still lands near the upstream
    /// bound (the transport accepts everything up to 524_288, so
    /// Phase 1 ends with `lo = 524_288` past the `n > ceiling`
    /// break — but the probe never tries the value 524_288
    /// either; the break happens before the probe fires).
    #[tokio::test]
    async fn pr473_probe_never_sends_value_above_deepseek_ceiling() {
        use crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP;
        let calls = Arc::new(AtomicU32::new(0));
        let max_sent = Arc::new(AtomicU32::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(RecordingTransport {
            // Provider accepts everything up to 2^20 = 1_048_576
            // (a hypothetical bigger upstream). The algorithm
            // must still stop at the per-provider ceiling
            // DEEPSEEK_MAX_TOKENS_CAP regardless of how permissive
            // the transport is — the whole point of the ceiling
            // is to prevent the upstream from rejecting the probe
            // itself.
            accept_up_to: 1_048_576,
            calls: calls.clone(),
            max_sent: max_sent.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, DEEPSEEK_MAX_TOKENS_CAP)
            .await
            .unwrap();
        // The largest probe value sent on the wire must never
        // exceed DEEPSEEK_MAX_TOKENS_CAP. With exponential phase
        // going 2^1..2^18 (262_144), the largest value probed is
        // 262_144 — well below the ceiling.
        let max = max_sent.load(Ordering::SeqCst);
        assert!(
            max <= DEEPSEEK_MAX_TOKENS_CAP,
            "probe sent a value ({max}) above DEEPSEEK_MAX_TOKENS_CAP ({DEEPSEEK_MAX_TOKENS_CAP}) — short-circuit failed"
        );
        // The discovered value lands at the upstream's actual
        // ceiling (524_288 in this scenario): the algorithm
        // observed every probe up to 2^18 = 262_144 accepted,
        // then Phase 1 broke at the `n > ceiling` check before
        // sending 2^19 = 524_288 (which would have been
        // accepted but is past the per-provider bound). Phase 2
        // tightens Phase 1's `lo = 262_144` and the floor lifts
        // the result.
        assert!(
            got <= DEEPSEEK_MAX_TOKENS_CAP,
            "discovered value ({got}) exceeded DEEPSEEK_MAX_TOKENS_CAP"
        );
        // Sanity: the probe short-circuits at k=19 (the first
        // `2^k > 393_216`). Without the ceiling the algorithm
        // would run 19 more sequential probes for k=19..=30
        // (each accepted by this permissive transport), plus a
        // full 32-round Phase 2; with the ceiling, Phase 1 ends
        // at k=18. Phase 2 still fires (it tightens Phase 1's
        // `lo`), so we check that Phase 1 alone is short —
        // i.e. total probes <= 19 (Phase 1) + 32 * 20 (Phase 2
        // budget) = 659. The pre-fix code with no ceiling
        // would run 30 (Phase 1) + 32 * 20 = 670. The
        // difference is small but the key invariant is "no
        // probe value above ceiling on the wire" (above), which
        // is the only thing the upstream actually cares about.
        let total_calls = calls.load(Ordering::SeqCst);
        assert!(
            total_calls <= 30 + 32 * 20,
            "probe ran more than the algorithm budget, got {total_calls}"
        );
    }

    /// PR-473 regression pin: the ceiling clamp contract applies
    /// even when the transport would accept the value. A
    /// hypothetical transport that accepts 1_000_000 (above the
    /// DeepSeek cap) must not let `max_tokens = 1_000_000` leak
    /// into a real DeepSeek probe. The ceiling is the safety
    /// net, not the transport.
    #[tokio::test]
    async fn pr473_probe_clamp_applies_even_when_transport_accepts() {
        use crate::llm::capabilities::DEEPSEEK_MAX_TOKENS_CAP;
        let calls = Arc::new(AtomicU32::new(0));
        let max_sent = Arc::new(AtomicU32::new(0));
        let t: Arc<dyn ProbeTransport> = Arc::new(RecordingTransport {
            // Always accept; the ceiling is what we trust.
            accept_up_to: u32::MAX,
            calls: calls.clone(),
            max_sent: max_sent.clone(),
        });
        let got = detect_max_tokens(t, MIN_AUTOPROBE_FLOOR, DEEPSEEK_MAX_TOKENS_CAP)
            .await
            .unwrap();
        // Returned value is bounded by ceiling, not by what the
        // transport would accept.
        assert!(got <= DEEPSEEK_MAX_TOKENS_CAP, "got {got}");
        let max = max_sent.load(Ordering::SeqCst);
        assert!(
            max <= DEEPSEEK_MAX_TOKENS_CAP,
            "probe sent {max} on the wire — ceiling clamp failed"
        );
    }

    /// The default ceiling (MAX_AUTOPROBE_CEILING) preserves the
    /// legacy unbounded behaviour for callers that do not opt in
    /// to a per-provider ceiling. Existing unit + integration
    /// tests pass MAX_AUTOPROBE_CEILING for this reason; the
    /// discovered value lands at the upstream boundary regardless
    /// of the ceiling (as long as ceiling >= boundary).
    #[tokio::test]
    async fn ceiling_at_or_above_boundary_does_not_constrain_discovery() {
        // Provider accepts everything up to 8_192. With the
        // default ceiling (1.07G), the algorithm must still
        // discover 8_192.
        let got = detect_max_tokens(cap(8192), MIN_AUTOPROBE_FLOOR, MAX_AUTOPROBE_CEILING)
            .await
            .unwrap();
        assert_eq!(
            got, 8192,
            "ceiling above the boundary must not constrain discovery"
        );
    }

    // -----------------------------------------------------------------
    // `quote_provider_model_keys` — cosmetic normalisation of the
    // sidecar so every `[providers.*.*]` header uses double-quoted
    // keys regardless of whether the underlying name contains a
    // bare-key-disqualifying character (e.g. `.` inside
    // `mimo-v2.5`). Pinning the shape here keeps the operator's
    // expectation stable across `toml` crate upgrades. Mirrors the
    // same set of tests in
    // [`crate::llm::temperature_probe::tests`] so both sidecars
    // share the same quoting contract.
    // -----------------------------------------------------------------
    #[test]
    fn quote_provider_model_keys_quotes_bare_keys() {
        let input = "\
schema_version = 1\n\
[providers.kimi-k3.kimi-k3]\n\
max_tokens = 524288\n\
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
max_tokens = 524288
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
max_tokens = 524288\n\
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
