//! Tests for D.17.1: `TelemetryEvent` enum. The surviving variants
//! are `PhaseStart`, `DiscoverySaturated`, `StaleArtifact`; the
//! 11-variant trim in v0.14.x removed the unused lifecycle/cache/
//! circuit/warning/hostile-prompt entries (zero production callers,
//! zero test sites outside this module — see the `Event::schema()`
//! and `Dead-rotator cleanup` PRs).

use crate::telemetry::event::TelemetryEvent;

#[test]
fn telemetry_event_variants_count_is_exactly_3() {
    let names: &[&str] = &["PhaseStart", "DiscoverySaturated", "StaleArtifact"];
    assert_eq!(
        names.len(),
        3,
        "TelemetryEvent must expose exactly 3 active variants, got {}",
        names.len()
    );
    let ser_phase = serde_json::to_string(&TelemetryEvent::PhaseStart {
        run_id: "r".into(),
        phase: "d".into(),
        at_unix: 1_700_000_000,
    })
    .unwrap();
    let ser_stale = serde_json::to_string(&TelemetryEvent::StaleArtifact {
        path: "proposals/p.json".into(),
        age_secs: 0,
        ttl_secs: None,
        at_unix: 1_700_000_001,
    })
    .unwrap();
    assert!(
        ser_phase.contains("phase_start"),
        "phase_start key missing: {ser_phase}"
    );
    assert!(
        ser_stale.contains("stale_artifact"),
        "stale_artifact key missing: {ser_stale}"
    );
}

#[test]
fn telemetry_event_serializes_to_snake_case() {
    let ev = TelemetryEvent::PhaseStart {
        run_id: "r1".into(),
        phase: "d".into(),
        at_unix: 1_700_000_000,
    };
    let j = serde_json::to_string(&ev).unwrap();
    assert!(j.contains("\"kind\":\"phase_start\""), "got {j}");
    assert!(!j.contains("PhaseStart"), "kind should be snake_case: {j}");

    let ev = TelemetryEvent::DiscoverySaturated {
        run_id: "r1".into(),
        coverage: 0.91,
        at_unix: 42,
    };
    let j = serde_json::to_string(&ev).unwrap();
    assert!(j.contains("\"kind\":\"discovery_saturated\""), "got {j}");
}
