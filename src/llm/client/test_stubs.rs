//! SDK-side test stub. Issue #925 — the LlmClient equivalent of the
//! legacy `MockProvider` for the probe subsystem.
//!
//! `ScriptedLlmClient` implements [`LlmClient`] with a queue of
//! scripted outcomes (`Ok(LlmResponse)` or `Err(Error)`) and a
//! scripted-per-request router. Test code in
//! `src/llm/probe.rs::tests` and
//! `src/llm/temperature_probe.rs::tests` uses this stub to model
//! upstream behaviour without standing up a wiremock server.
//!
//! The whole module is gated on `#[cfg(test)]` so the release binary
//! pays zero cost for the scripted-queue plumbing. Use
//! [`super::super::MockClient`] (which is `pub`) for production code-
//! path tests; this stub exists only for the probe algorithm, which
//! needs the per-call scripted `http_status` and `text` control that
//! the role-aware mock client does not expose.
//!
//! Two layers of scripting compose:
//!
//! - **Per-request router** ([`Self::with_router`]) — the most
//!   general form. Each call dispatches through `Fn(&LlmRequest) -> ScriptedLlmResponse`
//!   so a test can branch on `req.max_tokens`, `req.temperature`,
//!   etc. and emit the right outcome.
//! - **Per-call queue** ([`Self::push_ok`], [`Self::push_err`]) — a
//!   FIFO when the test only cares about a fixed sequence (the common
//!   case for `phase_0` regressions).
//!
//! `cycle = true` makes the queue wrap; `cycle = false` returns a
//! default `Ok(LlmResponse { http_status: 200, text: "" })` after
//! the queue is exhausted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::error::{Error, Result};
use crate::ids::sha256_hex;
use crate::llm::capabilities::ProviderCapabilities;
use crate::llm::client::{LlmCapabilities, LlmClient, LlmRequest, LlmResponse};

/// One scripted outcome for a single probe call. Distinguishes
/// `Ok(LlmResponse)` (transport succeeded, classifier inspects
/// `http_status` + `text`) from `Err(Error)` (transport failed,
/// classifier inspects `Error::Provider { http_status, message }`).
///
/// `Clone` is intentionally **not** derived — `Error` does not
/// implement `Clone`, and a test that needs the same scripted
/// outcome twice should build it once and queue it via
/// [`ScriptedLlmClient::push_ok`] / [`ScriptedLlmClient::push_err`]
/// (which deep-clones the body fields the test cares about).
#[derive(Debug)]
pub(crate) enum ScriptedLlmResponse {
    /// Scripted success. The classifier paths treat the response as
    /// the canonical transport response (`http_status` for the
    /// 2xx/3xx/4xx/5xx bands; `text` for body content checks).
    Ok(LlmResponse),
    /// Scripted transport failure. Used for the network / timeout /
    /// 5xx branches where the body is empty. `Error::Provider` is the
    /// common variant — the body-recovery classifier handles the
    /// "error carries body in `message`" shape via
    /// [`crate::llm::probe::body_from_provider_error`] when the
    /// algorithm sees `http_status: Some(4xx)` with a parseable body.
    Err(Error),
}

#[allow(dead_code)] // Test helpers retained for future probe tests (#925 + later).
impl ScriptedLlmResponse {
    /// Build a plain `LlmResponse` with `http_status = 200`,
    /// empty usage, and the supplied body text. Convenience for the
    /// common "upstream accepted the probe" branch.
    pub(crate) fn accepted(body: impl Into<String>) -> Self {
        Self::Ok(LlmResponse {
            text: body.into(),
            finish_reason: Some("end_turn".to_owned()),
            truncated: false,
            usage: Default::default(),
            http_status: 200,
        })
    }

    /// Build a 4xx `LlmResponse` carrying the supplied body so the
    /// algorithm's `body_carries_*_rejection` helper can classify
    /// the boundary. `http_status` defaults to 400 to mirror the
    /// canonical `"http <code> Bad Request"` shape the legacy
    /// provider surface emitted.
    pub(crate) fn rejected_4xx(body: impl Into<String>, status: u16) -> Self {
        Self::Ok(LlmResponse {
            text: body.into(),
            finish_reason: None,
            truncated: false,
            usage: Default::default(),
            http_status: status,
        })
    }

    /// Build a `5xx`-flavoured `LlmResponse` for the
    /// indeterminate branch.
    pub(crate) fn rejected_5xx(body: impl Into<String>, status: u16) -> Self {
        Self::Ok(LlmResponse {
            text: body.into(),
            finish_reason: None,
            truncated: false,
            usage: Default::default(),
            http_status: status,
        })
    }
}

/// Scripted SDK client. Hands out [`ScriptedLlmResponse`] values
/// either from a queue (FIFO) or via a per-request router closure,
/// whichever the test installs.
///
/// Per-request dispatcher signature: `Fn(&LlmRequest) -> ScriptedLlmResponse`
/// bridged through `Box` so the stub can run a fresh closure
/// inside the `Send + Sync` boundary required by `Mutex`. Kept as
/// an explicit type alias so clippy's `type_complexity` lint has a
/// single anchor to point at.
pub(crate) type RouterFn = Box<dyn Fn(&LlmRequest) -> ScriptedLlmResponse + Send + Sync>;

/// `Send + Sync` is required so the stub can sit inside an `Arc`
/// and be passed to the probe algorithms as `Arc<dyn LlmClient>`.
pub(crate) struct ScriptedLlmClient {
    name: String,
    model: String,
    endpoint: String,
    /// FIFO queue of scripted outcomes. When a router is set, it
    /// takes precedence over the queue; when neither is set the stub
    /// returns the `default_ok` fallback (200 with empty body).
    queue: Mutex<VecDeque<ScriptedLlmResponse>>,
    /// Per-request dispatcher. `None` means "use the queue".
    router: Mutex<Option<RouterFn>>,
    /// Whether exhausted queue / router calls return the default
    /// `accepted("")` response (`true`, smoke-friendly) or
    /// `Err(Error::MockExhausted)` (`false`, strict). Mirrors
    /// [`crate::llm::mock::MockProvider::set_cycle`].
    cycle: bool,
    /// Recorded call count. Bumped by every `send` / `send_probe`
    /// invocation regardless of which layer serviced it.
    call_count: AtomicUsize,
}

impl std::fmt::Debug for ScriptedLlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedLlmClient")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("queue_len", &self.queue.lock().len())
            .field("cycle", &self.cycle)
            .field("call_count", &self.call_count)
            .finish()
    }
}

#[allow(dead_code)] // Test helpers retained for future probe tests (#925 + later).
impl ScriptedLlmClient {
    /// Empty stub. All calls return the `accepted("")` fallback
    /// (`http_status = 200`, empty body, end_turn).
    pub(crate) fn empty() -> Self {
        Self {
            name: "mock".to_owned(),
            model: "mock-model".to_owned(),
            endpoint: "mock://local".to_owned(),
            queue: Mutex::new(VecDeque::new()),
            router: Mutex::new(None),
            cycle: true,
            call_count: AtomicUsize::new(0),
        }
    }

    /// Build with a starting queue (consumed in FIFO order before
    /// the cycle fallback). Convenience for the common "script a
    /// fixed sequence" pattern.
    pub(crate) fn with_responses(responses: Vec<ScriptedLlmResponse>) -> Self {
        Self {
            name: "mock".to_owned(),
            model: "mock-model".to_owned(),
            endpoint: "mock://local".to_owned(),
            queue: Mutex::new(responses.into()),
            router: Mutex::new(None),
            cycle: true,
            call_count: AtomicUsize::new(0),
        }
    }

    /// Override the operator-facing name. Mirrors `MockClient::set_name`
    /// so probe-internal assertions on `client.name()` stay precise.
    pub(crate) fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Override the model. Mirrors `MockClient::set_model`.
    pub(crate) fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }

    /// Override the endpoint. Mirrors `MockClient::set_endpoint`.
    pub(crate) fn set_endpoint(&mut self, endpoint: impl Into<String>) {
        self.endpoint = endpoint.into();
    }

    /// Push an `Ok(LlmResponse)` onto the back of the queue.
    pub(crate) fn push_ok(&mut self, response: ScriptedLlmResponse) {
        debug_assert!(
            matches!(response, ScriptedLlmResponse::Ok(_)),
            "push_ok requires an Ok variant; use push_err for the error path"
        );
        self.queue.lock().push_back(response);
    }

    /// Push an `Err(Error)` onto the back of the queue.
    pub(crate) fn push_err(&mut self, err: Error) {
        self.queue.lock().push_back(ScriptedLlmResponse::Err(err));
    }

    /// Install a per-request router. The router runs first, ahead of
    /// the queue. Tests that branch on `req.max_tokens`,
    /// `req.temperature`, etc. install one of these.
    pub(crate) fn set_router<F>(&mut self, router: F)
    where
        F: Fn(&LlmRequest) -> ScriptedLlmResponse + Send + Sync + 'static,
    {
        *self.router.lock() = Some(Box::new(router));
    }

    /// Set whether exhausted calls wrap to a default 200 OK response
    /// (`true`, smoke-friendly) or surface `Error::MockExhausted`
    /// (`false`, strict). Mirrors `MockClient::set_cycle`.
    pub(crate) fn set_cycle(&mut self, cycle: bool) {
        self.cycle = cycle;
    }

    /// Number of recorded `send` / `send_probe` invocations.
    pub(crate) fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Pop the next scripted response per the router / queue /
    /// cycle fallback. Held private; the trait methods are the only
    /// public callers.
    fn next_response(&self, req: &LlmRequest) -> ScriptedLlmResponse {
        // Router takes precedence — installed once, runs on every
        // call. Mutex only guards the `Option` swap; once present
        // the hot path is a single lock + closure call.
        if let Some(router) = self.router.lock().as_ref() {
            return router(req);
        }
        let mut queue = self.queue.lock();
        if let Some(resp) = queue.pop_front() {
            return resp;
        }
        if self.cycle {
            ScriptedLlmResponse::accepted("")
        } else {
            ScriptedLlmResponse::Err(Error::MockExhausted)
        }
    }
}

#[async_trait]
impl LlmClient for ScriptedLlmClient {
    fn sdk_type(&self) -> &'static str {
        // Stable identifier; matches `MockClient::sdk_type` so
        // probe-internal assertions on `client.sdk_type()` are
        // consistent across the two stubs.
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
        LlmCapabilities(ProviderCapabilities::for_mock())
    }

    async fn send(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        let scripted = self.next_response(_req);
        self.call_count.fetch_add(1, Ordering::SeqCst);
        match scripted {
            ScriptedLlmResponse::Ok(resp) => Ok(resp),
            ScriptedLlmResponse::Err(err) => Err(err),
        }
    }

    fn body_sha256(&self, req: &LlmRequest) -> Result<String> {
        // Mirrors `MockClient::body_sha256` — D8 invariant holds:
        // two calls with the same payload yield the same digest,
        // any wire-field mutation flips it. The stub has no wire
        // body of its own so the canonical JSON hash is the
        // appropriate source of truth.
        let bytes = serde_json::to_vec(req).map_err(|e| Error::Provider {
            message: format!("scripted serialize request: {e}"),
            http_status: None,
        })?;
        Ok(sha256_hex(&bytes))
    }

    async fn send_probe(&self, req: &LlmRequest) -> Result<LlmResponse> {
        // Same dispatch as `send`: scripts cannot distinguish
        // `send` from `send_probe`. Tests that need the
        // distinction install a router that branches on
        // `req.max_tokens` / `req.temperature`.
        let scripted = self.next_response(req);
        self.call_count.fetch_add(1, Ordering::SeqCst);
        match scripted {
            ScriptedLlmResponse::Ok(resp) => Ok(resp),
            ScriptedLlmResponse::Err(err) => Err(err),
        }
    }

    fn max_tokens_probe_ceiling(&self) -> u32 {
        // Mirror `MockClient::max_tokens_probe_ceiling` — the stub
        // has no upstream ceiling; `u32::MAX` keeps the probe's
        // exponential phase free to walk the full range.
        u32::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::role::Role;

    fn req(max_tokens: u32, temperature: f32) -> LlmRequest {
        LlmRequest {
            role: Role::Sketch,
            model: "m".into(),
            system: "sys".into(),
            user: "u".into(),
            max_tokens: Some(max_tokens),
            temperature: Some(temperature),
            top_p: None,
            top_k: None,
            response_schema: None,
            stream: false,
            extra_messages: Vec::new(),
            attachments: Vec::new(),
            tool_choice: None,
        }
    }

    /// `next_response` follows the router when set. Pins the
    /// router-first dispatch so a test that branches on
    /// `req.max_tokens` always lands on the right outcome.
    #[tokio::test]
    async fn router_takes_precedence_over_queue() {
        let mut stub = ScriptedLlmClient::empty();
        stub.push_ok(ScriptedLlmResponse::accepted("queue-1"));
        stub.set_router(|r| {
            if r.max_tokens == Some(2) {
                ScriptedLlmResponse::accepted("router-accepted")
            } else {
                ScriptedLlmResponse::rejected_4xx("rejected", 400)
            }
        });
        // Router returns "router-accepted" for max_tokens=2;
        // queue entry stays untouched behind the router.
        let r1 = stub.send_probe(&req(2, 0.0)).await.unwrap();
        assert_eq!(r1.text, "router-accepted");
        let r2 = stub.send_probe(&req(7, 0.0)).await.unwrap();
        assert_eq!(r2.text, "rejected");
        assert_eq!(r2.http_status, 400);
    }

    /// FIFO queue, no router. Each call consumes the next entry;
    /// once the queue is exhausted the `cycle = true` fallback
    /// returns `accepted("")`.
    #[tokio::test]
    async fn fifo_queue_then_default_accepted_when_cycle() {
        let mut stub = ScriptedLlmClient::empty();
        stub.push_ok(ScriptedLlmResponse::accepted("q1"));
        stub.push_ok(ScriptedLlmResponse::accepted("q2"));
        assert_eq!(stub.send_probe(&req(1, 0.0)).await.unwrap().text, "q1");
        assert_eq!(stub.send_probe(&req(1, 0.0)).await.unwrap().text, "q2");
        // Cycle fallback.
        let r = stub.send_probe(&req(1, 0.0)).await.unwrap();
        assert_eq!(r.text, "");
        assert_eq!(r.http_status, 200);
    }

    /// `cycle = false` returns `Error::MockExhausted` once the
    /// queue is drained. Pins the strict-mode contract used by
    /// tests that count wire calls.
    #[tokio::test]
    async fn cycle_false_surfaces_mock_exhausted() {
        let mut stub = ScriptedLlmClient::empty();
        stub.push_ok(ScriptedLlmResponse::accepted("only"));
        stub.set_cycle(false);
        let r1 = stub.send_probe(&req(1, 0.0)).await.unwrap();
        assert_eq!(r1.text, "only");
        let err = stub.send_probe(&req(1, 0.0)).await.unwrap_err();
        assert!(
            matches!(err, Error::MockExhausted),
            "expected MockExhausted; got {err:?}"
        );
    }

    /// `send_err` queues an `Err(Error::Provider { message, http_status })`
    /// entry. Used by probe tests that want to drive the
    /// "error carries body in `message`" classifier branch.
    #[tokio::test]
    async fn push_err_preserves_body_in_message() {
        let mut stub = ScriptedLlmClient::empty();
        stub.push_err(Error::Provider {
            message: "http 400 Bad Request: max_tokens > 100".to_owned(),
            http_status: Some(400),
        });
        let err = stub.send_probe(&req(1, 0.0)).await.unwrap_err();
        let Error::Provider {
            message,
            http_status,
        } = err
        else {
            panic!("expected Error::Provider");
        };
        assert_eq!(http_status, Some(400));
        assert!(message.contains("max_tokens"));
    }

    /// Per-call counter increments on every `send` and `send_probe`.
    /// Mirrors `MockClient::body_sha256` style: the counter is the
    /// single source of truth for "did the probe fire at all?".
    #[tokio::test]
    async fn call_count_increments_per_invocation() {
        let stub = ScriptedLlmClient::empty();
        assert_eq!(stub.call_count(), 0);
        let _ = stub.send_probe(&req(1, 0.0)).await;
        let _ = stub.send_probe(&req(2, 0.0)).await;
        let _ = stub.send(&req(3, 0.0)).await;
        assert_eq!(stub.call_count(), 3);
    }
}
