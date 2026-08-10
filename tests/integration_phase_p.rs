use moagan::execution::Parallelism;
use moagan::llm::prompts::{role_settings, system_prompt};
use moagan::llm::role::Role;

#[test]
fn merge_synthesizer_prompt_is_registered() {
    assert!(!system_prompt(Role::MergeSynthesizer).is_empty());
    assert_eq!(
        role_settings(Role::MergeSynthesizer).unwrap().max_tokens,
        1_000_000
    );
}

#[test]
fn recovery_explainer_prompt_is_registered() {
    assert!(!system_prompt(Role::RecoveryExplainer).is_empty());
    assert_eq!(
        role_settings(Role::RecoveryExplainer).unwrap().temperature,
        0.0
    );
}

#[test]
fn rationale_extractor_prompt_is_registered() {
    assert!(!system_prompt(Role::RationaleExtractor).is_empty());
    assert_eq!(role_settings(Role::RationaleExtractor).unwrap().top_p, 0.7);
}

#[tokio::test]
async fn parallelism_acquire_many_owned_returns_correct_count() {
    let permits = Parallelism::new(4).acquire_many_owned(3).await.unwrap();
    assert_eq!(permits.len(), 3);
}
