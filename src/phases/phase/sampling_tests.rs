//! #1026: a role's own sampling wins over the provider section; the
//! section's `temperature` / `top_p` are only the default of the roles
//! that do not fix their own.

use std::collections::HashMap;

use super::{resolve_temperature, resolve_top_p, role_fixes_temperature, temperature_for_role};
use crate::llm::prompts::{role_settings, system_prompt};
use crate::llm::role::Role;

/// The default `[providers.minimax]` section (`src/config/mod.rs`).
const SECTION_TEMPERATURE: f32 = 0.6;
const SECTION_TOP_P: f32 = 0.95;

#[test]
fn the_dimension_deriver_keeps_its_deterministic_sampling_over_the_provider_section() {
    assert_eq!(
        resolve_temperature(
            Role::DimensionDeriver,
            None,
            Some(SECTION_TEMPERATURE),
            "minimax"
        ),
        Some(0.0)
    );
    assert_eq!(
        resolve_top_p(Role::DimensionDeriver, Some(SECTION_TOP_P)),
        Some(0.2)
    );
}

#[test]
fn the_curator_keeps_its_temperature_and_takes_the_provider_top_p() {
    assert_eq!(
        resolve_temperature(Role::Curator, None, Some(SECTION_TEMPERATURE), "minimax"),
        Some(0.2)
    );
    assert_eq!(
        resolve_top_p(Role::Curator, Some(SECTION_TOP_P)),
        Some(SECTION_TOP_P)
    );
    assert_eq!(resolve_top_p(Role::Curator, None), None);
}

#[test]
fn every_catalogue_role_keeps_its_catalogue_sampling_over_the_provider_section() {
    let catalogue: Vec<Role> = Role::all()
        .iter()
        .copied()
        .filter(|r| role_settings(*r).is_some())
        .collect();
    assert_eq!(catalogue.len(), 7, "catalogue roles: {catalogue:?}");
    for role in catalogue {
        let Some(settings) = role_settings(role) else {
            continue;
        };
        assert_eq!(
            resolve_temperature(role, None, Some(SECTION_TEMPERATURE), "minimax"),
            Some(settings.temperature),
            "{role:?} temperature"
        );
        assert_eq!(
            temperature_for_role(role, None),
            settings.temperature,
            "{role:?}: the role table and the catalogue disagree"
        );
        assert_eq!(
            resolve_top_p(role, Some(SECTION_TOP_P)),
            Some(settings.top_p),
            "{role:?} top_p"
        );
    }
}

#[test]
fn roles_that_fix_no_temperature_take_the_provider_section() {
    for role in [Role::Intake, Role::Sketch, Role::Propose, Role::Judge] {
        assert_eq!(
            resolve_temperature(role, None, Some(SECTION_TEMPERATURE), "minimax"),
            Some(SECTION_TEMPERATURE),
            "{role:?} temperature"
        );
        assert_eq!(
            resolve_top_p(role, Some(SECTION_TOP_P)),
            Some(SECTION_TOP_P),
            "{role:?} top_p"
        );
    }
    assert_eq!(
        resolve_temperature(Role::Intake, None, None, "minimax"),
        Some(0.4),
        "without a section the role table is the default"
    );
}

#[test]
fn only_the_catalogue_and_the_curator_fix_their_temperature() {
    let mut fixed: Vec<&str> = Role::all()
        .iter()
        .copied()
        .filter(|r| role_fixes_temperature(*r))
        .map(|r| r.as_str())
        .collect();
    fixed.sort_unstable();
    let mut want: Vec<&str> = [
        Role::Continuation,
        Role::Curator,
        Role::DimensionDeriver,
        Role::FinalDisagreement,
        Role::HostilePromptDetector,
        Role::JsonRepairV2,
        Role::MergeSynthesizer,
        Role::TiefighterCritic,
    ]
    .into_iter()
    .map(|r| r.as_str())
    .collect();
    want.sort_unstable();
    assert_eq!(fixed, want);
}

#[test]
fn a_profile_override_still_beats_a_role_that_fixes_its_temperature() {
    let mut map = HashMap::new();
    map.insert(Role::Curator.as_str().to_owned(), 0.5_f32);
    assert_eq!(
        resolve_temperature(
            Role::Curator,
            Some(&map),
            Some(SECTION_TEMPERATURE),
            "minimax"
        ),
        Some(0.5)
    );
}

/// The sampling a prompt promises ("temperature 0.0, top_p 0.2" or
/// "T=0.0, top_p=0.1") is the sampling the role is sent with.
#[test]
fn every_prompt_that_states_its_sampling_states_the_catalogue_values() {
    let mut checked = 0;
    for role in Role::all().iter().copied() {
        let Some(settings) = role_settings(role) else {
            continue;
        };
        let text = system_prompt(role);
        let temperature = [
            format!("temperature {:.1}", settings.temperature),
            format!("T={:.1}", settings.temperature),
        ];
        let top_p = [
            format!("top_p {}", settings.top_p),
            format!("top_p={}", settings.top_p),
        ];
        let states_any = text.contains("temperature ") && text.contains("top_p")
            || text.contains("T=") && text.contains("top_p=");
        if !states_any {
            continue;
        }
        checked += 1;
        assert!(
            temperature.iter().any(|t| text.contains(t.as_str())),
            "{role:?} prompt does not state temperature {}",
            settings.temperature
        );
        assert!(
            top_p.iter().any(|t| text.contains(t.as_str())),
            "{role:?} prompt does not state top_p {}",
            settings.top_p
        );
    }
    assert!(checked >= 5, "only {checked} prompts state their sampling");
}
