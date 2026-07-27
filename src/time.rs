//! Wall-clock helpers. Centralised so production code and tests agree on
//! the same epoch source.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in seconds. Returns 0 if the system clock is
/// before the epoch (which only happens on broken systems).
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current Unix time in milliseconds. Returns 0 if the system clock is
/// before the epoch.
pub fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_secs_is_positive() {
        // After 2026-01-01 the value is well above 1.7e9.
        let s = now_unix_secs();
        assert!(s > 1_700_000_000, "got {s}");
    }

    #[test]
    fn unix_millis_consistent_with_secs() {
        let s = now_unix_secs();
        let ms = now_unix_millis();
        assert!((ms / 1000 - s).abs() <= 1);
    }
}
