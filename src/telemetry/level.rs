//! D.17.3: `TelemetryLevel` enum.

/// Telemetry verbosity level.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TelemetryLevel {
    /// Telemetry disabled.
    Off,
    /// Lifecycle events only (start/end, budget/cancel).
    Summary,
    /// Every event reaches the sinks.
    Full,
}

impl Default for TelemetryLevel {
    /// Defaults to `Summary` (less noise than `Full` but still useful
    /// for post-mortem).
    fn default() -> Self {
        Self::Summary
    }
}

impl TelemetryLevel {
    /// Resolve the level from `MOAGAN_TELEMETRY`. Unknown values
    /// (including unset) fall back to `Summary`.
    pub fn from_env() -> Self {
        match std::env::var("MOAGAN_TELEMETRY").as_deref() {
            Ok("off") => Self::Off,
            Ok("full") => Self::Full,
            Ok("summary") => Self::Summary,
            _ => Self::default(),
        }
    }

    /// True when `Summary` and `Full` are both allowed.
    pub fn allows_summary(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// True when only `Full` is allowed.
    pub fn allows_full(self) -> bool {
        matches!(self, Self::Full)
    }
}
