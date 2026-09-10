//! Integration snapshot tests for the `moagan-docgen` binary.
//!
//! Each test invokes the compiled `moagan-docgen` with one
//! subcommand (`cli`, `events`, `test-skips`) and asserts the
//! output is byte-identical to the corresponding committed doc
//! under `docs/`. The committed docs are the canonical source for
//! downstream consumers (dashboards, e2e tooling, public docs
//! site); a drift between the live `clap::Command` tree /
//! `Event<'a>` enum / codebase and the committed docs is what
//! these tests catch.
//!
//! These tests are gated behind `--features dev-tools`: the
//! `moagan-docgen` binary itself only compiles when that feature
//! is enabled, so the test would otherwise fail to find the
//! executable. They do NOT run by default — operators that want
//! to verify the docgen pipeline do
//! `cargo test --features dev-tools --test integration_docgen_snapshots`.
//!
//! The three CI drift gates
//! (`.github/workflows/{cli,events,test-skips}-doc-sync.yml`) are
//! the production enforcement path; these tests are the local
//! feedback loop.
//!
//! EPIC #852 closes #866 (cli), #867 (events), #868 (test-skips).

#![cfg(feature = "dev-tools")]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    let mut p = std::env::current_dir().expect("cwd");
    while !(p.join("Cargo.toml").is_file() && p.join("src/lib.rs").is_file()) {
        let parent = p.parent().expect("repo root").to_path_buf();
        p = parent;
    }
    p
}

fn run_docgen(sub: &str) -> String {
    let exe = env!("CARGO_BIN_EXE_moagan-docgen");
    let output = Command::new(exe)
        .arg(sub)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(repo_root())
        .output()
        .unwrap_or_else(|e| panic!("failed to run moagan-docgen {sub}: {e}"));
    assert!(
        output.status.success(),
        "moagan-docgen {sub} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("docgen output is UTF-8")
}

fn assert_matches_committed_doc(sub: &str, doc_relpath: &str) {
    let doc_path = repo_root().join(doc_relpath);
    let committed = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", doc_path.display()));
    let generated = run_docgen(sub);
    if committed != generated {
        // Show a small diff for easy diagnosis in CI logs.
        let diff = simple_diff(&committed, &generated);
        panic!(
            "doc drift for {doc_relpath} (subcommand `{sub}`).\n\
             Run `cargo run --features dev-tools --bin moagan-docgen -- {sub} > {doc_relpath}` and commit.\n\n\
             {diff}"
        );
    }
}

fn simple_diff(a: &str, b: &str) -> String {
    let mut out = String::new();
    let mut la = a.lines();
    let mut lb = b.lines();
    let mut n = 0;
    while let (Some(x), Some(y)) = (la.next(), lb.next()) {
        n += 1;
        if x != y {
            let _ = writeln!(out, "line {n}: committed={x:?} generated={y:?}");
        }
        if n > 50 {
            break;
        }
    }
    out
}

#[test]
fn cli_reference_matches_committed_doc() {
    assert_matches_committed_doc("cli", "docs/cli-reference.md");
}

#[test]
fn events_reference_matches_committed_doc() {
    assert_matches_committed_doc("events", "docs/events-reference.md");
}

#[test]
fn test_skips_report_matches_committed_doc() {
    assert_matches_committed_doc("test-skips", "docs/test-skips-report.md");
}

/// `moagan-docgen --help` (no subcommand) prints a one-page summary.
#[test]
fn help_lists_three_subcommands() {
    let stdout = run_docgen("--help");
    assert!(stdout.contains("cli"), "help missing `cli` subcommand");
    assert!(
        stdout.contains("events"),
        "help missing `events` subcommand"
    );
    assert!(
        stdout.contains("test-skips"),
        "help missing `test-skips` subcommand"
    );
}

/// `moagan-docgen bogus` exits non-zero (2) and prints the help.
#[test]
fn unknown_subcommand_exits_nonzero() {
    let exe = env!("CARGO_BIN_EXE_moagan-docgen");
    let output = Command::new(exe)
        .arg("bogus")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(repo_root())
        .output()
        .expect("run moagan-docgen bogus");
    assert!(!output.status.success(), "expected non-zero exit");
    assert_eq!(output.status.code(), Some(2), "exit code should be 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown subcommand"),
        "stderr did not mention unknown subcommand: {stderr}"
    );
}
