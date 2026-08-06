//! Tests for D.17.2: `TelemetryHub` + `TelemetrySink`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::telemetry::event::TelemetryEvent;
use crate::telemetry::hub::{StdoutSink, TelemetryHub, TelemetrySink};

#[test]
fn telemetry_hub_dispatches_to_all_sinks() {
    struct CountingSink {
        n: Arc<AtomicUsize>,
    }
    impl TelemetrySink for CountingSink {
        fn emit(&self, _event: &TelemetryEvent) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    let n1 = Arc::new(AtomicUsize::new(0));
    let n2 = Arc::new(AtomicUsize::new(0));
    let s1 = CountingSink { n: Arc::clone(&n1) };
    let s2 = CountingSink { n: Arc::clone(&n2) };

    let mut hub = TelemetryHub::new();
    hub.register(Box::new(s1));
    hub.register(Box::new(s2));
    hub.register(Box::new(StdoutSink));

    let ev = TelemetryEvent::RunStart {
        run_id: "r1".into(),
        mode: "fast".into(),
        at_unix: 1,
    };
    hub.dispatch(&ev);
    assert_eq!(n1.load(Ordering::SeqCst), 1);
    assert_eq!(n2.load(Ordering::SeqCst), 1);

    let ev2 = TelemetryEvent::RunEnd {
        run_id: "r1".into(),
        status: "ok".into(),
        at_unix: 2,
    };
    hub.dispatch(&ev2);
    assert_eq!(n1.load(Ordering::SeqCst), 2);
    assert_eq!(n2.load(Ordering::SeqCst), 2);
}
