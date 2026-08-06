//! Tests for D.17.8: dashboard HTML.

use crate::telemetry::dashboard_static::{DASHBOARD_HTML, write_dashboard};

#[test]
fn dashboard_html_has_runs_div() {
    assert!(
        DASHBOARD_HTML.contains("<div id=\"runs\">"),
        "dashboard html must mount a #runs container"
    );
    assert!(
        DASHBOARD_HTML.contains("function load"),
        "dashboard html must define the load() function"
    );
    assert!(
        DASHBOARD_HTML.contains("/api/runs"),
        "dashboard html must call /api/runs"
    );
}

#[test]
fn dashboard_write_drops_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_dashboard(tmp.path()).unwrap();
    let out = tmp.path().join("dashboard.html");
    assert!(out.exists(), "write_dashboard must drop dashboard.html");
    let content = std::fs::read_to_string(&out).unwrap();
    assert_eq!(content, DASHBOARD_HTML);
}
