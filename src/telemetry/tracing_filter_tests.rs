//! Tests for D.17.9: tracing filter helper.

use crate::telemetry::tracing_filter::recommended_env_filter;

#[test]
fn tracing_filter_default_includes_moagan() {
    let prior = std::env::var("RUST_LOG").ok();
    unsafe {
        std::env::remove_var("RUST_LOG");
    }
    let v = recommended_env_filter();
    assert!(
        v.contains("moagan"),
        "default filter must mention moagan, got: {v}"
    );
    assert!(
        v.contains("moagan::telemetry"),
        "default filter must include telemetry module, got: {v}"
    );
    restore(prior);
}

#[test]
fn tracing_filter_respects_rust_log() {
    let prior = std::env::var("RUST_LOG").ok();
    unsafe {
        std::env::set_var("RUST_LOG", "warn,moagan=info");
    }
    let v = recommended_env_filter();
    assert_eq!(v, "warn,moagan=info");
    restore(prior);
}

fn restore(prior: Option<String>) {
    match prior {
        Some(v) => unsafe {
            std::env::set_var("RUST_LOG", v);
        },
        None => unsafe {
            std::env::remove_var("RUST_LOG");
        },
    }
}
