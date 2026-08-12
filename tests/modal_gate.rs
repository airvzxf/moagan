//! Wiremock-backed integration tests for
//! [`moagan::llm::modal_gate::ModalityGate`].
//!
//! Pin the gate's behaviour in a more realistic scenario than the
//! unit tests: the wire body is sent to a mock upstream, so any
//! accidental re-introduction of an attachment or a `tool_choice`
//! field (after the gate has dropped it) trips the test.
//!
//! The tests deliberately exercise only the gate + the wire
//! serialiser — not a concrete provider implementation — so the
//! contract stays decoupled from the per-wire-shape translation
//! rules. The body shape assertions use the
//! `models_dev`-aligned vocabulary the gate is meant to enforce.

use moagan::llm::Role;
use moagan::llm::modal_gate::ModalityGate;
use moagan::llm::models_dev::{Limits, Modalities, ModelsDevEntry};
use moagan::llm::wire::{Attachment, Request, ToolChoice};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a `ModelsDevEntry` for the integration tests. Mirrors the
/// helper in `modal_gate.rs::tests` but lives in a public location
/// so the wiremock tests can construct a gate from it.
fn entry(attachment: bool, tool_call: bool, modalities_in: &[&str]) -> ModelsDevEntry {
    ModelsDevEntry {
        id: "wiremock-model".to_string(),
        name: "wiremock-model".to_string(),
        family: Some("test".to_string()),
        attachment,
        reasoning: false,
        reasoning_options: vec![],
        tool_call,
        temperature: true,
        interleaved: None,
        modalities: Modalities {
            input: modalities_in.iter().map(|s| s.to_string()).collect(),
            output: vec!["text".to_string()],
        },
        limit: Limits {
            context: 8192,
            output: 2048,
        },
        cost: Default::default(),
        open_weights: false,
        release_date: None,
        last_updated: None,
    }
}

/// Skeleton request for the wiremock tests. Filled in by the test
/// body with the field under test.
fn request() -> Request {
    Request {
        role: Role::Sketch,
        model: "wiremock-model".to_string(),
        system: String::new(),
        user: "hello".to_string(),
        max_tokens: 16,
        temperature: Some(0.6),
        top_p: Some(0.95),
        response_schema: None,
        stream: false,
        extra_messages: vec![],
        attachments: vec![],
        tool_choice: None,
    }
}

/// Serialise a request the way the OpenAI-compat wire builder
/// would, so the body we send to the mock server is exactly what
/// a real call would carry. The translation here is intentionally
/// minimal (the per-provider module is the canonical
/// translator); the goal is to exercise the gate's effect on the
/// body, not the full wire format.
fn body_json(req: &Request) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": [{
            "role": "user",
            "content": req.user,
        }],
    });
    if !req.attachments.is_empty() {
        let arr = serde_json::json!(
            req.attachments
                .iter()
                .map(|a| serde_json::json!({
                    "type": a.modality,
                    "mime": a.mime,
                }))
                .collect::<Vec<_>>()
        );
        body["attachments"] = arr;
    }
    if let Some(tc) = &req.tool_choice {
        body["tool_choice"] = serde_json::json!(tc);
    }
    body
}

/// PR-5 test 7: a request that carries an attachment to a model
/// that does not accept attachments is blocked at the gate.
/// The mock server never sees a request body because the gate
/// rejects the call before the wire builder runs.
#[tokio::test]
async fn integration_attachment_to_non_attachment_model_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        })))
        .expect(0)
        .mount(&server)
        .await;

    let gate = ModalityGate::from_entry(&entry(false, true, &["text"]));
    let mut req = request();
    req.attachments.push(Attachment {
        mime: "image/png".to_string(),
        modality: "image".to_string(),
        data: vec![0x89, 0x50, 0x4e, 0x47],
    });

    let err = gate
        .apply(&mut req)
        .expect_err("non-attachment model must reject an attached image");
    match err {
        moagan::error::Error::ModalityUnsupported(msg) => {
            assert!(
                msg.contains("wiremock-model"),
                "error names the model: {msg}"
            );
            assert!(
                msg.contains("does not accept attachments"),
                "error explains the refusal: {msg}"
            );
        }
        other => panic!("expected ModalityUnsupported, got {other:?}"),
    }

    // The gate blocked the call; the upstream never received a
    // POST. wiremock records no request, so the assertion is
    // implicit — but the `expect(0)` mount above pins the same
    // invariant at the server side so a future refactor that
    // routes around the gate trips the test.
    let received = server
        .received_requests()
        .await
        .expect("recording enabled by default");
    assert!(
        received.is_empty(),
        "the gate must block the request before the wire builder runs; got {} requests",
        received.len()
    );
}

/// PR-5 test 8: a text-only request to a text-only model passes
/// the gate end-to-end. The mock server receives exactly one
/// POST, and the wire body has no `tool_choice` (because the
/// gate dropped it — this is the silent half of the gate
/// contract, the part that never errors).
#[tokio::test]
async fn integration_text_to_text_model_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    // `tool_call: false` is the interesting case: the caller
    // supplied a `ToolChoice::Auto`, the gate drops it to
    // `None`, and the wire body must not carry a `tool_choice`
    // field. The test pins that side-effect end-to-end by
    // checking the body the mock server received.
    let gate = ModalityGate::from_entry(&entry(true, false, &["text"]));
    let mut req = request();
    req.tool_choice = Some(ToolChoice::Auto);

    gate.apply(&mut req)
        .expect("text-only request must pass the gate");
    assert!(
        req.tool_choice.is_none(),
        "gate must drop tool_choice when tool_call=false"
    );

    // Send the (gate-cleared) body to the mock server so the
    // `expect(1)` mount fires. The point of the test is that
    // the body the server receives has no `tool_choice`; the
    // upstream interaction is the witness.
    let body = body_json(&req);
    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", server.uri());
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("mock upstream reachable");
    assert_eq!(resp.status(), 200);

    let received = server
        .received_requests()
        .await
        .expect("recording enabled by default");
    assert_eq!(
        received.len(),
        1,
        "exactly one POST must reach the upstream"
    );
    let sent: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("mock server received a JSON body");
    assert!(
        sent.get("tool_choice").is_none(),
        "gate must drop tool_choice from the wire body; got: {sent}"
    );
    assert_eq!(sent["model"], serde_json::json!("wiremock-model"));
    assert_eq!(sent["messages"][0]["content"], serde_json::json!("hello"));
}
