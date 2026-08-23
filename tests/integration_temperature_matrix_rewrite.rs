//! End-to-end regression tests for
//! [`ExplorationMatrix::rewrite_temperatures_to_supported`] —
//! the per-profile temperature rewrite the discovery
//! coordinator runs against the auto-discovered supported set
//! right before the matrix fan-out.
//!
//! The rewriter is the seam between the auto-probe and the
//! per-cell fan-out: it reshapes the operator's declared
//! temperature profile so the per-cell temperature buckets and
//! the cache-key cardinality reflect the post-clamp reality. The
//! three tests below pin the contract against the canonical
//! scenarios documented in the plan's analysis section:
//!
//! - Caso A: the upstream accepts only `{1.0}`. The profile
//!   `[0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9]` collapses to seven
//!   copies of `1.0`. `n_clamped` reports `6` because the
//!   requested `1.0` is already a no-op (the rewriter counts only
//!   entries whose value actually changes).
//! - Caso B: the upstream accepts `{0.2, 1.0, 1.8}`. The same
//!   profile snaps to `[0.2, 0.2, 1.0, 1.0, 1.0, 1.8, 1.8]` —
//!   the seven-element nearest-neighbour lookup the plan's
//!   "Análisis de vecinos más cercanos" table documents.
//! - Caso sin caché: no supported set is provided (the auto-probe
//!   ran on a uniformly-rejecting upstream and returned an empty
//!   set, or the table is empty). The rewriter must leave the
//!   profile untouched so the safety-net clamp at dispatch has
//!   something to fall back to.
//!
//! The tests build an [`ExplorationMatrix`] directly and call the
//! rewriter — the same code path the coordinator invokes at line
//! ~534 of `src/discovery/coordinator.rs`. Going through the CLI
//! `moagan discover` flow would only add HTTP-mock surface area
//! without exercising any additional rewriter logic; the unit
//! tests inside `matrix.rs` already pin the algorithm, and this
//! integration test pins the contract from outside the module.

use std::collections::HashMap;

use moagan::discovery::matrix::{
    Dimension, ExplorationMatrix, Facet, RewriteEvent, TemperatureProfile,
};

/// Build a minimal `Dimension` with one or two facets. Used to
/// give the matrix at least one cell so the post-rewrite profile
/// sits on a realistic matrix — the rewriter itself does not
/// inspect dimensions, but the production coordinator builds the
/// matrix from real dimensions before calling the rewriter, so
/// the integration test mirrors that shape.
fn dim(id: &str, facets: &[(&str, &str)]) -> Dimension {
    Dimension {
        id: id.to_owned(),
        label: id.to_owned(),
        facets: facets
            .iter()
            .map(|(fid, label)| Facet {
                id: (*fid).to_owned(),
                label: (*label).to_owned(),
            })
            .collect(),
    }
}

/// Caso A: supported set `{1.0}` only. The profile
/// `[0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9]` snaps to seven copies
/// of `1.0`; `n_clamped` is `6` because the requested `1.0` at
/// position 3 is already in the supported set (no clamp — the
/// rewriter only counts entries whose value actually changes).
///
/// The in-place mutation also has to land: after the rewrite the
/// matrix's stored profile carries the snapped temperatures, so
/// the per-cell fan-out iterates `7 × 1 = 7` cells against the
/// upstream at `temperature = 1.0` every time.
#[test]
fn case_a_kimi_k3_only_accepts_1_0() {
    let mut profiles = HashMap::new();
    profiles.insert(
        "kimi-k3".to_owned(),
        TemperatureProfile {
            temperatures: vec![0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9],
            replicas_per_temperature: 1,
        },
    );
    let mut matrix = ExplorationMatrix::new(
        vec![dim("deployment-model", &[("serverless", "serverless")])],
        1,
    );
    matrix.temperature_profiles = profiles;

    let mut supported_sets: HashMap<String, Vec<f32>> = HashMap::new();
    supported_sets.insert("kimi-k3".to_owned(), vec![1.0]);

    let events = matrix.rewrite_temperatures_to_supported(&supported_sets);
    assert_eq!(events.len(), 1, "one profile was rewritten");
    let e: &RewriteEvent = &events[0];
    assert_eq!(e.provider_model, "kimi-k3");
    assert_eq!(
        e.requested,
        vec![0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9],
        "event records the original operator-declared temperatures"
    );
    assert_eq!(
        e.clamped_to,
        vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        "every value snaps to the only supported temperature"
    );
    assert_eq!(
        e.n_clamped, 6,
        "n_clamped is 6, not 7 — the 1.0→1.0 entry is a no-op and the rewriter only counts actual changes"
    );
    // In-place mutation: the matrix profile now carries the
    // snapped temperatures, so the per-cell fan-out iterates
    // every cell at T=1.0 (matches the plan's claim that the
    // cardinalidad del matrix sigue siendo 7 = 7 × 1 réplica).
    let stored = matrix.profile_for("kimi-k3");
    assert_eq!(stored.temperatures, vec![1.0; 7]);
    assert_eq!(stored.replicas_per_temperature, 1);
}

/// Caso B: supported set `{0.2, 1.0, 1.8}`. Same profile maps
/// onto the seven-element nearest-neighbour table the plan's
/// analysis section documents:
///
/// ```text
/// requested | nearest | distance
/// ----------+---------+---------
/// 0.1       | 0.2     | 0.1
/// 0.3       | 0.2     | 0.1
/// 0.7       | 1.0     | 0.3
/// 1.0       | 1.0     | 0.0  (no-op)
/// 1.2       | 1.0     | 0.2
/// 1.5       | 1.8     | 0.3
/// 1.9       | 1.8     | 0.1
/// ```
///
/// Resulting profile: `[0.2, 0.2, 1.0, 1.0, 1.0, 1.8, 1.8]`.
/// `n_clamped = 6` (the `1.0→1.0` no-op doesn't count).
#[test]
fn case_b_set_0_2_1_0_1_8() {
    let mut profiles = HashMap::new();
    profiles.insert(
        "MiniMax-M3".to_owned(),
        TemperatureProfile {
            temperatures: vec![0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9],
            replicas_per_temperature: 1,
        },
    );
    let mut matrix = ExplorationMatrix::new(
        vec![dim("deployment-model", &[("serverless", "serverless")])],
        1,
    );
    matrix.temperature_profiles = profiles;

    let mut supported_sets: HashMap<String, Vec<f32>> = HashMap::new();
    supported_sets.insert("MiniMax-M3".to_owned(), vec![0.2, 1.0, 1.8]);

    let events = matrix.rewrite_temperatures_to_supported(&supported_sets);
    assert_eq!(events.len(), 1, "one profile was rewritten");
    let e: &RewriteEvent = &events[0];
    assert_eq!(e.provider_model, "MiniMax-M3");
    assert_eq!(e.requested, vec![0.1, 0.3, 0.7, 1.0, 1.2, 1.5, 1.9]);
    assert_eq!(
        e.clamped_to,
        vec![0.2, 0.2, 1.0, 1.0, 1.0, 1.8, 1.8],
        "nearest-neighbour lookup must match the plan's analysis table"
    );
    assert_eq!(e.n_clamped, 6);
    let stored = matrix.profile_for("MiniMax-M3");
    assert_eq!(stored.temperatures, vec![0.2, 0.2, 1.0, 1.0, 1.0, 1.8, 1.8]);
}

/// Caso sin caché: empty supported-set map (the auto-probe ran
/// against a uniformly-rejecting upstream and produced an empty
/// entry, or no probe has ever run for this model). The rewriter
/// emits no events and leaves the profile untouched so the
/// dispatch-time gate (the safety net) has the operator's
/// literal values to fall back to. Without this branch a missing
/// cache would silently drop the operator's profile — the
/// failure mode the production contract exists to prevent.
#[test]
fn case_no_cache_passes_profile_through() {
    let original_temps: Vec<f32> = vec![0.1, 0.5, 1.0];
    let mut profiles = HashMap::new();
    profiles.insert(
        "kimi-k3".to_owned(),
        TemperatureProfile {
            temperatures: original_temps.clone(),
            replicas_per_temperature: 1,
        },
    );
    let mut matrix = ExplorationMatrix::new(
        vec![dim("deployment-model", &[("serverless", "serverless")])],
        1,
    );
    matrix.temperature_profiles = profiles;

    // Empty supported-set map: the auto-probe ran with no signal
    // (or never ran). The rewriter must not mutate the profile.
    let supported_sets: HashMap<String, Vec<f32>> = HashMap::new();
    let events = matrix.rewrite_temperatures_to_supported(&supported_sets);
    assert!(
        events.is_empty(),
        "no events when no provider has a supported set"
    );
    let stored = matrix.profile_for("kimi-k3");
    assert_eq!(
        stored.temperatures, original_temps,
        "profile must pass through verbatim when the cache is empty"
    );
    assert_eq!(stored.replicas_per_temperature, 1);
}
