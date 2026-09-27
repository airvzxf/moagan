//! End-to-end smoke test for the cost_overrides module (closes #965).
//!
//! Loads a hand-authored `<MOAGAN_HOME>/cost_overrides.toml` from disk,
//! passes it through `cost_estimate_with_overrides`, and prints the
//! per-call dollar total alongside the same calculation against the
//! catalog (None) — so the operator can SEE the override branch win
//! without waiting for the PhaseCtx wiring follow-up.
//!
//! This file is intentionally inside `tests/` so it is part of the
//! regression suite and surfaces in `cargo test` automatically. The
//! real runtime path (`cli/run.rs` -> `PhaseCtx` -> `cost_estimate`)
//! is the follow-up mentioned in the PR #968 body.

use moagan::llm::client::Usage;
use moagan::llm::cost::{CostOverrides, cost_estimate_with_overrides};
use std::path::PathBuf;

fn unique_tmp(label: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix(&format!("moagan-cost-overrides-smoke-{label}-"))
        .tempdir()
        .expect("tmp dir");
    let path = tmp.path().to_path_buf();
    (tmp, path)
}

#[test]
fn cost_overrides_smoke_full_path_through_disk_and_math() {
    let (_tmp, dir) = unique_tmp("full");
    let path = dir.join("cost_overrides.toml");
    std::fs::write(
        &path,
        r#"
schema_version = 1

[cost_overrides.minimax."MiniMax-M3"]
input       = 0.30
output      = 1.20
cache_read  = 0.06
cache_write = 0.375

[cost_overrides.minimax."MiniMax-M2.7"]
input       = 0.30
output      = 1.20
cache_read  = 0.03
cache_write = 0.375
"#,
    )
    .expect("write cost_overrides.toml fixture");

    // Real on-disk load — exactly what the runtime will do once the
    // PhaseCtx wiring lands.
    let overrides = CostOverrides::from_path(&path).expect("parse override file");

    // The MiniMax-M3 row from the v0.18.1 discovery benchmark (see
    // /home/wolf/workspace/moagan/new-arch-linux/run_discovery_analysis.txt):
    //   216 939 in, 88 601 out, 0 cache_read, 0 cache_creation.
    let usage_m3 = Usage {
        input_tokens: 216_939,
        output_tokens: 88_601,
        cache_read: 0,
        cache_creation: 0,
    };

    let cost_with_override = cost_estimate_with_overrides(
        None, // catalog absent — the v0.18.1 status quo for minimax
        Some(&overrides),
        "minimax",
        "MiniMax-M3",
        &usage_m3,
    );

    let cost_without_override = cost_estimate_with_overrides(
        None,
        None, // overrides absent
        "minimax",
        "MiniMax-M3",
        &usage_m3,
    );

    // Expected:
    //   ($0.30/M * 216 939) + ($1.20/M * 88 601) = $0.06508 + $0.10632 = $0.17140
    // Without override the catalog-miss path returns $0.00.
    let expected = (0.30 / 1e6) * 216_939.0 + (1.20 / 1e6) * 88_601.0;

    eprintln!("\n=== cost_overrides smoke (closes #965) ===");
    eprintln!("override file         : {}", path.display());
    eprintln!(
        "overrides.get(M3)     : present={}",
        overrides.get("minimax", "MiniMax-M3").is_some()
    );
    eprintln!("usage                 : in=216 939  out=88 601");
    eprintln!("with override         : ${cost_with_override:.6}");
    eprintln!("without override      : ${cost_without_override:.6}");
    eprintln!("expected (manual math): ${expected:.6}");
    eprintln!("==========================================\n");

    assert!(
        (cost_with_override - expected).abs() < 1e-6,
        "override branch must produce ${expected:.6}, got ${cost_with_override:.6}"
    );
    assert_eq!(
        cost_without_override, 0.0,
        "without overrides, the catalog-miss path must still return $0.00 (v0.18.1 status quo)"
    );
}
