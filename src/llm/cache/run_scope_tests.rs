//! #1030: the cache key of a discover sketch call is scoped to its run.

use super::Cache;
use crate::llm::client::Request;
use crate::llm::role::Role;

fn sketch_request() -> Request {
    Request {
        role: Role::Sketch,
        model: "MiniMax-M3".to_owned(),
        system: "system prompt".to_owned(),
        user: "Exploration cell: storage / engine (storage:engine)".to_owned(),
        max_tokens: Some(4096),
        temperature: Some(1.0),
        top_p: Some(0.95),
        top_k: None,
        response_schema: None,
        stream: false,
        extra_messages: vec![],
        attachments: vec![],
        tool_choice: None,
    }
}

#[test]
fn the_same_sketch_payload_has_a_different_key_in_another_run() {
    let req = sketch_request();
    assert_ne!(
        Cache::run_scoped_key(&req, "minimax", "MiniMax-M3", "run-a"),
        Cache::run_scoped_key(&req, "minimax", "MiniMax-M3", "run-b")
    );
}

#[test]
fn the_same_sketch_payload_has_the_same_key_within_one_run() {
    let req = sketch_request();
    assert_eq!(
        Cache::run_scoped_key(&req, "minimax", "MiniMax-M3", "run-a"),
        Cache::run_scoped_key(&req, "minimax", "MiniMax-M3", "run-a")
    );
}

#[test]
fn a_run_scoped_key_still_tells_payloads_and_providers_apart() {
    let req = sketch_request();
    let mut other = sketch_request();
    other.user.push_str(" (another cell)");
    let key = Cache::run_scoped_key(&req, "minimax", "MiniMax-M3", "run-a");
    assert_ne!(
        key,
        Cache::run_scoped_key(&other, "minimax", "MiniMax-M3", "run-a")
    );
    assert_ne!(
        key,
        Cache::run_scoped_key(&req, "opencode", "MiniMax-M3", "run-a")
    );
    assert_ne!(key, Cache::cache_key(&req, "minimax", "MiniMax-M3"));
}
