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

#[tokio::test]
async fn parallelism_acquire_many_owned_returns_correct_count() {
    let permits = Parallelism::new(4).acquire_many_owned(3).await.unwrap();
    assert_eq!(permits.len(), 3);
}
