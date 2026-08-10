//! Prompt registry. Versioned prompts, embedded into the binary as
//! `&'static str` constants. The `prompt_set_hash` is part of the
//! cache key so editing a prompt invalidates cached responses.

use std::sync::OnceLock;

use crate::discovery::epistemic_legacy::EpistemicLegacy;
use crate::ids::blake3_hex;

use super::role::Role;

/// Default `max_tokens` ceiling for every role and provider.
///
/// Raised from the previous per-role values (512..=32_768) to a single
/// `1_000_000` ceiling so prose-heavy roles no longer truncate
/// mid-thought. The Anthropic-compatible request path uses this number
/// verbatim; the OpenAI-compat provider additionally clamps to the
/// per-provider `ProviderConfig::max_tokens`, which by default is also
/// this constant.
pub const DEFAULT_MAX_TOKENS: u32 = 1_000_000;

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
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        Role::RecoveryExplainer => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.1,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        Role::RationaleExtractor => Some(RoleSettings {
            temperature: 0.2,
            top_p: 0.7,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        Role::TiefighterCritic => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.1,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        Role::PersonaPicker => Some(RoleSettings {
            temperature: 0.3,
            top_p: 0.9,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        Role::AnglePicker => Some(RoleSettings {
            temperature: 0.7,
            top_p: 0.95,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        // Track H batch-2: tiebreaker for the 3 base judges. Low
        // temperature keeps the call stable so snapshot diffs of
        // cluster disagreements are meaningful.
        Role::FinalDisagreement => Some(RoleSettings {
            temperature: 0.2,
            top_p: 0.85,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        // Track H batch-2 (commit 2): LLM re-call for malformed
        // JSON. Deterministic (T=0.0) so re-runs against the same
        // malformed text produce the same repair; top_p=0.5 leaves
        // a small headroom for tokens the local heuristic cannot
        // guess.
        Role::JsonRepairV2 => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.5,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        // Track H batch-2 (commit 3): prompt-injection guard.
        // Fully deterministic (T=0.0, top_p=0.1) so two
        // detectors on the same input agree — a flaky detector
        // would cause false negatives in the quarantine path.
        Role::HostilePromptDetector => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.1,
            max_tokens: DEFAULT_MAX_TOKENS,
            json_mode: true,
        }),
        // PR-C2: focused re-call on a truncated response. T=0.0 so
        // two continuations of the same excerpt produce the same
        // output (useful for snapshot diffs); top_p=0.5 leaves a
        // small headroom for tokens the iterative bracket repair
        // cannot guess.
        Role::Continuation => Some(RoleSettings {
            temperature: 0.0,
            top_p: 0.5,
            max_tokens: DEFAULT_MAX_TOKENS,
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
const FINAL_DISAGREEMENT_PROMPT: &str = include_str!("prompts/final_disagreement.md");
const JSON_REPAIR_V2_PROMPT: &str = include_str!("prompts/json_repair_v2.md");
const HOSTILE_PROMPT_DETECTOR_PROMPT: &str = include_str!("prompts/hostile_prompt_detector.md");
const CONTINUATION_PROMPT: &str = include_str!("prompts/continuation.md");

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
                FINAL_DISAGREEMENT_PROMPT,
                JSON_REPAIR_V2_PROMPT,
                HOSTILE_PROMPT_DETECTOR_PROMPT,
                CONTINUATION_PROMPT,
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
        Role::FinalDisagreement => FINAL_DISAGREEMENT_PROMPT,
        Role::JsonRepairV2 => JSON_REPAIR_V2_PROMPT,
        Role::HostilePromptDetector => HOSTILE_PROMPT_DETECTOR_PROMPT,
        Role::Continuation => CONTINUATION_PROMPT,
    }
}

/// Discovery-mode system prompt. Different from `system_prompt` because
/// the matrix phase uses a single shared prompt that varies only by
/// the `(dimension, facet)` injected into the user payload.
pub fn discover_matrix_system_prompt() -> &'static str {
    DISCOVER_MATRIX_PROMPT
}

/// Placeholder token that prompts embed when they want the current
/// epistemic legacy rendered inline. Substitute via
/// [`inject_epistemic_legacy`].
pub const EPISTEMIC_LEGACY_PLACEHOLDER: &str = "${epistemic_legacy}";

/// Substitute [`EPISTEMIC_LEGACY_PLACEHOLDER`] in `prompt` with the
/// rendered view of the current [`EpistemicLegacy`] loaded from
/// `<MOAGAN_HOME>/epistemic_legacy.json`. If the placeholder is not
/// present, `prompt` is returned unchanged.
pub fn inject_epistemic_legacy(prompt: &str) -> String {
    if !prompt.contains(EPISTEMIC_LEGACY_PLACEHOLDER) {
        return prompt.to_owned();
    }
    let legacy = EpistemicLegacy::load();
    prompt.replace(EPISTEMIC_LEGACY_PLACEHOLDER, &legacy.render_markdown())
}

/// Placeholder token that prompts embed when they want the user's
/// top-N preference ratings rendered inline. Substitute via
/// [`inject_epistemic_preferences`]. PR C.5 (K.3b).
pub const EPISTEMIC_PREFERENCES_PLACEHOLDER: &str = "${epistemic_preferences}";

/// Substitute [`EPISTEMIC_PREFERENCES_PLACEHOLDER`] in `prompt` with
/// the rendered Markdown view of the user's top-3 recent preference
/// ratings (or empty when the learning loop is disabled / the cache
/// is empty). If the placeholder is not present, `prompt` is
/// returned unchanged.
pub fn inject_epistemic_preferences(prompt: &str, user: &str) -> String {
    if !prompt.contains(EPISTEMIC_PREFERENCES_PLACEHOLDER) {
        return prompt.to_owned();
    }
    let block = crate::preferences::integration::render_preferences_block(user, 3);
    prompt.replace(EPISTEMIC_PREFERENCES_PLACEHOLDER, &block)
}

/// Placeholder token that prompts embed when they want a fetched
/// research snippet block rendered inline. Substitute via
/// [`inject_known_apis`]. Track K (D9): the bounded external research
/// fetcher (proposal-04 §4) returns redacted snippets that the
/// Sketch phase appends to the prompt so the model can ground
/// opinions in current docs.
pub const KNOWN_APIS_PLACEHOLDER: &str = "${known_apis}";

/// Placeholder token that prompts embed when they want the canonical
/// six-axis rubric rendered inline. Substitute via
/// [`inject_rubric`]. Track E (E2): the Judge and Critique prompts
/// share a single rubric so the LLM-side scoring contract cannot
/// drift between the two phases.
pub const RUBRIC_PLACEHOLDER: &str = "${rubric}";

/// Substitute [`RUBRIC_PLACEHOLDER`] in `prompt` with the rendered
/// Markdown view of [`crate::ranking::RUBRIC_ANCHORS`] (the
/// six-criterion rubric: correctness, completeness, feasibility,
/// safety, cost, clarity). When the placeholder is absent, `prompt`
/// is returned unchanged.
pub fn inject_rubric(prompt: &str) -> String {
    if !prompt.contains(RUBRIC_PLACEHOLDER) {
        return prompt.to_owned();
    }
    let block = crate::ranking::render_rubric_block();
    prompt.replace(RUBRIC_PLACEHOLDER, &block)
}

/// Substitute [`KNOWN_APIS_PLACEHOLDER`] in `prompt` with the rendered
/// Markdown of the supplied research snippets. When the placeholder
/// is absent, `prompt` is returned unchanged. When `snippets` is
/// empty, the placeholder is replaced with a marker line that
/// explicitly states "no research available" so the model is not
/// misled into believing the block was forgotten.
pub fn inject_known_apis(prompt: &str, snippets: &[crate::research::ResearchSnippet]) -> String {
    if !prompt.contains(KNOWN_APIS_PLACEHOLDER) {
        return prompt.to_owned();
    }
    let block = crate::research::render_known_apis_block(snippets);
    prompt.replace(KNOWN_APIS_PLACEHOLDER, &block)
}

/// Placeholder token the [`super::role::Role::Continuation`] prompt
/// embeds for the last ~500 bytes of the truncated response. The
/// continuation loop in
/// `phases::phase::call_with_retry_parse` substitutes this with
/// the real excerpt via [`render_continuation_prompt`] before the
/// request leaves the process so the LLM never sees the literal
/// `${last_excerpt}` token.
pub const CONTINUATION_LAST_EXCERPT_PLACEHOLDER: &str = "${last_excerpt}";

/// Render the continuation system prompt with `last_excerpt`
/// substituted into [`CONTINUATION_LAST_EXCERPT_PLACEHOLDER`]. The
/// helper centralises the substitution so the dispatcher does not
/// have to know which prompt file is in use — it just calls
/// `render_continuation_prompt(excerpt)`. Returns the prompt with
/// the placeholder replaced verbatim by `last_excerpt`. When the
/// source prompt does not embed the placeholder (which would only
/// happen on a corrupted prompt registry), the helper still
/// appends the excerpt on its own line so the model always sees
/// the input it was asked to continue from.
pub fn render_continuation_prompt(last_excerpt: &str) -> String {
    let base = system_prompt(Role::Continuation);
    if base.contains(CONTINUATION_LAST_EXCERPT_PLACEHOLDER) {
        base.replace(CONTINUATION_LAST_EXCERPT_PLACEHOLDER, last_excerpt)
    } else {
        format!("{base}\n\n{last_excerpt}")
    }
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
    fn final_disagreement_prompt_file_exists_and_is_non_empty() {
        // Track H batch-2: the D.7.1 catalog entry for the judge
        // tiebreaker ships with a real placeholder prompt.
        assert!(!FINAL_DISAGREEMENT_PROMPT.trim().is_empty());
    }

    #[test]
    fn json_repair_v2_prompt_file_exists_and_is_non_empty() {
        // Track H batch-2 (commit 2): the D.7.1 catalog entry for
        // the LLM re-call on malformed JSON ships with a real
        // placeholder prompt.
        assert!(!JSON_REPAIR_V2_PROMPT.trim().is_empty());
    }

    #[test]
    fn hostile_prompt_detector_prompt_file_exists_and_is_non_empty() {
        // Track H batch-2 (commit 3): the D.7.1 catalog entry for
        // the prompt-injection guard ships with a real placeholder
        // prompt.
        assert!(!HOSTILE_PROMPT_DETECTOR_PROMPT.trim().is_empty());
    }

    #[test]
    fn continuation_prompt_file_exists_and_is_non_empty() {
        // PR-C2: the focused-continuation role ships with a real
        // placeholder prompt. The dispatcher injects
        // `${last_excerpt}` at runtime via
        // [`render_continuation_prompt`].
        assert!(!CONTINUATION_PROMPT.trim().is_empty());
    }

    /// PR-C2: the continuation prompt template must contain the
    /// `${last_excerpt}` placeholder so the dispatcher can
    /// substitute the truncated payload at runtime. Pin that the
    /// placeholder is present in the bundled file so a future
    /// copy-paste mistake cannot silently regress the loop into a
    /// "no input" call.
    #[test]
    fn continuation_prompt_contains_last_excerpt_placeholder() {
        assert!(
            CONTINUATION_PROMPT.contains(CONTINUATION_LAST_EXCERPT_PLACEHOLDER),
            "continuation prompt must embed the {{last_excerpt}} placeholder"
        );
    }

    /// PR-C2: `render_continuation_prompt` substitutes the
    /// `${last_excerpt}` placeholder with the actual excerpt and
    /// returns the result. The literal placeholder must be gone
    /// after the call so the LLM never sees it; the excerpt must
    /// appear verbatim in the rendered prompt so the model picks
    /// up at the right byte offset.
    #[test]
    fn continuation_prompt_contains_last_excerpt() {
        let excerpt = "{\"a\":1,\"approach\":\"Sharded ledger";
        let rendered = render_continuation_prompt(excerpt);
        assert!(
            !rendered.contains(CONTINUATION_LAST_EXCERPT_PLACEHOLDER),
            "literal placeholder must be gone after substitution"
        );
        assert!(
            rendered.contains(excerpt),
            "rendered continuation prompt must contain the supplied excerpt verbatim"
        );
    }

    /// PR-C2: `system_prompt(Role::Continuation)` returns the raw
    /// template (with `${last_excerpt}` unsubstituted). Tests that
    /// want a renderable prompt must use
    /// `render_continuation_prompt` instead; this test pins the
    /// "raw template vs rendered prompt" split.
    #[test]
    fn continuation_system_prompt_is_raw_template_with_placeholder() {
        let raw = system_prompt(Role::Continuation);
        assert!(
            raw.contains(CONTINUATION_LAST_EXCERPT_PLACEHOLDER),
            "raw continuation prompt must still embed the placeholder"
        );
        assert_eq!(
            raw, CONTINUATION_PROMPT,
            "system_prompt(Role::Continuation) must return the bundled template verbatim"
        );
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

    #[test]
    fn inject_epistemic_legacy_substitutes_placeholder() {
        let template = "Hello\n${epistemic_legacy}\nWorld";
        let injected = inject_epistemic_legacy(template);
        // Empty legacy still renders the heading; placeholder must be gone.
        assert!(!injected.contains(EPISTEMIC_LEGACY_PLACEHOLDER));
        assert!(injected.contains("# Epistemic legacy"));
        assert!(injected.starts_with("Hello\n"));
        assert!(injected.ends_with("\nWorld"));
    }

    #[test]
    fn inject_epistemic_legacy_returns_unchanged_when_no_placeholder() {
        let template = "no placeholder here, only prose";
        let injected = inject_epistemic_legacy(template);
        assert_eq!(injected, template);
    }

    /// Track K (D9): the `${known_apis}` placeholder is substituted
    /// with the rendered Markdown block when the prompt embeds it.
    /// The block must include the source URL so the model can cite
    /// it, and the placeholder must be gone after the call.
    #[test]
    fn inject_known_apis_substitutes_placeholder_with_snippets() {
        let snippet = crate::research::ResearchSnippet {
            url: "https://docs.rs/serde".into(),
            content: "fn main() {}".into(),
            truncated: false,
        };
        let template = "before\n${known_apis}\nafter";
        let injected = inject_known_apis(template, std::slice::from_ref(&snippet));
        assert!(!injected.contains(KNOWN_APIS_PLACEHOLDER));
        assert!(injected.contains("https://docs.rs/serde"));
        assert!(injected.contains("fn main() {}"));
        assert!(injected.starts_with("before\n"));
        assert!(injected.ends_with("\nafter"));
    }

    /// Track K (D9): a prompt without the placeholder is returned
    /// verbatim. This is the cached-response path — the injector
    /// must never mutate a prompt that does not ask for research.
    #[test]
    fn inject_known_apis_returns_unchanged_when_no_placeholder() {
        let template = "plain prompt, no slot";
        let snippet = crate::research::ResearchSnippet {
            url: "https://docs.rs/x".into(),
            content: "x".into(),
            truncated: false,
        };
        let injected = inject_known_apis(template, std::slice::from_ref(&snippet));
        assert_eq!(injected, template);
    }

    /// Track K (D9): an empty snippet list collapses to a marker
    /// line so the prompt never contains the literal placeholder.
    /// The marker text is the contract the deliver phase reads to
    /// tell the user "research was requested but the fetch failed".
    #[test]
    fn inject_known_apis_empty_list_substitutes_no_research_marker() {
        let template = "before\n${known_apis}\nafter";
        let injected = inject_known_apis(template, &[]);
        assert!(!injected.contains(KNOWN_APIS_PLACEHOLDER));
        assert!(injected.contains("no research available"));
    }

    /// Track E (E2): when the Judge prompt embeds `${rubric}` the
    /// injector must replace it with the rendered six-axis Markdown
    /// block. The block must list every key from `RUBRIC_ANCHORS`
    /// and the placeholder must be gone after the call so the LLM
    /// never sees the literal token.
    #[test]
    fn judge_prompt_substitutes_rubric_placeholder() {
        let template = "# judge\n${rubric}\nrest";
        let injected = inject_rubric(template);
        assert!(!injected.contains(RUBRIC_PLACEHOLDER));
        for (k, _) in crate::ranking::RUBRIC_ANCHORS {
            assert!(
                injected.contains(&format!("**{k}**")),
                "injected judge prompt missing key {k}"
            );
        }
        assert!(injected.starts_with("# judge\n"));
        assert!(injected.ends_with("\nrest"));
    }

    /// Track E (E2): the Critique prompt substitutes `${rubric}`
    /// identically to the Judge prompt. Both prompts must share the
    /// same six-axis rubric so the LLM-side scoring contract is
    /// stable across phases.
    #[test]
    fn critique_prompt_substitutes_rubric_placeholder() {
        let template = "# critique\n${rubric}\ntail";
        let injected = inject_rubric(template);
        assert!(!injected.contains(RUBRIC_PLACEHOLDER));
        assert!(injected.contains("# Rubric anchors"));
        for (k, _) in crate::ranking::RUBRIC_ANCHORS {
            assert!(
                injected.contains(&format!("**{k}**")),
                "injected critique prompt missing key {k}"
            );
        }
        assert!(injected.starts_with("# critique\n"));
        assert!(injected.ends_with("\ntail"));
    }
}
