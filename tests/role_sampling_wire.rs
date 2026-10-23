//! #1026: with the default `[providers.minimax]` section
//! (`temperature = 0.6`, `top_p = 0.95`) a role that fixes its own
//! sampling is sent with it, and the other roles get the section's.

use std::sync::Arc;

use moagan::config::{Config, ProviderConfig};
use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::client::{AnthropicClient, LlmClient, ProviderRegistry};
use moagan::llm::role::Role;
use moagan::phases::phase::RunContext;
use moagan::redact::RedactPolicy;
use moagan::secret::SecretString;
use moagan::telemetry::Telemetry;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WMRequest, ResponseTemplate};

const PROVIDER: &str = "minimax";
const MODEL: &str = "minimax-role-sampling-test";

fn client(uri: String) -> Arc<dyn LlmClient> {
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
        top_p_auto_enabled: None,
        top_k_auto_enabled: None,
        extra_body: None,
    };
    Arc::new(
        AnthropicClient::new(&cfg, SecretString::new("sk-test".to_owned()))
            .expect("AnthropicClient::new accepts the test config"),
    )
}

/// Send one call per role (each with its own user text) and return
/// the wire body the upstream saw for each, in order.
async fn wire_bodies(roles: &[Role]) -> Vec<Value> {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
    home.ensure().expect("home.ensure");
    let run_id = RunId::new();

    let server = MockServer::start().await;
    let seen: Arc<parking_lot::Mutex<Vec<Value>>> = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |req: &WMRequest| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
            sink.lock().push(body);
            ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "{}"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }))
        })
        .mount(&server)
        .await;

    let mut registry = ProviderRegistry::default();
    registry.insert(PROVIDER.into(), client(server.uri()));
    let config = Config::default();
    let section = config
        .providers_by_section
        .get(PROVIDER)
        .expect("default minimax section");
    assert_eq!(
        (section.temperature, section.top_p),
        (Some(0.6), Some(0.95))
    );
    let telemetry = Telemetry::open(run_id, &home.run_dir(run_id), RedactPolicy::default(), None)
        .expect("Telemetry::open");
    let ctx = RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::new(registry),
        PROVIDER.into(),
        MODEL.into(),
        Parallelism::new(1),
        telemetry,
        String::new(),
        "standard".into(),
        Arc::new(config),
    );
    for (i, role) in roles.iter().enumerate() {
        ctx.call(*role, "sys".into(), format!("user {i}"))
            .await
            .expect("call succeeds");
    }
    let bodies = seen.lock().clone();
    assert_eq!(bodies.len(), roles.len(), "one upstream call per role");
    bodies
}

fn sampling(body: &Value) -> (Option<f64>, Option<f64>) {
    let round = |v: f64| (v * 100.0).round() / 100.0;
    (
        body.get("temperature").and_then(Value::as_f64).map(round),
        body.get("top_p").and_then(Value::as_f64).map(round),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_role_is_sent_with_its_own_sampling_and_the_rest_with_the_section() {
    let bodies = wire_bodies(&[Role::DimensionDeriver, Role::Curator, Role::Intake]).await;
    assert_eq!(
        sampling(&bodies[0]),
        (Some(0.0), Some(0.2)),
        "the deriver keeps temperature 0.0 / top_p 0.2"
    );
    assert_eq!(
        sampling(&bodies[1]),
        (Some(0.2), Some(0.95)),
        "the curator keeps temperature 0.2 and takes the section top_p"
    );
    assert_eq!(
        sampling(&bodies[2]),
        (Some(0.6), Some(0.95)),
        "intake fixes neither, so the section applies"
    );
}
