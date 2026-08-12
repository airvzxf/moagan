//! Mock provider. Returns canned responses from an in-memory list, or
//! from a JSON file on disk.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::llm::role::Role;

use super::capabilities::ProviderCapabilities;
use super::provider::Provider;
use super::wire::{CallRecord, Request, Response, Usage};

/// A single canned response.
#[derive(Debug, Clone)]
pub struct MockResponse {
    /// Text to return as the LLM output.
    pub text: String,
    /// Optional pre-baked usage; defaults to 0 tokens.
    pub usage: Usage,
    /// Optional finish reason.
    pub finish_reason: Option<String>,
}

impl MockResponse {
    /// Build a response from raw text with zero usage.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            usage: Usage::default(),
            finish_reason: Some("end_turn".into()),
        }
    }

    /// Build a response with `finish_reason=max_tokens` so the
    /// pipeline sees a truncated response.
    pub fn truncated(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            usage: Usage::default(),
            finish_reason: Some("max_tokens".into()),
        }
    }

    /// Build a response with explicit usage.
    pub fn with_usage(text: impl Into<String>, input: u64, output: u64) -> Self {
        Self {
            text: text.into(),
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read: 0,
                cache_creation: 0,
            },
            finish_reason: Some("end_turn".into()),
        }
    }

    /// Convert to a `Response` for the [`Provider`] trait.
    pub fn into_response(self) -> Response {
        let truncated = matches!(self.finish_reason.as_deref(), Some("max_tokens"));
        Response {
            text: self.text,
            finish_reason: self.finish_reason,
            truncated,
            usage: self.usage,
        }
    }
}

/// Provider that hands out `MockResponse` values in order.
#[derive(Debug, Default)]
pub struct MockProvider {
    responses: Vec<MockResponse>,
    index: AtomicUsize,
    /// Per-role sub-pools. Populated by [`Self::from_dir`] when the
    /// fixture tree has role-named subdirectories (e.g. `propose/`,
    /// `sketch/`). In `send()` the request's role is matched against
    /// this map first; on miss the global `responses` pool is used as
    /// a fallback. Empty when no per-role fixtures are present, so
    /// the original "serve from one ordered pool" behaviour is
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
    calls: parking_lot::Mutex<Vec<CallRecord>>,
    /// When true, wrap around to the start when the queue is exhausted.
    /// Default true so smoke tests do not need to count call sequences.
    cycle: bool,
}

/// Map a directory name to the role whose fixtures live in it. The
/// returned mapping is used by [`MockProvider::from_dir`] to route
/// each `.json` file into the correct per-role sub-pool. Unrecognised
/// directory names fall through to the global pool so the existing
/// flat-layout fixtures (everything at the root) keep working.
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

impl MockProvider {
    /// Build a mock with explicit canned responses.
    pub fn new(responses: Vec<MockResponse>) -> Self {
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

    /// Override the `name()` reported by the trait methods. Useful
    /// when the same `MockProvider` instance stands in for one of
    /// several distinct provider kinds in a pool test (D.19.19:
    /// `ProviderPool` distinguishes entries by `inner.name()`).
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Override the `endpoint()` reported by the trait methods.
    /// Pairs with [`Self::set_name`] so tests that pin
    /// `Provider::endpoint` (telemetry, dashboards) can assert
    /// which entry the pool actually picked.
    pub fn set_endpoint(&mut self, endpoint: impl Into<String>) {
        self.endpoint = endpoint.into();
    }

    /// Override the `model()` reported by the trait methods.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    /// Push a response onto the queue.
    pub fn push(&mut self, response: MockResponse) {
        self.responses.push(response);
    }

    /// Set whether exhausted calls wrap to the start. Default true.
    pub fn set_cycle(&mut self, cycle: bool) {
        self.cycle = cycle;
    }

    /// Number of remaining (unconsumed) responses.
    pub fn remaining(&self) -> usize {
        self.responses
            .len()
            .saturating_sub(self.index.load(Ordering::SeqCst))
    }

    /// Read all calls recorded so far.
    pub fn calls(&self) -> Vec<CallRecord> {
        self.calls.lock().clone()
    }

    /// Load canned responses from a directory tree. Each `.json` file
    /// is a `MockResponseJson` (`text` required; `usage`, `finish_reason`
    /// optional). Files are read in alphabetical order.
    ///
    /// Routing rules:
    ///
    /// * A `.json` whose immediate parent directory is the root
    ///   (i.e. lives flat at the top of `path`) goes into the global
    ///   pool. This is the historical layout and is preserved so
    ///   existing fixtures keep working.
    /// * A `.json` whose immediate parent is a recognised role
    ///   subdirectory (e.g. `propose/`, `sketch/`, `judge/`, `deliver/`,
    ///   `critique/`, `synthesizer/`, `tiebreaker/`, … — see
    ///   [`role_for_subdir`]) goes into the per-role sub-pool for that
    ///   role. In `send()` the request's role is matched against the
    ///   sub-pools first, so role-specific fixtures no longer collide
    ///   with each other when a phase fans out (e.g. seven `propose`
    ///   calls in a row all draw from `propose/`, never from `sketch/`).
    /// * A `.json` whose immediate parent is an unrecognised
    ///   directory falls through to the global pool — same behaviour
    ///   as the flat layout.
    ///
    /// Within each pool (global or per-role) files are sorted
    /// alphabetically. Each per-role sub-pool gets its own cycle
    /// cursor, independent of the others and of the global pool.
    pub fn from_dir(path: &Path) -> Result<Self> {
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

        for entry in entries {
            let raw = fs::read_to_string(&entry)
                .map_err(|e| Error::Provider(format!("mock read {entry:?}: {e}")))?;
            let resp: MockResponseJson = serde_json::from_str(&raw)
                .map_err(|e| Error::Provider(format!("mock parse {entry:?}: {e}")))?;
            let resp: MockResponse = resp.into();

            // Route by immediate parent directory. `path.parent()`
            // strips the file name; comparing that to the root `path`
            // tells us whether the file is at the top level or under
            // a role subdirectory. We only consult the *immediate*
            // parent because deeper nesting (e.g. `propose/v1/*.json`)
            // is not in the current contract and treating it as the
            // outer role keeps the wire path explicit.
            let parent = entry.parent();
            let routed = parent.and_then(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(role_for_subdir)
            });
            match routed {
                Some(role) => {
                    responses_by_role.entry(role).or_default().push(resp);
                }
                None => responses.push(resp),
            }
        }

        // Pre-create the cycle cursor for every role that ended up
        // with at least one fixture, so the hot path in `send()` is
        // a single HashMap lookup followed by a lock-free
        // `fetch_add`. We do this in a second pass so the order of
        // `entries` does not matter.
        for &role in responses_by_role.keys() {
            role_index.insert(role, Arc::new(AtomicUsize::new(0)));
        }

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
            usage: Usage {
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
impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::for_mock()
    }

    async fn send(&self, req: &Request) -> Result<(u16, Response)> {
        // Role-aware dispatch: when the request's role has its own
        // sub-pool (populated from a role-named subdirectory in
        // `from_dir`), serve from that pool and advance *its* cursor.
        // On a miss we fall through to the global pool, preserving
        // the historical "single ordered pool" behaviour for callers
        // that build the mock via `new()` / `empty()` or load a flat
        // (root-level) fixture tree.
        let response_text = {
            let sub_pool = self.responses_by_role.get(&req.role);
            match sub_pool {
                Some(pool) if !pool.is_empty() => {
                    let cursor = {
                        let map = self.role_index.lock();
                        map.get(&req.role).cloned()
                    };
                    let cursor = cursor.ok_or(Error::MockExhausted)?;
                    let n = pool.len();
                    let i = if self.cycle {
                        cursor.fetch_add(1, Ordering::SeqCst) % n
                    } else {
                        let i = cursor.fetch_add(1, Ordering::SeqCst);
                        if i >= n {
                            return Err(Error::MockExhausted);
                        }
                        i
                    };
                    pool.get(i)
                        .ok_or(Error::MockExhausted)?
                        .clone()
                        .into_response()
                }
                _ => {
                    let n = self.responses.len();
                    if n == 0 {
                        return Err(Error::MockExhausted);
                    }
                    let i = if self.cycle {
                        self.index.fetch_add(1, Ordering::SeqCst) % n
                    } else {
                        let i = self.index.fetch_add(1, Ordering::SeqCst);
                        if i >= n {
                            return Err(Error::MockExhausted);
                        }
                        i
                    };
                    self.responses
                        .get(i)
                        .ok_or(Error::MockExhausted)?
                        .clone()
                        .into_response()
                }
            }
        };

        let record = CallRecord {
            cache_key: String::new(),
            provider: self.name().to_owned(),
            model: self.model().to_owned(),
            started_unix: crate::time::now_unix_secs(),
            ended_unix: crate::time::now_unix_secs(),
            http_status: Some(200),
            cache_hit: false,
            usage: Usage::default(),
            error: None,
        };
        self.calls.lock().push(record);
        Ok((200, response_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_responses_in_order() {
        let p = MockProvider::new(vec![
            MockResponse::plain("first"),
            MockResponse::plain("second"),
        ]);
        let req = || Request {
            role: Role::Intake,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let (status1, r1) = p.send(&req()).await.unwrap();
        let (status2, r2) = p.send(&req()).await.unwrap();
        assert_eq!(status1, 200);
        assert_eq!(status2, 200);
        assert_eq!(r1.text, "first");
        assert_eq!(r2.text, "second");
    }

    #[tokio::test]
    async fn cycle_returns_error_when_disabled() {
        let mut p = MockProvider::new(vec![MockResponse::plain("only")]);
        p.set_cycle(false);
        let req = || Request {
            role: Role::Intake,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        };
        let _r1 = p.send(&req()).await.unwrap();
        assert!(p.send(&req()).await.is_err());
    }

    #[test]
    fn from_dir_loads_responses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("01_intake.json"), r#"{"text": "intake-ok"}"#).unwrap();
        fs::write(
            dir.join("02_propose.json"),
            r#"{"text": "propose-ok", "input_tokens": 10, "output_tokens": 5}"#,
        )
        .unwrap();
        let p = MockProvider::from_dir(dir).unwrap();
        assert_eq!(p.remaining(), 2);
    }

    #[test]
    fn truncated_response_sets_flag() {
        let r = MockResponse::truncated("partial").into_response();
        assert_eq!(r.finish_reason.as_deref(), Some("max_tokens"));
        assert!(r.truncated);
    }

    #[test]
    fn plain_response_is_not_truncated() {
        let r = MockResponse::plain("hi").into_response();
        assert!(!r.truncated);
    }

    fn req_with_role(role: Role) -> Request {
        Request {
            role,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            attachments: vec![],
            tool_choice: None,
        }
    }

    /// When the fixture tree contains a role-named subdirectory
    /// (e.g. `propose/`), every call with that role must come from
    /// the sub-pool — never from a sibling role's pool and never
    /// from the global pool. This is the core regression: the smoke
    /// test `pipeline_synthesized_proposal_has_cluster_sources`
    /// was failing because seven consecutive `Propose` calls were
    /// cycling through `intake` → `clarify` → `route` → 12 `sketch`
    /// fixtures from a single global pool.
    #[tokio::test]
    async fn role_aware_dispatch_serves_only_matching_fixtures() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let propose_dir = dir.join("propose");
        let sketch_dir = dir.join("sketch");
        fs::create_dir(&propose_dir).unwrap();
        fs::create_dir(&sketch_dir).unwrap();
        fs::write(
            propose_dir.join("01-propose.json"),
            r#"{"text": "PROPOSE-1"}"#,
        )
        .unwrap();
        fs::write(sketch_dir.join("01-sketch.json"), r#"{"text": "SKETCH-1"}"#).unwrap();

        let p = MockProvider::from_dir(dir).unwrap();
        let (s1, r1) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        let (s2, r2) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        assert_eq!(s1, 200);
        assert_eq!(s2, 200);
        assert_eq!(r1.text, "PROPOSE-1");
        assert_eq!(r2.text, "PROPOSE-1");
    }

    /// Backward-compat path: when the fixture tree has no role
    /// subdirectories (the historical layout), every call falls
    /// through to the global pool regardless of the request's role.
    /// This keeps `from_dir_loads_responses` and the dozens of
    /// `integration_*` tests that load flat fixture trees working.
    #[tokio::test]
    async fn role_aware_dispatch_falls_back_to_global_when_role_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("01.json"), r#"{"text": "global-1"}"#).unwrap();
        fs::write(dir.join("02.json"), r#"{"text": "global-2"}"#).unwrap();

        let p = MockProvider::from_dir(dir).unwrap();
        let (_s1, r1) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        let (_s2, r2) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        assert_eq!(r1.text, "global-1");
        assert_eq!(r2.text, "global-2");
    }

    /// Per-role cursors are independent of the global pool cursor
    /// and of every other role's cursor. With one propose fixture
    /// and `cycle = true` (the default), three `Propose` calls must
    /// all return the same text — no spillover into other pools.
    #[tokio::test]
    async fn role_aware_dispatch_cycles_within_subpool() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let propose_dir = dir.join("propose");
        fs::create_dir(&propose_dir).unwrap();
        fs::write(propose_dir.join("01.json"), r#"{"text": "PROPOSE-ONLY"}"#).unwrap();

        let p = MockProvider::from_dir(dir).unwrap();
        let (_s1, r1) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        let (_s2, r2) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        let (_s3, r3) = p.send(&req_with_role(Role::Propose)).await.unwrap();
        assert_eq!(r1.text, "PROPOSE-ONLY");
        assert_eq!(r2.text, "PROPOSE-ONLY");
        assert_eq!(r3.text, "PROPOSE-ONLY");
    }
}
