//! Wire-body prefill rules and the answer shapes MiniMax-M3 returns
//! with and without the `{` prefill (spike S1, 2026-10-04). The answer
//! texts keep the observed shapes with neutral content.

use super::*;
use crate::domain::{Intake, Sketch};
use crate::llm::Role;
use crate::llm::client::Message;
use crate::phases::util::parse_model_json;

fn request(role: Role, model: &str) -> LlmRequest {
    LlmRequest {
        role,
        model: model.into(),
        system: "system".into(),
        user: "user".into(),
        max_tokens: Some(1024),
        temperature: Some(0.4),
        top_p: None,
        top_k: None,
        response_schema: None,
        stream: false,
        extra_messages: Vec::new(),
        attachments: Vec::new(),
        tool_choice: None,
    }
}

fn wire_messages(req: &LlmRequest) -> Vec<serde_json::Value> {
    let body = serde_json::to_value(body_from_request(req)).unwrap();
    body["messages"].as_array().unwrap().clone()
}

#[test]
fn minimax_json_roles_are_sent_without_an_assistant_prefill() {
    for role in [Role::Intake, Role::Sketch, Role::DimensionDeriver] {
        let messages = wire_messages(&request(role, "MiniMax-M3"));
        assert_eq!(messages.len(), 1, "{role:?}: {messages:?}");
        assert_eq!(messages[0]["role"], "user");
    }
}

#[test]
fn prompt_prefill_models_still_get_the_brace_prefill() {
    let messages = wire_messages(&request(Role::Intake, "deepseek-v4-pro"));
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "{");
}

#[test]
fn caller_supplied_messages_are_sent_verbatim() {
    let mut req = request(Role::Intake, "MiniMax-M3");
    req.extra_messages = vec![Message {
        role: "assistant".into(),
        content: "{\"problem\":".into(),
    }];
    let messages = wire_messages(&req);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[1]["content"], "{\"problem\":");
}

#[test]
fn an_unprefilled_minimax_intake_answer_keeps_every_list() {
    let answer = "{\n  \"problem\": \"Map the decisions a parts web shop enables\",\n  \
         \"objectives\": [\"List every decision\", \"Mark what stays offline\"],\n  \
         \"constraints\": [\"Fixed budget\", \"No customer credit\", \"ERP is the source of truth\"],\n  \
         \"non_goals\": [\"No ranking\", \"No synthesis prose\"],\n  \
         \"open_questions\": [],\n  \"raw_prompt\": \"Map the decisions\"\n}";
    let intake: Intake = parse_model_json(answer).unwrap();
    assert_eq!(intake.constraints.len(), 3);
    assert_eq!(intake.non_goals.len(), 2);
}

#[test]
fn an_unprefilled_minimax_sketch_answer_in_a_json_fence_parses() {
    let answer = "```json\n{\n  \"thesis\": \"Publish list prices per SKU and keep counter discounts offline.\",\n  \
         \"key_decisions\": [\"list price per SKU\"],\n  \"architecture_outline\": \"The web shows the list price.\",\n  \
         \"assumptions\": [], \"strengths\": [\"simple\"], \"weaknesses\": [\"two prices\"],\n  \
         \"hard_constraint_check\": {\"C1\": true},\n  \"expected_validation\": \"Compare tickets.\"\n}\n```";
    let sketch: Sketch = parse_model_json(answer).unwrap();
    assert!(sketch.thesis.starts_with("Publish list prices"));
    assert_eq!(sketch.hard_constraint_check.get("C1"), Some(&true));
}
