//! Tests for D.17.1: `TelemetryEvent` enum. The surviving variants
//! are `PhaseStart`, `DiscoverySaturated`, `StaleArtifact`; the
//! 11-variant trim in v0.14.x removed the unused lifecycle/cache/
//! circuit/warning/hostile-prompt entries (zero production callers,
//! zero test sites outside this module — see the v0.14.x cluster
//! that closes #706, #707, #708, #709, #710, #717).

use crate::telemetry::event::TelemetryEvent;

#[test]
fn telemetry_event_active_variants_round_trip_to_distinct_kinds() {
    let variants: Vec<TelemetryEvent> = vec![
        TelemetryEvent::PhaseStart {
            run_id: "r".into(),
            phase: "p".into(),
            at_unix: 0,
        },
        TelemetryEvent::DiscoverySaturated {
            run_id: "r".into(),
            coverage: 0.5,
            at_unix: 0,
        },
        TelemetryEvent::StaleArtifact {
            path: "p".into(),
            age_secs: 0,
            ttl_secs: None,
            at_unix: 0,
        },
    ];
    let kinds: std::collections::HashSet<String> = variants
        .iter()
        .map(|v| {
            let j = serde_json::to_string(v).unwrap();
            let needle = "\"kind\":\"";
            let start = j.find(needle).expect("kind field present") + needle.len();
            let end = j[start..].find('"').unwrap() + start;
            j[start..end].to_string()
        })
        .collect();
    assert_eq!(
        kinds.len(),
        3,
        "expected 3 distinct kind tags, got {kinds:?}"
    );
    for k in ["phase_start", "discovery_saturated", "stale_artifact"] {
        assert!(kinds.contains(k), "missing kind: {k}");
    }
}

#[test]
fn telemetry_event_stale_artifact_emits_ttl_secs_when_some() {
    let ev = TelemetryEvent::StaleArtifact {
        path: "p".into(),
        age_secs: 0,
        ttl_secs: Some(60),
        at_unix: 0,
    };
    let j = serde_json::to_string(&ev).unwrap();
    assert!(
        j.contains("\"kind\":\"stale_artifact\""),
        "missing kind: {j}"
    );
    assert!(j.contains("\"ttl_secs\":60"), "ttl_secs missing: {j}");
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
