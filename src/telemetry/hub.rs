//! D.17.2: `TelemetryHub` + `TelemetrySink` trait.

use std::sync::Mutex;

use crate::telemetry::event::TelemetryEvent;

/// A sink consumes telemetry events. `Send + Sync` so the hub can hold
/// a heterogeneous list behind a single mutex.
#[allow(missing_docs)]
pub trait TelemetrySink: Send + Sync {
    /// Handle one event. Implementations MUST be fast and infallible
    /// (log + drop on internal error rather than propagating).
    fn emit(&self, event: &TelemetryEvent);
}

/// Reference sink that pipes events through `TelemetryEvent::emit`
/// (i.e. `tracing::info!`).
#[allow(missing_docs)]
pub struct StdoutSink;

impl TelemetrySink for StdoutSink {
    fn emit(&self, event: &TelemetryEvent) {
        event.emit();
    }
}

/// Hub that fans events out to a registered set of sinks.
#[allow(missing_docs)]
#[derive(Default)]
pub struct TelemetryHub {
    sinks: Mutex<Vec<Box<dyn TelemetrySink>>>,
}

impl TelemetryHub {
    /// Build an empty hub.
    pub fn new() -> Self {
        Self {
            sinks: Mutex::new(Vec::new()),
        }
    }

    /// Convenience constructor with the default `StdoutSink` registered.
    pub fn with_stdout() -> Self {
        let mut h = Self::new();
        h.register(Box::new(StdoutSink));
        h
    }

    /// Register a sink.
    pub fn register(&mut self, sink: Box<dyn TelemetrySink>) {
        self.sinks.lock().unwrap().push(sink);
    }

    /// Dispatch one event to every registered sink in registration order.
    pub fn dispatch(&self, event: &TelemetryEvent) {
        for sink in self.sinks.lock().unwrap().iter() {
            sink.emit(event);
        }
    }
}

impl Clone for TelemetryHub {
    fn clone(&self) -> Self {
        Self::new()
    }
}
