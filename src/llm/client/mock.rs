//! SDK-side mock client. Issue #919 — the test-time escape hatch
//! that lets the rest of the runtime exercise the new
//! [`LlmClient`](super::LlmClient) trait without touching a live
//! provider.
//!
//! Behaviour-equivalent to [`crate::llm::mock::MockProvider`]: the
//! same `MockResponse` queue, the same role-aware dispatch (so a
//! fixture tree with role-named subdirectories still routes
//! `propose/` calls only into `propose/`, never into the global
//! pool), and the same `cycle = true` default so smoke tests do not
//! have to count call sequences. The differences are at the trait
//! boundary only:
//!
//! * Returns `sdk_type() -> "mock"`.
//! * `send` returns `Result<LlmResponse>` (the legacy `Provider`
//!   trait returns `Result<(u16, Response)>`; the status folds into
//!   `LlmResponse::http_status`).
//! * `body_sha256` hashes the canonical `LlmRequest` JSON
//!   (`sha256(serde_json::to_vec(req))`) — no provider-name special
//!   case, satisfying #900 D8 ("no compat layer").

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::ids::sha256_hex;
use crate::llm::mock::MockResponse;
use crate::llm::role::Role;

use super::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};

/// Mock SDK client. Hands out [`MockResponse`] values in order from
/// an in-memory queue (per-role sub-pools when the fixture tree has
/// role-named subdirectories, otherwise the flat global pool).
///
/// Mirrors [`crate::llm::mock::MockProvider`] field-for-field so the
/// migration PRs (issues #921-#930) can swap the registry type
/// without re-thinking the queue semantics. The new fields on the
/// SDK trait surface (`sdk_type`, `capabilities`) have SDK-side
/// defaults set here.
#[derive(Debug, Default)]
pub struct MockClient {
    responses: Vec<MockResponse>,
    index: AtomicUsize,
    /// Per-role sub-pools. Populated by [`Self::from_dir`] when the
    /// fixture tree has role-named subdirectories (e.g. `propose/`,
    /// `sketch/`). In `send()` the request's role is matched against
    /// this map first; on miss the global `responses` pool is used
    /// as a fallback. Empty when no per-role fixtures are present,
    /// so the original "serve from one ordered pool" behaviour is
    /// preserved for callers that build the mock via
    /// [`Self::new`] / [`Self::empty`].
    responses_by_role: HashMap<Role, Vec<MockResponse>>,
    /// Per-role cycle cursor. `parking_lot::Mutex` only guards the
    /// `HashMap` lookup — once we have the `Arc<AtomicUsize>` the
    /// hot path is lock-free. Empty when no per-role fixtures are
    /// present.
    role_index: parking_lot::Mutex<HashMap<Role, Arc<AtomicUsize>>>,
    name: String,
    model: String,
    endpoint: String,
    /// Recorded call metadata. SDK-side mirror of
    /// [`crate::llm::mock::MockProvider::calls`] so future migrations
    /// can keep the same telemetry shape.
    calls: parking_lot::Mutex<Vec<crate::llm::wire::CallRecord>>,
    /// When true, wrap around to the start when the queue is
    /// exhausted. Default true so smoke tests do not need to count
    /// call sequences.
    cycle: bool,
}

/// Map a directory name to the role whose fixtures live in it. The
/// returned mapping is used by [`MockClient::from_dir`] to route
/// each `.json` file into the correct per-role sub-pool.
/// Unrecognised directory names fall through to the global pool so
/// the existing flat-layout fixtures (everything at the root) keep
/// working. Mirrors [`crate::llm::mock::role_for_subdir`] one-to-one.
fn role_for_subdir(name: &str) -> Option<Role> {
    match name {
        "intake" => Some(Role::Intake),
        "clarify" => Some(Role::Clarify),
        "route" => Some(Role::Route),
        "sketch" => Some(Role::Sketch),
        "propose" => Some(Role::Propose),
        "critique" => Some(Role::Critique),
        "judge" => Some(Role::Judge),
        "deliver" => Some(Role::Deliver),
        "repair" => Some(Role::Repair),
        "tagger" => Some(Role::Tagger),
        "facet_deriver" => Some(Role::FacetDeriver),
        "extractor" => Some(Role::Extractor),
        "integrator" => Some(Role::Integrator),
        "synthesizer" => Some(Role::Synthesizer),
        "adversary" => Some(Role::Adversary),
        "merge_synthesizer" => Some(Role::MergeSynthesizer),
        "tiebreaker" => Some(Role::TiefighterCritic),
        "final_disagreement" => Some(Role::FinalDisagreement),
        "json_repair_v2" => Some(Role::JsonRepairV2),
        "hostile_prompt" => Some(Role::HostilePromptDetector),
        "persona_picker" => Some(Role::PersonaPicker),
        "angle_picker" => Some(Role::AnglePicker),
        "continuation" => Some(Role::Continuation),
        _ => None,
    }
}

impl MockClient {
    /// Build a mock client with explicit canned responses.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        tracing::debug!(count = responses.len(), "MockClient: constructed");
        Self {
            responses,
            index: AtomicUsize::new(0),
            responses_by_role: HashMap::new(),
            role_index: parking_lot::Mutex::new(HashMap::new()),
            name: "mock".to_owned(),
            model: "mock-model".to_owned(),
            endpoint: "mock://local".to_owned(),
            calls: parking_lot::Mutex::new(Vec::new()),
            cycle: true,
        }
    }

    /// Build an empty mock — useful as a placeholder for tests that
    /// will inject responses via [`Self::push`].
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Override the `endpoint()` reported by the trait methods.
    /// Tests that pin `LlmClient::endpoint` (telemetry, dashboards)
    /// use this to assert which entry the pool actually picked.
    pub fn set_endpoint(&mut self, endpoint: impl Into<String>) {
        let ep = endpoint.into();
        tracing::trace!(endpoint = %ep, "MockClient::set_endpoint");
        self.endpoint = ep;
    }

    /// Override the operator-facing `name()` reported by the trait
    /// methods. Mirrors the legacy `MockProvider` hook so future
    /// migrations keep the same registry-key control surface.
    pub fn set_name(&mut self, name: impl Into<String>) {
        let n = name.into();
        tracing::trace!(name = %n, "MockClient::set_name");
        self.name = n;
    }

    /// Override the `model()` reported by the trait methods.
    pub fn set_model(&mut self, model: impl Into<String>) {
        let m = model.into();
        tracing::trace!(model = %m, "MockClient::set_model");
        self.model = m;
    }

    /// Push a response onto the global queue.
    pub fn push(&mut self, response: MockResponse) {
        tracing::trace!(text_len = response.text.len(), "MockClient::push");
        self.responses.push(response);
    }

    /// Push a response into the per-role sub-pool for `role`.
    /// Subsequent calls with the same `role` (in `send`) draw from
    /// this sub-pool; calls with a role that has no sub-pool fall
    /// through to the global queue.
    pub fn push_for_role(&mut self, role: Role, response: MockResponse) {
        tracing::trace!(role = ?role, "MockClient::push_for_role");
        let pool = self.responses_by_role.entry(role).or_default();
        pool.push(response);
        // Lazily create the per-role cursor so the hot path in
        // `send()` is a single HashMap lookup followed by a lock-free
        // `fetch_add`. Mirrors the eager pre-creation in
        // `from_dir` for the directory-loader path.
        let mut idx = self.role_index.lock();
        idx.entry(role)
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)));
    }

    /// Set whether exhausted calls wrap to the start. Default true.
    pub fn set_cycle(&mut self, cycle: bool) {
        tracing::debug!(cycle, "MockClient::set_cycle");
        self.cycle = cycle;
    }

    /// Number of remaining (unconsumed) responses in the global pool.
    /// Per-role sub-pool sizes are reported via
    /// [`Self::remaining_for_role`].
    pub fn remaining(&self) -> usize {
        self.responses
            .len()
            .saturating_sub(self.index.load(Ordering::SeqCst))
    }

    /// Number of remaining (unconsumed) responses in the per-role
    /// sub-pool for `role`. Returns `None` when the role has no
    /// sub-pool (caller falls back to the global pool).
    pub fn remaining_for_role(&self, role: Role) -> Option<usize> {
        let pool = self.responses_by_role.get(&role)?;
        let cursor = self.role_index.lock().get(&role).cloned();
        let consumed = cursor.map(|c| c.load(Ordering::SeqCst)).unwrap_or(0);
        Some(pool.len().saturating_sub(consumed))
    }

    /// Read all calls recorded so far.
    pub fn calls(&self) -> Vec<crate::llm::wire::CallRecord> {
        self.calls.lock().clone()
    }

    /// Load canned responses from a directory tree. Each `.json` file
    /// is a `MockResponseJson` (`text` required; `usage`,
    /// `finish_reason` optional). Files are read in alphabetical
    /// order. See [`crate::llm::mock::MockProvider::from_dir`] for
    /// the full routing rules (flat → global pool; recognised role
    /// subdir → per-role sub-pool; unrecognised subdir → global
    /// pool). Mirrored here so the migration PRs can swap
    /// constructors without re-walking the fixture tree.
    pub fn from_dir(path: &Path) -> Result<Self> {
        tracing::debug!(path = %path.display(), "MockClient::from_dir");
        let mut responses: Vec<MockResponse> = Vec::new();
        let mut responses_by_role: HashMap<Role, Vec<MockResponse>> = HashMap::new();
        let mut role_index: HashMap<Role, Arc<AtomicUsize>> = HashMap::new();

        let mut entries: Vec<PathBuf> = WalkDir::new(path)
            .max_depth(2)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        entries.sort();
        tracing::trace!(entries = entries.len(), "MockClient::from_dir: walked");

        for entry in entries {
            let raw = fs::read_to_string(&entry).map_err(|e| {
                tracing::warn!(path = %entry.display(), error = %e, "MockClient: read failed");
                Error::Provider {
                    message: format!("mock read {entry:?}: {e}"),
                    http_status: None,
                }
            })?;
            let resp: MockResponseJson = serde_json::from_str(&raw).map_err(|e| {
                tracing::warn!(path = %entry.display(), error = %e, "MockClient: parse failed");
                Error::Provider {
                    message: format!("mock parse {entry:?}: {e}"),
                    http_status: None,
                }
            })?;
            let resp: MockResponse = resp.into();

            // Route by immediate parent directory — matches the
            // routing rules in `crate::llm::mock::MockProvider::from_dir`.
            let parent = entry.parent();
            let routed = parent.and_then(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(role_for_subdir)
            });
            match routed {
                Some(role) => {
                    tracing::trace!(role = ?role, path = %entry.display(), "MockClient: routing to sub-pool");
                    responses_by_role.entry(role).or_default().push(resp);
                }
                None => {
                    tracing::trace!(path = %entry.display(), "MockClient: routing to global pool");
                    responses.push(resp);
                }
            }
        }

        for &role in responses_by_role.keys() {
            role_index.insert(role, Arc::new(AtomicUsize::new(0)));
        }

        tracing::info!(
            global = responses.len(),
            per_role = responses_by_role.len(),
            "MockClient::from_dir loaded"
        );

        Ok(Self {
            responses,
            index: AtomicUsize::new(0),
            responses_by_role,
            role_index: parking_lot::Mutex::new(role_index),
            name: "mock".to_owned(),
            model: "mock-model".to_owned(),
            endpoint: "mock://local".to_owned(),
            calls: parking_lot::Mutex::new(Vec::new()),
            cycle: true,
        })
    }
}

#[derive(Debug, serde::Deserialize)]
struct MockResponseJson {
    text: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    finish_reason: Option<String>,
}

impl From<MockResponseJson> for MockResponse {
    fn from(j: MockResponseJson) -> Self {
        Self {
            text: j.text,
            usage: crate::llm::wire::Usage {
                input_tokens: j.input_tokens.unwrap_or(0),
                output_tokens: j.output_tokens.unwrap_or(0),
                cache_read: 0,
                cache_creation: 0,
            },
            finish_reason: j.finish_reason,
        }
    }
}

#[async_trait]
impl LlmClient for MockClient {
    fn sdk_type(&self) -> &'static str {
        "mock"
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> LlmCapabilities {
        // The mock advertises full streaming and tools support so the
        // dispatcher can exercise every branch deterministically —
        // mirrors `MockProvider::capabilities()` which returns
        // `ProviderCapabilities::for_mock()`.
        LlmCapabilities(crate::llm::capabilities::ProviderCapabilities::for_mock())
    }

    async fn send(&self, req: &LlmRequest) -> Result<LlmResponse> {
        tracing::trace!(role = ?req.role, "MockClient::send");
        let response = {
            let sub_pool = self.responses_by_role.get(&req.role);
            match sub_pool {
                Some(pool) if !pool.is_empty() => {
                    tracing::trace!(
                        role = ?req.role,
                        pool_size = pool.len(),
                        "MockClient::send: serving from per-role sub-pool"
                    );
                    let cursor = {
                        let map = self.role_index.lock();
                        map.get(&req.role).cloned()
                    };
                    let cursor = cursor.ok_or_else(|| {
                        tracing::error!(role = ?req.role, "MockClient::send: missing cursor (inconsistent)");
                        Error::MockExhausted
                    })?;
                    let n = pool.len();
                    let i = if self.cycle {
                        cursor.fetch_add(1, Ordering::SeqCst) % n
                    } else {
                        let i = cursor.fetch_add(1, Ordering::SeqCst);
                        if i >= n {
                            tracing::warn!(role = ?req.role, "MockClient::send: sub-pool exhausted (cycle=false)");
                            return Err(Error::MockExhausted);
                        }
                        i
                    };
                    pool.get(i).ok_or(Error::MockExhausted)?.clone()
                }
                _ => {
                    let n = self.responses.len();
                    if n == 0 {
                        tracing::warn!(role = ?req.role, "MockClient::send: global pool exhausted (empty)");
                        return Err(Error::MockExhausted);
                    }
                    let i = if self.cycle {
                        self.index.fetch_add(1, Ordering::SeqCst) % n
                    } else {
                        let i = self.index.fetch_add(1, Ordering::SeqCst);
                        if i >= n {
                            tracing::warn!("MockClient::send: global pool exhausted (cycle=false)");
                            return Err(Error::MockExhausted);
                        }
                        i
                    };
                    self.responses.get(i).ok_or(Error::MockExhausted)?.clone()
                }
            }
        };

        let record = crate::llm::wire::CallRecord {
            cache_key: String::new(),
            provider: self.name().to_owned(),
            model: self.model().to_owned(),
            started_unix: crate::time::now_unix_secs(),
            ended_unix: crate::time::now_unix_secs(),
            http_status: Some(200),
            cache_hit: false,
            usage: response.usage.clone(),
            error: None,
        };
        self.calls.lock().push(record);

        let mock_resp = response;
        let truncated = matches!(mock_resp.finish_reason.as_deref(), Some("max_tokens"));
        Ok(LlmResponse {
            text: mock_resp.text,
            finish_reason: mock_resp.finish_reason,
            truncated,
            usage: mock_resp.usage,
            http_status: 200,
        })
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // The mock has no wire body of its own — it just returns the
        // queued response. Per #900 D8 ("no compat layer, no
        // `if minimax` branch") the audit-hash must be a deterministic
        // function of the request so two calls with the same payload
        // produce the same digest and two calls with different
        // payloads produce different digests. `sha256(serde_json::to_vec(req))`
        // satisfies both invariants with zero provider-name special
        // casing.
        let bytes = serde_json::to_vec(req).map_err(|e| {
            tracing::warn!(error = %e, "MockClient::body_sha256: serialize failed");
            Error::Provider {
                message: format!("mock serialize request: {e}"),
                http_status: None,
            }
        })?;
        Ok(sha256_hex(&bytes))
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // The mock has no safety clamp to bypass, so the probe variant
        // is identical to `send`. The trait default does the same
        // forward; we override here only to make the intent explicit
        // in the call graph (so a future migration that DOES add a
        // clamp has a clear spot to override).
        self.send(req).await
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Mock has no upstream ceiling — the trait default (`u32::MAX`)
        // is correct. Override for symmetry with the live SDKs that
        // land in #920-#922 so the dispatcher's auto-probe path has
        // a hook here if a future contributor needs to clamp the
        // probe for some test-only scenario.
        u32::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(role: Role, user: &str) -> LlmRequest {
        LlmRequest {
            role,
            model: "m".into(),
            system: "s".into(),
            user: user.into(),
            max_tokens: Some(16),
            temperature: None,
            top_p: None,
            top_k: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    /// MockClient::body_sha256 is deterministic for the same
    /// LlmRequest — re-hashing the same payload returns the same
    /// digest. Pins the audit-trail determinism invariant #900 D8
    /// requires at the trait boundary.
    #[test]
    fn body_sha256_is_deterministic_for_same_request() {
        let m = MockClient::empty();
        let r = req(Role::Intake, "hello");
        let h1 = m.body_sha256(&r).unwrap();
        let h2 = m.body_sha256(&r).unwrap();
        assert_eq!(h1, h2, "same request must yield same hash");
        assert_eq!(h1.len(), 64, "SHA-256 hex is 64 lowercase chars");
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// MockClient::body_sha256 is content-addressed — changing any
    /// wire field flips the digest. Pins that the function actually
    /// hashes the request and is not returning a constant.
    #[test]
    fn body_sha256_distinguishes_requests() {
        let m = MockClient::empty();
        let a = req(Role::Intake, "hello");
        let b = req(Role::Intake, "world");
        let ha = m.body_sha256(&a).unwrap();
        let hb = m.body_sha256(&b).unwrap();
        assert_ne!(ha, hb, "different user prompts must hash differently");
    }

    /// MockClient::send returns `LlmResponse.http_status == 200` so
    /// the audit trail records a transport-level success. The
    /// legacy `Provider` trait surfaced the status as a tuple;
    /// the SDK trait folds it into the response struct so callers
    /// only deal with a single value.
    #[tokio::test]
    async fn send_returns_http_status_200() {
        let m = MockClient::new(vec![MockResponse::plain("ok")]);
        let r = m.send(&req(Role::Intake, "u")).await.unwrap();
        assert_eq!(r.http_status, 200);
        assert_eq!(r.text, "ok");
    }

    /// MockClient::send does not panic when `max_tokens = None` —
    /// the field is purely a wire-side hint and the mock has no
    /// wire to populate. Mirrors the contract pinned for
    /// `Provider::send` in `src/llm/wire.rs`.
    #[tokio::test]
    async fn send_does_not_panic_when_max_tokens_is_none() {
        let m = MockClient::new(vec![MockResponse::plain("ok")]);
        let mut r = req(Role::Intake, "u");
        r.max_tokens = None;
        r.temperature = None;
        r.top_p = None;
        r.top_k = None;
        let resp = m.send(&r).await.unwrap();
        assert_eq!(resp.text, "ok");
    }

    /// MockClient::capabilities() returns the mock baseline (full
    /// streaming + tools support, OpenAI-compat wire preference) —
    /// mirrors `MockProvider::capabilities()` which delegates to
    /// `ProviderCapabilities::for_mock()`. The trait returns
    /// `LlmCapabilities(pub ProviderCapabilities)`; the test pins
    /// the deref-through to `wire_format_id()`.
    #[test]
    fn capabilities_advertise_streaming_and_openai_compat_wire() {
        let m = MockClient::empty();
        let cap = m.capabilities();
        assert!(cap.supports_streaming);
        assert!(cap.supports_tools);
        // The mock is OpenAI-compat-flavoured (the legacy baseline
        // shared by every SDK that does not pin a different wire).
        assert_eq!(cap.wire_format_id(), "openai_compatible");
        // Pin the trait return type so a future refactor that drops
        // the newtype wrapper fails here rather than at the call site.
        let _inner: &crate::llm::capabilities::ProviderCapabilities = &cap.0;
    }

    /// MockClient::sdk_type() is the stable `"mock"` identifier the
    /// URL-path dispatcher (#922) routes on. Lives on the SDK trait
    /// surface — distinct from `name()` (the operator-facing
    /// registry key) so the dispatcher can route on SDK identity
    /// without consulting the registry.
    #[test]
    fn sdk_type_is_mock() {
        let m = MockClient::empty();
        assert_eq!(m.sdk_type(), "mock");
    }

    /// MockClient queue reuse — push a MockResponse, call `send`,
    /// get the same text back. Pins the legacy contract that
    /// migration PRs rely on so the swap of `MockProvider` for
    /// `MockClient` does not break the fixture-driven tests.
    #[tokio::test]
    async fn queue_reuse_returns_pushed_responses() {
        let mut m = MockClient::empty();
        m.push(MockResponse::plain("first"));
        m.push(MockResponse::plain("second"));
        let r1 = m.send(&req(Role::Intake, "u")).await.unwrap();
        let r2 = m.send(&req(Role::Intake, "u")).await.unwrap();
        assert_eq!(r1.text, "first");
        assert_eq!(r2.text, "second");
    }

    /// MockClient::send_probe is identical to send (no safety clamp
    /// on the mock). The override exists for symmetry with the
    /// live SDKs that will land in #920-#922.
    #[tokio::test]
    async fn send_probe_matches_send() {
        let mut m = MockClient::empty();
        m.set_endpoint("mock://probe");
        m.push(MockResponse::plain("probe-ok"));
        let r = m.send_probe(&req(Role::Intake, "u")).await.unwrap();
        assert_eq!(r.text, "probe-ok");
        assert_eq!(r.http_status, 200);
    }

    /// Per-role sub-pool: a `Propose` call draws from `Propose`'s
    /// pool only, never from the global queue. Mirrors the regression
    /// fix in `MockProvider::role_aware_dispatch_serves_only_matching_fixtures`
    /// (`src/llm/mock.rs`) so the migration PRs can swap constructors
    /// without re-discovering the bug.
    #[tokio::test]
    async fn role_aware_dispatch_serves_only_matching_subpool() {
        let mut m = MockClient::empty();
        m.push_for_role(Role::Propose, MockResponse::plain("PROPOSE-1"));
        m.push_for_role(Role::Sketch, MockResponse::plain("SKETCH-1"));
        let r1 = m.send(&req(Role::Propose, "u")).await.unwrap();
        let r2 = m.send(&req(Role::Propose, "u")).await.unwrap();
        assert_eq!(r1.text, "PROPOSE-1");
        assert_eq!(r2.text, "PROPOSE-1");
    }

    /// Backward-compat path: when no per-role sub-pool is configured
    /// for the request's role, `send` falls through to the global
    /// pool regardless of role. Keeps the flat-fixture layout that
    /// the dozens of `integration_*` tests rely on working.
    #[tokio::test]
    async fn falls_back_to_global_when_role_missing() {
        let mut m = MockClient::empty();
        m.push(MockResponse::plain("global-1"));
        m.push(MockResponse::plain("global-2"));
        let r1 = m.send(&req(Role::Propose, "u")).await.unwrap();
        let r2 = m.send(&req(Role::Propose, "u")).await.unwrap();
        assert_eq!(r1.text, "global-1");
        assert_eq!(r2.text, "global-2");
    }

    /// MockResponse::truncated sets `LlmResponse.truncated = true`
    /// so the rest of the pipeline can branch on the flag without
    /// re-parsing the finish reason.
    #[tokio::test]
    async fn truncated_response_sets_flag() {
        let m = MockClient::new(vec![MockResponse::truncated("partial")]);
        let r = m.send(&req(Role::Intake, "u")).await.unwrap();
        assert_eq!(r.finish_reason.as_deref(), Some("max_tokens"));
        assert!(r.truncated);
    }

    /// MockClient::from_dir loads the same flat-layout fixtures as
    /// MockProvider::from_dir. Pins the wiring so the migration PRs
    /// can swap constructors without re-walking the fixture tree.
    #[test]
    fn from_dir_loads_flat_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("01_intake.json"), r#"{"text": "intake-ok"}"#).unwrap();
        fs::write(
            dir.join("02_propose.json"),
            r#"{"text": "propose-ok", "input_tokens": 10, "output_tokens": 5}"#,
        )
        .unwrap();
        let m = MockClient::from_dir(dir).unwrap();
        assert_eq!(m.remaining(), 2);
    }
}
