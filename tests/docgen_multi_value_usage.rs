//! A positional argument that takes several values shows `...` after
//! its name in the generated usage lines of `docs/cli-reference.md`.
//!
//! Gated behind `--features dev-tools`, like the `moagan-docgen` binary.

#![cfg(feature = "dev-tools")]

use std::process::Command;

fn cli_reference() -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_moagan-docgen"))
        .arg("cli")
        .output()
        .expect("run moagan-docgen cli");
    assert!(output.status.success(), "moagan-docgen cli failed");
    String::from_utf8(output.stdout).expect("docgen output is UTF-8")
}

#[test]
fn discover_merge_usage_says_it_takes_several_run_ids() {
    let doc = cli_reference();
    assert!(
        doc.contains("**Usage:** `moagan discover merge <RUN_ID>... [OPTIONS]`"),
        "usage line of `discover merge` lacks `<RUN_ID>...`"
    );
}

#[test]
fn a_single_value_positional_keeps_its_plain_name() {
    let doc = cli_reference();
    assert!(doc.contains("**Usage:** `moagan pause <RUN_ID>`"));
    assert!(doc.contains("**Usage:** `moagan coverage show <RUN_ID> [OPTIONS]`"));
    assert!(!doc.contains("`moagan pause <RUN_ID>...`"));
}
