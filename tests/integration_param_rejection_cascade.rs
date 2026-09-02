//! End-to-end pin of the param-rejection cascade contract landed in
//! PR-C2 + PR-C3: the dispatcher now retries with every name detected
//! from a single upstream 4xx, the pre-call omit path skips the
//! failing round-trip when the table already knows the answer, and
//! `top_p` is opt-in via `resolve_top_p(role, provider_top_p)` so a
//! role without a catalogue entry no longer forces a `0.95` onto the
//! wire.
//!
//! Test 1 uses a scripted provider that returns errors in the EXACT
//! shape `dispatch_to_provider::parse_provider_error_body` strips
//! (`Error::Provider { message: "http {status}: {body}", ... }`). The
//! HTTP transport layer (`minimax::classify_status`) builds
//! `format!("http {status}: {body}")` with the
//! `reqwest::StatusCode` display ("400 Bad Request" instead of just
//! "400"), which would skip the dispatcher's prefix strip and break
//! the detection loop. Tests 2 + 3 go through the HTTP wire
//! (`wiremock` + `MinimaxProvider`) because they care about the
//! wire-body shape rather than the cascade logic.
//!
//! Each test drives `RunContext::call` (the public wrapper around
//! `dispatch_to_provider`). The provider is built with
//! `with_max_retries(0)` in the wiremock tests, and the scripted
//! provider in Test 1 doesn't retry by design.

#![allow(clippy::await_holding_lock)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use moagan::config::{Config, ProviderConfig, ProviderEntry, SectionKnobs};
use moagan::error::Result;
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::minimax::MinimaxProvider;
use moagan::llm::param_rejections::{PARAM_NAMES, ParamRejectionsFile, ParamRejectionsTable};
use moagan::llm::provider::{Provider, ProviderRegistry};
use moagan::llm::role::Role;
use moagan::llm::wire::{Request, Response};
use moagan::phases::phase::RunContext;
use moagan::redact::RedactPolicy;
use moagan::secret::SecretString;
use moagan::telemetry::Telemetry;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WMRequest, ResponseTemplate};

/// Provider section used by every test below. Must agree with the
/// name passed to `ProviderRegistry::insert` and the section the
/// `Config` looks up when resolving per-provider `top_p` /
/// `temperature`.
const PROVIDER: &str = "minimax";
/// `Provider::model()` for the wired-up providers. Distinct from a
/// real `MiniMax-M3` so a stale cached entry from another test
/// cannot collide.
const MODEL: &str = "minimax-cascade-test";

/// Canonical Anthropic-compat 200 envelope (minimax-style). Mirrors
/// the body `MinimaxProvider`'s response decoder expects: a
/// `content: [{"type":"text", "text":...}]` block plus
/// `stop_reason`/`usage`. Avoids re-running the response decoder's
/// parser logic in the success leg of every test.
fn success_body(text: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 1,
            "output_tokens": 1,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        }
    })
}

/// Build a real `MinimaxProvider` pointed at `uri`, with its
/// transport retry count pinned to 0. The cascade under test is the
/// dispatcher's; any internal retry in the provider layer would
/// multi-count against the cascade cap and make the request-count
/// assertions unreadable.
fn build_provider(uri: String) -> Arc<MinimaxProvider> {
    let cfg = ProviderConfig {
        models: Vec::new(),
        endpoint: Some(uri),
        temperature: None,
        top_p: None,
        omit_max_tokens: false,
        plan: None,
        max_token_auto: None,
        max_token_auto_enabled: None,
        max_token_auto_save: true,
        temperature_auto_enabled: None,
    };
    Arc::new(
        MinimaxProvider::new(&cfg, SecretString::new("sk-test".to_owned()))
            .expect("MinimaxProvider::new accepts the test config")
            .with_max_retries(0),
    )
}

/// Adjust `cfg` so the `minimax` section matches the test's
/// expectations. Each test starts from `Config::default()` and
/// narrows the section; the default section already has
/// `top_p = Some(0.95)` (line ~1066 of `src/config/mod.rs`), which
/// is correct for the cascade tests but **wrong** for the `top_p`
/// opt-in test — that one must overwrite `top_p = None`.
fn cfg_with_minimax_provider_section(
    mut cfg: Config,
    section_override: Option<ProviderConfig>,
) -> Config {
    if let Some(override_) = section_override {
        // The new-shape `providers` map is the source of truth; mutate
        // it, then re-collapse into `providers_by_section` so downstream
        // consumers see the override.
        cfg.providers.insert(
            PROVIDER.into(),
            vec![ProviderEntry {
                endpoint: override_
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| "https://api.minimax.io/anthropic/v1/messages".to_owned()),
                models: override_.models.iter().map(|m| m.id.clone()).collect(),
                knobs: SectionKnobs::default(),
            }],
        );
        cfg.collapse_providers()
            .expect("minimax section must collapse without error");
        // Re-apply per-field overrides after the collapse so
        // tests can pin temperature / top_p to specific values.
        if let Some(slot) = cfg.providers_by_section.get_mut(PROVIDER) {
            slot.temperature = override_.temperature;
            slot.top_p = override_.top_p;
            slot.omit_max_tokens = override_.omit_max_tokens;
            slot.max_token_auto = override_.max_token_auto;
            slot.max_token_auto_enabled = override_.max_token_auto_enabled;
            slot.max_token_auto_save = override_.max_token_auto_save;
            slot.temperature_auto_enabled = override_.temperature_auto_enabled;
            slot.plan = override_.plan;
        }
    }
    cfg
}

/// Build a `RunContext` rooted at `home` with the given `registry`
/// and the canonical `(PROVIDER, MODEL)` default. The `home` Arc is
/// cloned so callers retain a clone for post-call inspection
/// (`home.param_rejections_path()` etc.); the underlying directory
/// must outlive the call, which the test scope enforces via the
/// `TempDir` handle.
fn build_ctx(
    run_id: RunId,
    home: Arc<MoaganHome>,
    registry: ProviderRegistry,
    cfg: Config,
) -> RunContext {
    let telemetry = Telemetry::open(run_id, &home.run_dir(run_id), RedactPolicy::default(), None)
        .expect("Telemetry::open for cascade test");
    RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::new(registry),
        PROVIDER.into(),
        MODEL.into(),
        Parallelism::new(1),
        telemetry,
        String::new(),
        "standard".into(),
        Arc::new(cfg),
    )
}

/// Mount a wiremock that accepts every POST with the canonical 200
/// envelope and hands the parsed body to the supplied closure so the
/// test can assert on field-level shape. The closure is called once
/// per POST, in arrival order.
async fn mount_accept_all_with<F>(server: &MockServer, capture: F)
where
    F: Fn(&Value) + Send + Sync + 'static,
{
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &WMRequest| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            capture(&body);
            ResponseTemplate::new(200).set_body_json(success_body("ok"))
        })
        .mount(server)
        .await;
}

// ----- Test 1: cascade recovery via a scripted provider ----------------

/// `Provider` impl that hands out a pre-loaded queue of scripted
/// results in arrival order. Each `send` pops the front of the
/// queue — the test controls whether the next call is a 4xx (with
/// the exact error-shape `parse_provider_error_body` strips) or a
/// 200. The dispatcher's cascade loop is exercised against the
/// scripted results regardless of HTTP transport, which keeps the
/// test focused on the cascade's contract — not on the wire-format
/// boundary conditions of `reqwest::StatusCode` formatting.
struct ScriptedProvider {
    name: String,
    endpoint: String,
    outcomes: parking_lot::Mutex<VecDeque<Result<(u16, Response)>>>,
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new(outcomes: Vec<Result<(u16, Response)>>) -> Self {
        Self {
            name: PROVIDER.to_owned(),
            endpoint: format!("scripted://{MODEL}"),
            outcomes: parking_lot::Mutex::new(outcomes.into()),
            calls: AtomicUsize::new(0),
        }
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn model(&self) -> &str {
        MODEL
    }
    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    async fn send(&self, _req: &Request) -> Result<(u16, Response)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcomes
            .lock()
            .pop_front()
            .expect("scripted provider drained its queue")
    }
}

/// `opencode:gpt-5.6-luna` lists every forbidden parameter in a
/// single response (`"Unknown parameters: 'temperature', 'max_tokens',
/// 'top_p'"`). The dispatcher must detect every name from the very
/// first rejection, persist all of them, omit every one at once, and
/// retry — exactly twice at the `Provider::send` boundary (1 fail, 1
/// success). The `param_rejections.toml` sidecar must contain all
/// three names so the next run skips the failing round-trip
/// entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_recovers_from_three_param_cascade() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
    home.ensure().expect("home.ensure");
    let run_id = RunId::new();

    // The plural form the dispatcher's `detect_all_rejections`
    // catches in one shot. `Unknown parameters:` (note the trailing
    // `s`) hits the dedicated extractor in `param_rejections.rs`
    // and yields all three quoted identifiers.
    //
    // The error message shape `"http 400: {body}"` (no "Bad Request"
    // suffix) is what `phase.rs::parse_provider_error_body` strips
    // via its `format!("provider error: http {status}: ")` prefix.
    // The HTTP transport layer's `classify_status` uses
    // `reqwest::StatusCode`'s `Display`, which expands to "400 Bad
    // Request" and would skip the prefix match — so this test pins
    // the cascade contract via the documented shape rather than the
    // HTTP layer's full shape.
    let plural_body = r#"{"error":{"message":"Unknown parameters: 'temperature', 'max_tokens', 'top_p'","type":"invalid_request_error"}}"#;
    let outcomes: Vec<Result<(u16, Response)>> = vec![
        Err(moagan::error::Error::Provider {
            message: format!("http 400: {plural_body}"),
            http_status: Some(400),
        }),
        Ok((
            200,
            Response {
                text: "ok".into(),
                finish_reason: Some("end_turn".into()),
                truncated: false,
                usage: Default::default(),
            },
        )),
    ];
    let scripted = Arc::new(ScriptedProvider::new(outcomes));
    let scripted_dyn: Arc<dyn Provider> = scripted.clone();

    let mut registry = ProviderRegistry::default();
    registry.insert(PROVIDER.into(), scripted_dyn);

    let cfg = cfg_with_minimax_provider_section(Config::default(), None);
    let table = ParamRejectionsTable::from_path(&home.param_rejections_path())
        .expect("from_path on a fresh home");
    let ctx =
        build_ctx(run_id, Arc::clone(&home), registry, cfg).with_param_rejections(Arc::new(table));

    let result = ctx.call(Role::Intake, "sys".into(), "user".into()).await;

    // Cascade must recover to a 200 envelope — the dispatcher's
    // loop omits every detected name at once and retries.
    let response = result.expect("cascade must recover to 200");
    assert_eq!(
        response.text, "ok",
        "second attempt must surface the 200 body"
    );
    assert_eq!(
        scripted.call_count(),
        2,
        "cascade must issue exactly 2 sends (1 initial fail + 1 success after omit-all)"
    );

    // The cascade table at the dispatcher's default location must
    // carry all three names so a fresh run can short-circuit.
    let path = home.param_rejections_path();
    assert!(
        path.exists(),
        "param_rejections.toml must exist after the cascade; got {}",
        path.display()
    );
    let persisted = ParamRejectionsFile::load(&path).expect("load param_rejections.toml");
    let entry = persisted
        .providers
        .get(PROVIDER)
        .and_then(|m| m.get(MODEL))
        .expect("on-disk entry for (PROVIDER, MODEL) after cascade");
    let recorded: Vec<&String> = entry.iter().collect();
    assert!(
        recorded.iter().any(|p| p.as_str() == "temperature"),
        "temperature must be persisted; recorded={recorded:?}"
    );
    assert!(
        recorded.iter().any(|p| p.as_str() == "max_tokens"),
        "max_tokens must be persisted; recorded={recorded:?}"
    );
    assert!(
        recorded.iter().any(|p| p.as_str() == "top_p"),
        "top_p must be persisted; recorded={recorded:?}"
    );

    drop(temp);
    drop(home);
    drop(scripted);
}

// ----- Test 2: preflight omit (wiremock + MinimaxProvider) -------------

/// When the table already knows every rejected param for the pair,
/// the dispatcher consults the table BEFORE the first send and drops
/// every field up front. The upstream never sees a forbidden
/// parameter, so the very first POST returns 200 — there is no
/// cascade, no second round-trip, and the body sent on the wire
/// must NOT contain `temperature` / `top_p` / `max_tokens`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_omits_all_known_rejections_without_round_trip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
    home.ensure().expect("home.ensure");
    let run_id = RunId::new();

    // Pre-populate `param_rejections.toml` with all three names for
    // the (PROVIDER, MODEL) pair so the dispatcher's pre-call omit
    // gate fires on the very first send. `from_path` anchors the
    // persistence at the canonical `MOAGAN_HOME` location.
    let table_path = home.param_rejections_path();
    let table = ParamRejectionsTable::from_path(&table_path).expect("from_path on a fresh home");
    table
        .record(PROVIDER, MODEL, "temperature")
        .expect("record temperature");
    table
        .record(PROVIDER, MODEL, "max_tokens")
        .expect("record max_tokens");
    table
        .record(PROVIDER, MODEL, "top_p")
        .expect("record top_p");

    let server = MockServer::start().await;
    // Capture the first outbound body. The test asserts on that one
    // body to confirm the pre-call omit ran.
    let captured: Arc<parking_lot::Mutex<Option<Value>>> = Arc::new(parking_lot::Mutex::new(None));
    let cap_clone = Arc::clone(&captured);
    mount_accept_all_with(&server, move |body: &Value| {
        let mut slot = cap_clone.lock();
        if slot.is_none() {
            *slot = Some(body.clone());
        }
    })
    .await;

    let provider = build_provider(server.uri());
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let mut registry = ProviderRegistry::default();
    registry.insert(PROVIDER.into(), provider_dyn);

    let cfg = cfg_with_minimax_provider_section(Config::default(), None);
    let ctx =
        build_ctx(run_id, Arc::clone(&home), registry, cfg).with_param_rejections(Arc::new(table));

    let result = ctx.call(Role::Intake, "sys".into(), "user".into()).await;

    assert!(
        result.is_ok(),
        "preflight-omitted call must succeed: {result:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "preflight omit must close the loop in a single send — no cascade retries"
    );

    // Wire-body assertion: the 3 known-rejected params must NOT be
    // present on the first (only) outbound body. The minimax
    // provider's `encode_body` uses `skip_serializing_if =
    // "Option::is_none"` on `temperature` / `top_p` / `max_tokens`,
    // so `omit_param` clearing them to `None` is what drops the
    // fields from the wire.
    let captured_body = captured
        .lock()
        .clone()
        .expect("wiremock captured at least one body");
    assert!(
        captured_body.get("temperature").is_none(),
        "wire body must NOT carry temperature; got: {captured_body}"
    );
    assert!(
        captured_body.get("top_p").is_none(),
        "wire body must NOT carry top_p; got: {captured_body}"
    );
    assert!(
        captured_body.get("max_tokens").is_none(),
        "wire body must NOT carry max_tokens; got: {captured_body}"
    );

    drop(temp);
    drop(home);
}

// ----- Test 3: top_p opt-in (wiremock + MinimaxProvider) ---------------

/// PR-C3: when neither the provider nor the role's catalogue entry
/// declare `top_p`, the dispatcher resolves it to `None` via
/// `resolve_top_p(role, provider_top_p)` and the wire layer omits
/// the field via `skip_serializing_if = "Option::is_none"`. The
/// legacy `Some(unwrap_or(0.95))` is gone. `Role::Sketch` is the
/// canonical non-catalogued role (it falls through the `_ => None`
/// arm of `role_settings`), so the contract is provable end-to-end
/// against a real wiremock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn top_p_absent_from_wire_when_provider_and_role_unset() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
    home.ensure().expect("home.ensure");
    let run_id = RunId::new();

    let server = MockServer::start().await;
    let captured: Arc<parking_lot::Mutex<Option<Value>>> = Arc::new(parking_lot::Mutex::new(None));
    let cap_clone = Arc::clone(&captured);
    mount_accept_all_with(&server, move |body: &Value| {
        let mut slot = cap_clone.lock();
        if slot.is_none() {
            *slot = Some(body.clone());
        }
    })
    .await;

    let provider = build_provider(server.uri());
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let mut registry = ProviderRegistry::default();
    registry.insert(PROVIDER.into(), provider_dyn);

    // The minimax provider's per-section override: pin `top_p =
    // None` so `resolve_top_p(Sketch, None)` returns `None` and the
    // wire builder omits the field. The default `Config::default()`
    // already ships `top_p = Some(0.95)` for `minimax`
    // (~line 1066 of `src/config/mod.rs`); leaving it would shadow
    // the catalogue contract this test pins.
    let minimax_section = ProviderConfig {
        models: Vec::new(),
        endpoint: None,
        temperature: None,
        top_p: None,
        omit_max_tokens: false,
        plan: None,
        max_token_auto: None,
        max_token_auto_enabled: None,
        max_token_auto_save: true,
        temperature_auto_enabled: None,
    };
    let cfg = cfg_with_minimax_provider_section(Config::default(), Some(minimax_section));
    let ctx = build_ctx(run_id, Arc::clone(&home), registry, cfg);

    let result = ctx.call(Role::Sketch, "sys".into(), "user".into()).await;

    assert!(
        result.is_ok(),
        "call must succeed in the no-cascade branch: {result:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "first send must already carry the omitted body"
    );

    // The wire body must NOT carry `top_p`. The other fields stay
    // (`temperature` is `Some(1.0)` from the per-role default; the
    // Anthropic-compat envelope's other keys are unchanged) — the
    // test narrowly asserts on `top_p` so the contract is
    // unambiguous.
    let body = captured.lock().clone().expect("wiremock captured the body");
    assert!(
        body.get("top_p").is_none(),
        "wire body must NOT carry top_p when neither provider nor role set it; got: {body}"
    );
    // Sanity: the rest of the wire shape is intact (`model` +
    // `system` + `temperature` from the per-role default), so a
    // future regression cannot silently flip the omit by swapping
    // the wrong field.
    assert_eq!(
        body.get("model").and_then(|v| v.as_str()),
        Some(MODEL),
        "model field must round-trip on the wire"
    );
    let temp_field = body
        .get("temperature")
        .and_then(|v| v.as_f64())
        .expect("temperature is set to Role::Sketch's hard-coded default (1.0)");
    assert!(
        (temp_field - 1.0).abs() < 1e-6,
        "Sketch's temperature default is 1.0 (per-role fallback); got {temp_field}"
    );
    // PARAM_NAMES documents the names the cascade path knows about
    // — referenced so a future contributor who adds a new name
    // remembers to mirror it here.
    assert_eq!(
        PARAM_NAMES.len(),
        3,
        "PARAM_NAMES must continue to enumerate exactly the cascade's known fields"
    );

    drop(temp);
    drop(home);
}
