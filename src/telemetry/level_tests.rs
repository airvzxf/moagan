//! Tests for D.17.3: `TelemetryLevel`.

use crate::telemetry::level::TelemetryLevel;

#[test]
fn telemetry_level_from_env_off() {
    let prior = std::env::var("MOAGAN_TELEMETRY").ok();
    unsafe {
        std::env::set_var("MOAGAN_TELEMETRY", "off");
    }
    assert_eq!(TelemetryLevel::from_env(), TelemetryLevel::Off);
    restore(prior);
}

#[test]
fn telemetry_level_from_env_full() {
    let prior = std::env::var("MOAGAN_TELEMETRY").ok();
    unsafe {
        std::env::set_var("MOAGAN_TELEMETRY", "full");
    }
    assert_eq!(TelemetryLevel::from_env(), TelemetryLevel::Full);
    restore(prior);
}

#[test]
fn telemetry_level_from_env_default() {
    let prior = std::env::var("MOAGAN_TELEMETRY").ok();
    unsafe {
        std::env::remove_var("MOAGAN_TELEMETRY");
    }
    assert_eq!(TelemetryLevel::from_env(), TelemetryLevel::Summary);

    unsafe {
        std::env::set_var("MOAGAN_TELEMETRY", "garbage");
    }
    assert_eq!(TelemetryLevel::from_env(), TelemetryLevel::Summary);
    restore(prior);
}

fn restore(prior: Option<String>) {
    match prior {
        Some(v) => unsafe {
            std::env::set_var("MOAGAN_TELEMETRY", v);
        },
        None => unsafe {
            std::env::remove_var("MOAGAN_TELEMETRY");
        },
    }
}
