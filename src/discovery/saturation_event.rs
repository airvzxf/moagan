//! D.13.7: event emitted when a discovery run reaches saturation.
//! Caller (DiscoveryCoordinator::run) publishes via tracing.

use serde::Serialize;

/// Tracing-friendly event payload for "discovery loop has reached
/// the target sketch count". Serializes to JSON for the telemetry
/// pipeline and is also rendered through [`Self::emit`] as a
/// `tracing::info!` record tagged with `event = "discovery_saturated"`.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoverySaturated {
    /// Run identifier (UUID v7 string).
    pub run_id: String,
    /// Number of sketches that actually completed.
    pub sketches_completed: usize,
    /// Final coverage ratio in `[0.0, 1.0]`.
    pub coverage: f32,
    /// Unix epoch seconds when saturation was detected.
    pub at_unix: i64,
}

impl DiscoverySaturated {
    /// Publish the event via `tracing::info!`. The macro fields
    /// carry every payload field so log aggregators can index them
    /// without re-parsing the message body.
    pub fn emit(&self) {
        tracing::info!(
            event = "discovery_saturated",
            run_id = %self.run_id,
            sketches_completed = self.sketches_completed,
            coverage = self.coverage,
            "DiscoverySaturated"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_saturated_serializes_to_json() {
        let evt = DiscoverySaturated {
            run_id: "run-123".to_string(),
            sketches_completed: 10,
            coverage: 1.0,
            at_unix: 1_700_000_000,
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"run_id\":\"run-123\""));
        assert!(json.contains("\"sketches_completed\":10"));
        assert!(json.contains("\"coverage\":1.0"));
        assert!(json.contains("\"at_unix\":1700000000"));
    }

    #[test]
    fn discovery_saturated_emits_tracing_event() {
        let evt = DiscoverySaturated {
            run_id: "run-emit".to_string(),
            sketches_completed: 4,
            coverage: 0.8,
            at_unix: 1_700_000_001,
        };
        evt.emit();
    }
}
