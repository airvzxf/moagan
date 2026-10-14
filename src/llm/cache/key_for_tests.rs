//! Which answers are cached per run: the theses and the proposals.
//! Every other role keeps the cross-run key.

use super::Cache;
use crate::llm::client::Request;
use crate::llm::role::Role;

fn request(role: Role) -> Request {
    Request {
        role,
        model: "MiniMax-M3".to_owned(),
        system: "system prompt".to_owned(),
        user: "user prompt".to_owned(),
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
fn only_theses_and_proposals_are_cached_per_run() {
    let per_run: Vec<Role> = Role::all()
        .iter()
        .copied()
        .filter(|r| r.caches_per_run())
        .collect();
    assert_eq!(per_run, vec![Role::Sketch, Role::Propose]);
}

#[test]
fn a_proposal_payload_has_a_different_key_in_another_run() {
    let req = request(Role::Propose);
    assert_ne!(
        Cache::key_for(&req, "minimax", "MiniMax-M3", "run-a"),
        Cache::key_for(&req, "minimax", "MiniMax-M3", "run-b")
    );
}

#[test]
fn a_thesis_payload_uses_the_run_scoped_key() {
    let req = request(Role::Sketch);
    assert_eq!(
        Cache::key_for(&req, "minimax", "MiniMax-M3", "run-a"),
        Cache::run_scoped_key(&req, "minimax", "MiniMax-M3", "run-a")
    );
}

#[test]
fn a_critique_payload_keeps_its_cross_run_key() {
    let req = request(Role::Critique);
    assert_eq!(
        Cache::key_for(&req, "minimax", "MiniMax-M3", "run-a"),
        Cache::cache_key(&req, "minimax", "MiniMax-M3")
    );
    assert_eq!(
        Cache::key_for(&req, "minimax", "MiniMax-M3", "run-a"),
        Cache::key_for(&req, "minimax", "MiniMax-M3", "run-b")
    );
}
