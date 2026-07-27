//! Prompt registry. Versioned prompts, embedded into the binary as
//! `&'static str` constants. The `prompt_set_hash` is part of the
//! cache key so editing a prompt invalidates cached responses.

use std::sync::OnceLock;

use crate::ids::blake3_hex;

use super::role::Role;

const INTAKE_PROMPT: &str = include_str!("prompts/intake.md");
const CLARIFY_PROMPT: &str = include_str!("prompts/clarify.md");
const ROUTE_PROMPT: &str = include_str!("prompts/route.md");
const SKETCH_PROMPT: &str = include_str!("prompts/sketch.md");
const PROPOSE_PROMPT: &str = include_str!("prompts/propose.md");
const GATE_PROMPT: &str = include_str!("prompts/gate.md");
const CRITIQUE_PROMPT: &str = include_str!("prompts/critique.md");
const REPAIR_PROMPT: &str = include_str!("prompts/repair.md");
const JUDGE_PROMPT: &str = include_str!("prompts/judge.md");
const RANK_PROMPT: &str = include_str!("prompts/rank.md");
const DELIVER_PROMPT: &str = include_str!("prompts/deliver.md");

static PROMPT_SET_HASH: OnceLock<String> = OnceLock::new();

/// Hash of the bundled prompt set. Used in cache keys so changing a
/// prompt invalidates cached responses.
pub fn prompt_set_hash() -> String {
    PROMPT_SET_HASH
        .get_or_init(|| {
            let all = [
                INTAKE_PROMPT,
                CLARIFY_PROMPT,
                ROUTE_PROMPT,
                SKETCH_PROMPT,
                PROPOSE_PROMPT,
                GATE_PROMPT,
                CRITIQUE_PROMPT,
                REPAIR_PROMPT,
                JUDGE_PROMPT,
                RANK_PROMPT,
                DELIVER_PROMPT,
            ]
            .join("\u{1f}");
            blake3_hex(all.as_bytes())
        })
        .clone()
}

/// Get the system prompt for `role`.
pub fn system_prompt(role: Role) -> &'static str {
    match role {
        Role::Intake => INTAKE_PROMPT,
        Role::Clarify => CLARIFY_PROMPT,
        Role::Route => ROUTE_PROMPT,
        Role::Sketch => SKETCH_PROMPT,
        Role::Propose => PROPOSE_PROMPT,
        Role::Gate => GATE_PROMPT,
        Role::Critique => CRITIQUE_PROMPT,
        Role::Repair => REPAIR_PROMPT,
        Role::Judge => JUDGE_PROMPT,
        Role::Rank => RANK_PROMPT,
        Role::Deliver => DELIVER_PROMPT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_set_hash_is_stable() {
        let a = prompt_set_hash();
        let b = prompt_set_hash();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn all_prompts_load() {
        for r in Role::all() {
            assert!(!system_prompt(*r).is_empty(), "empty prompt for {r}");
        }
    }
}
