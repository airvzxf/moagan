//! Prompt registry. Versioned prompts, embedded into the binary as
//! `&'static str` constants. The `prompt_set_hash` is part of the
//! cache key so editing a prompt invalidates cached responses.

use std::sync::OnceLock;

use crate::ids::blake3_hex;

use super::role::Role;

/// Sampling settings registered for an opt-in role.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoleSettings {
    /// Sampling temperature.
    pub temperature: f32,
    /// Nucleus sampling threshold.
    pub top_p: f32,
    /// Output token ceiling.
    pub max_tokens: u32,
    /// Whether the role requires JSON output.
    pub json_mode: bool,
}

/// Return the catalogue settings for a role.
pub fn role_settings(role: Role) -> Option<RoleSettings> {
    match role {
        Role::MergeSynthesizer => Some(RoleSettings {
            temperature: 0.2,
            top_p: 0.7,
            max_tokens: 4000,
            json_mode: true,
        }),
        Role::RecoveryExplainer => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.1,
            max_tokens: 1000,
            json_mode: true,
        }),
        Role::RationaleExtractor => Some(RoleSettings {
            temperature: 0.2,
            top_p: 0.7,
            max_tokens: 1500,
            json_mode: true,
        }),
        Role::TiefighterCritic => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.1,
            max_tokens: 2048,
            json_mode: true,
        }),
        Role::PersonaPicker => Some(RoleSettings {
            temperature: 0.3,
            top_p: 0.9,
            max_tokens: 512,
            json_mode: true,
        }),
        Role::AnglePicker => Some(RoleSettings {
            temperature: 0.7,
            top_p: 0.95,
            max_tokens: 1024,
            json_mode: true,
        }),
        _ => None,
    }
}

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
const TAGGER_PROMPT: &str = include_str!("prompts/tag.md");
const FACET_DERIVER_PROMPT: &str = include_str!("prompts/facet_deriver.md");
const EXTRACTOR_PROMPT: &str = include_str!("prompts/extract.md");
const INTEGRATOR_PROMPT: &str = include_str!("prompts/integrate.md");
const DISCOVER_MATRIX_PROMPT: &str = include_str!("prompts/discover_matrix.md");
const SYNTHESIZE_PROMPT: &str = include_str!("prompts/synthesize.md");
const JUDGE_ADVERSARY_PROMPT: &str = include_str!("prompts/judge_adversary.md");
const DECOMPOSE_PROMPT: &str = include_str!("prompts/decompose.md");
const MERGE_SYNTHESIZER_PROMPT: &str = include_str!("prompts/merge_synthesizer.md");
const RECOVERY_EXPLAINER_PROMPT: &str = include_str!("prompts/recovery_explainer.md");
const RATIONALE_EXTRACTOR_PROMPT: &str = include_str!("prompts/rationale_extractor.md");
const TIEFIGHTER_CRITIC_PROMPT: &str = include_str!("prompts/tiefighter_critic.md");
const PERSONA_PICKER_PROMPT: &str = include_str!("prompts/persona_picker.md");
const ANGLE_PICKER_PROMPT: &str = include_str!("prompts/angle_picker.md");

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
                TAGGER_PROMPT,
                FACET_DERIVER_PROMPT,
                EXTRACTOR_PROMPT,
                INTEGRATOR_PROMPT,
                DISCOVER_MATRIX_PROMPT,
                SYNTHESIZE_PROMPT,
                JUDGE_ADVERSARY_PROMPT,
                DECOMPOSE_PROMPT,
                MERGE_SYNTHESIZER_PROMPT,
                RECOVERY_EXPLAINER_PROMPT,
                RATIONALE_EXTRACTOR_PROMPT,
                TIEFIGHTER_CRITIC_PROMPT,
                PERSONA_PICKER_PROMPT,
                ANGLE_PICKER_PROMPT,
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
        Role::Tagger => TAGGER_PROMPT,
        Role::FacetDeriver => FACET_DERIVER_PROMPT,
        Role::Extractor => EXTRACTOR_PROMPT,
        Role::Integrator => INTEGRATOR_PROMPT,
        Role::Synthesizer => SYNTHESIZE_PROMPT,
        Role::Adversary => JUDGE_ADVERSARY_PROMPT,
        Role::Decomposer => DECOMPOSE_PROMPT,
        Role::MergeSynthesizer => MERGE_SYNTHESIZER_PROMPT,
        Role::RecoveryExplainer => RECOVERY_EXPLAINER_PROMPT,
        Role::RationaleExtractor => RATIONALE_EXTRACTOR_PROMPT,
        Role::TiefighterCritic => TIEFIGHTER_CRITIC_PROMPT,
        Role::PersonaPicker => PERSONA_PICKER_PROMPT,
        Role::AnglePicker => ANGLE_PICKER_PROMPT,
    }
}

/// Discovery-mode system prompt. Different from `system_prompt` because
/// the matrix phase uses a single shared prompt that varies only by
/// the `(dimension, facet)` injected into the user payload.
pub fn discover_matrix_system_prompt() -> &'static str {
    DISCOVER_MATRIX_PROMPT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_synthesizer_prompt_file_exists_and_is_non_empty() {
        assert!(!MERGE_SYNTHESIZER_PROMPT.trim().is_empty());
    }

    #[test]
    fn recovery_explainer_prompt_file_exists_and_is_non_empty() {
        assert!(!RECOVERY_EXPLAINER_PROMPT.trim().is_empty());
    }

    #[test]
    fn rationale_extractor_prompt_file_exists_and_is_non_empty() {
        assert!(!RATIONALE_EXTRACTOR_PROMPT.trim().is_empty());
    }

    #[test]
    fn tiefighter_critic_prompt_file_exists_and_is_non_empty() {
        // Track H batch-1: the D.7.1 catalog entry for the
        // adversarial critic ships with a real placeholder prompt.
        assert!(!TIEFIGHTER_CRITIC_PROMPT.trim().is_empty());
    }

    #[test]
    fn persona_picker_prompt_file_exists_and_is_non_empty() {
        // Track H batch-1 (commit 2): persona selector carries its
        // own placeholder prompt.
        assert!(!PERSONA_PICKER_PROMPT.trim().is_empty());
    }

    #[test]
    fn angle_picker_prompt_file_exists_and_is_non_empty() {
        // Track H batch-1 (commit 3): exploration angle selector
        // carries its own placeholder prompt.
        assert!(!ANGLE_PICKER_PROMPT.trim().is_empty());
    }

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
