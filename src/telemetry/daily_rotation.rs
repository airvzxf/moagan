//! D.17.4: daily log rotation helper.

use std::sync::Mutex;
use std::time::SystemTime;

/// Tracks the last-observed day. Used by retention sweeps to detect
/// day-rollover and emit a stale-artifact warning.
#[allow(missing_docs)]
#[derive(Default)]
pub struct DailyRotator {
    pub(crate) last_day: Mutex<i64>,
}

impl DailyRotator {
    /// Build a new rotator initialized to today.
    pub fn new() -> Self {
        Self {
            last_day: Mutex::new(current_day()),
        }
    }

    /// Returns `true` once per day-rollover.
    pub fn check_rotate(&self) -> bool {
        let today = current_day();
        let mut last = self.last_day.lock().unwrap();
        if today != *last {
            *last = today;
            tracing::warn!(
                kind = "stale_artifact",
                path = "telemetry/daily.log",
                age_secs = 0,
                "log rotated (day rollover)"
            );
            true
        } else {
            false
        }
    }
}

/// Unix-epoch day count (`secs / 86400`).
#[allow(missing_docs)]
fn current_day() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0) as i64
}

impl std::fmt::Debug for DailyRotator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let last = *self.last_day.lock().unwrap();
        f.debug_struct("DailyRotator")
            .field("last_day", &last)
            .finish()
    }
}
