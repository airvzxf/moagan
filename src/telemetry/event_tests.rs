//! Tests for D.17.1: `TelemetryEvent` enum.

use crate::telemetry::event::TelemetryEvent;

#[test]
fn telemetry_event_variants_count_is_at_least_15() {
    let names: &[&str] = &[
        "RunStart",
        "RunEnd",
        "PhaseStart",
        "PhaseEnd",
        "CallStart",
        "CallEnd",
        "DiscoverySaturated",
        "CacheHit",
        "CacheMiss",
        "CircuitOpen",
        "CircuitClose",
        "BudgetSoft",
        "BudgetHard",
        "Cancel",
        "StaleArtifact",
        "Warning",
        "HostilePrompt",
    ];
    assert!(
        names.len() >= 15,
        "TelemetryEvent must expose at least 15 variants, got {}",
        names.len()
    );
    let ser_warn = serde_json::to_string(&TelemetryEvent::Warning {
        run_id: "r".into(),
        code: "c".into(),
        message: "m".into(),
        at_unix: 0,
    })
    .unwrap();
    let ser_run = serde_json::to_string(&TelemetryEvent::RunStart {
        run_id: "r".into(),
        mode: "fast".into(),
        at_unix: 1,
    })
    .unwrap();
    assert!(
        ser_warn.contains("warning"),
        "warning key missing: {ser_warn}"
    );
    assert!(
        ser_run.contains("run_start"),
        "run_start key missing: {ser_run}"
    );
}

#[test]
fn telemetry_event_serializes_to_snake_case() {
    let ev = TelemetryEvent::RunStart {
        run_id: "r1".into(),
        mode: "fast".into(),
        at_unix: 1_700_000_000,
    };
    let j = serde_json::to_string(&ev).unwrap();
    assert!(j.contains("\"kind\":\"run_start\""), "got {j}");
    assert!(!j.contains("RunStart"), "kind should be snake_case: {j}");

    let ev = TelemetryEvent::DiscoverySaturated {
        run_id: "r1".into(),
        coverage: 0.91,
        at_unix: 42,
    };
    let j = serde_json::to_string(&ev).unwrap();
    assert!(j.contains("\"kind\":\"discovery_saturated\""), "got {j}");
}
