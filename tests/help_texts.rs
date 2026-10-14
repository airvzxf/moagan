//! Shared-flag and `preflight` help texts state current behaviour only
//! (#1007): no decision kind that is never emitted, no migration
//! history, no discover concepts removed in v0.20/v0.21.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

fn help(args: &[&str]) -> String {
    let out = Command::new(binary())
        .args(args)
        .arg("--help")
        .env("MOAGAN_QUIET", "1")
        .output()
        .expect("run moagan --help");
    assert!(out.status.success(), "{args:?} --help failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The paragraph of `--<flag>` in a long help: from its line to the
/// next flag line.
fn flag_paragraph(help: &str, flag: &str) -> String {
    let start = help
        .find(&format!("--{flag}"))
        .unwrap_or_else(|| panic!("--{flag} in help"));
    let rest = &help[start..];
    let end = rest[2..].find("\n      --").map_or(rest.len(), |i| i + 2);
    rest[..end].to_owned()
}

#[test]
fn decision_format_lists_only_emitted_decisions() {
    let text = flag_paragraph(&help(&["run"]), "decision-format");
    for gone in ["category_assigned", "cache_hit"] {
        assert!(!text.contains(gone), "{gone} is never emitted:\n{text}");
    }
    for kept in [
        "winner_picked",
        "low_confidence_winner",
        "cluster_skipped",
        "repair_applied",
        "portfolio_finalized",
        "cache_miss",
        "judge_verdict",
    ] {
        assert!(text.contains(kept), "{kept} is emitted:\n{text}");
    }
}

#[test]
fn log_to_stderr_states_current_behaviour_without_history() {
    let text = flag_paragraph(&help(&["run"]), "log-to-stderr");
    for gone in [
        "v0.12.0",
        "v0.13",
        "v0.15.0",
        "PR-04a",
        "#657",
        "BoolishValueParser",
    ] {
        assert!(!text.contains(gone), "history {gone:?} in:\n{text}");
    }
    assert!(text.contains("1> out.jsonl 2> errors.jsonl"), "{text}");
}

#[test]
fn preflight_help_describes_the_current_pipeline() {
    let text = help(&["preflight"]);
    for gone in [
        "coordinator",
        "matrix build",
        "tag →",
        "cluster →",
        "facets →",
        "cardinalidad",
        "PR #565",
    ] {
        assert!(!text.contains(gone), "removed concept {gone:?} in:\n{text}");
    }
    for kept in ["curator", "--context", "propose"] {
        assert!(text.contains(kept), "{kept:?} missing in:\n{text}");
    }
}
