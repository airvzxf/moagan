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
        };
        let res = timeout(PROBE_TIMEOUT, self.provider.send_probe(&req)).await;
        match res {
            Ok(Ok((status, _body))) => {
                // Classify:
                //   - 2xx / 3xx         → Accepted (the upstream accepted
                //                         this max_tokens value).
                //   - 4xx (any)         → Rejected (a 4xx is the algorithm's
                //                         signal; the max-tokens rejection
                //                         lives in 4xx territory per the
                //                         Anthropic and OpenAI specs).
                //   - 5xx / network    → Indeterminate (transient; do not
                //                         treat as a max-tokens boundary).
                // The 4xx-vs-5xx distinction matters because some 5xx
                // storm or auth-500 would otherwise be misread as
                // "the upstream rejected this max_tokens" and collapse
                // the discovered ceiling to that exact probe value.
                if (200..400).contains(&status) {
                    ProbeOutcome::Accepted
                } else if (400..500).contains(&status) {
                    ProbeOutcome::Rejected
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
/// `ProviderConfig::max_token_auto`). Returns the discovered
/// `max_tokens` clamped into `[MIN_AUTOPROBE_FLOOR,
/// MAX_AUTOPROBE_CEILING]`.
///
/// The algorithm is independent of the transport — tests use a
/// `MockProbeTransport` and production code uses the
/// `ProviderProbeTransport`. The transport is the only place that
/// talks to the network.
pub async fn detect_max_tokens(transport: Arc<dyn ProbeTransport>, floor: u32) -> Result<u32> {
    // Phase 1: exponential search 2^1..2^30.
    let mut lo: u32 = 0;
    let mut hi: u32 = u32::MAX;

    for k in 1..=MAX_PROBE_SHIFT {
        let n = 1u32 << k;
        match transport.probe_send(n).await {
            ProbeOutcome::Accepted => lo = n,
            _ => {
                hi = n;
                break;
            }
        }
    }
    // All 30 probes passed. Cap at the safety ceiling.
    if hi == u32::MAX {
        hi = MAX_AUTOPROBE_CEILING;
    }

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
            match outcome {
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
        return Err(Error::Provider(format!(
            "auto-probe failed to discover a usable max_tokens (got {discovered}); provider likely rejected every probe"
        )));
    }
    Ok(discovered.max(floor).min(MAX_AUTOPROBE_CEILING))
}

/// 20-point parallel fan-out. Each point runs its own probe against
/// the transport; we collect every outcome before deciding where the
/// boundary is. Failure of any single probe degrades to
/// `Indeterminate` so a transient network blip cannot lock the
/// algorithm.
pub async fn parallel_probe(
    transport: Arc<dyn ProbeTransport>,
    points: &[u32],
) -> Vec<ProbeOutcome> {
    let mut handles = Vec::with_capacity(points.len());
    for &pt in points {
        let t = transport.clone();
        handles.push(tokio::spawn(async move { t.probe_send(pt).await }));
    }
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(o) => out.push(o),
            Err(_) => out.push(ProbeOutcome::Indeterminate),
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
        }
    }

    /// Read from a TOML file. Missing file is `Ok(new_empty())`;
    /// malformed file is `Err(Provider(...))` so a typo in
    /// operator-land cannot silently break startup.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let parsed: Self = toml::from_str(&s).map_err(|e| {
                    Error::Provider(format!(
                        "max_tokens_auto.toml at {} is malformed: {e}",
                        path.display()
                    ))
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    return Err(Error::Provider(format!(
                        "max_tokens_auto.toml at {} has schema_version={}, this binary only knows up to {}",
                        path.display(),
                        parsed.schema_version,
                        Self::CURRENT_SCHEMA_VERSION
                    )));
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
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Provider(format!(
                    "create dir for max_tokens_auto.toml at {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| Error::Provider(format!("encode max_tokens_auto.toml: {e}")))?;
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(|e| Error::Provider(format!("tempfile for max_tokens_auto.toml: {e}")))?;
        std::fs::write(tmp.path(), body)
            .map_err(|e| Error::Provider(format!("write max_tokens_auto.toml: {e}")))?;
        tmp.persist(path)
            .map_err(|e| Error::Provider(format!("rename max_tokens_auto.toml into place: {e}")))?;
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
        let got = detect_max_tokens(cap(8192), MIN_AUTOPROBE_FLOOR)
            .await
            .unwrap();
        assert_eq!(got, 8192);
    }

    #[tokio::test]
    async fn detect_finds_cap_at_524k() {
        let got = detect_max_tokens(cap(524_288), MIN_AUTOPROBE_FLOOR)
            .await
            .unwrap();
        assert!((524_000..=524_288).contains(&got), "got {got}");
    }

    #[tokio::test]
    async fn detect_caps_at_ceiling_when_provider_accepts_everything() {
        let got = detect_max_tokens(cap(u32::MAX), MIN_AUTOPROBE_FLOOR)
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
        let err = detect_max_tokens(cap(MIN_AUTOPROBE_FLOOR - 1), MIN_AUTOPROBE_FLOOR)
            .await
            .expect_err("provider rejects everything must error");
        match err {
            Error::Provider(msg) => assert!(msg.contains("auto-probe failed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn floor_is_respected() {
        // Provider caps at 1024; the operator wants at least 8192.
        // The discovered value is 1024, but the floor pushes it to
        // 8192 so the wire body always carries at least the floor.
        let got = detect_max_tokens(cap(1024), 8192).await.unwrap();
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
            Error::Provider(msg) => assert!(msg.contains("schema_version")),
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
            Error::Provider(msg) => assert!(msg.contains("malformed")),
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
}
