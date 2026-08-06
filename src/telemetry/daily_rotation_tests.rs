//! Tests for D.17.4: daily log rotation helper.

use crate::telemetry::daily_rotation::DailyRotator;

#[test]
fn daily_rotator_rotates_on_day_change() {
    let r = DailyRotator::new();
    assert!(!r.check_rotate());
    assert!(!r.check_rotate());

    *r.last_day.lock().unwrap() -= 1;
    assert!(r.check_rotate());
    assert!(!r.check_rotate());
}
