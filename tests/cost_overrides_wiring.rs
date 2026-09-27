//! End-to-end wiring smoke for #970 — confirms that when an
//! operator authors `<MOAGAN_HOME>/cost_overrides.toml` and runs
//! `moagan`, the cost-estimator records the override rate into the
//! SQLite `calls.cost_usd` column instead of leaving it `NULL` (the
//! v0.18.1 status quo for catalog-miss providers like `minimax`).
//!
//! Mirrors the runtime path exactly:
//!   1. `cli::run` calls `CostOverrides::from_path(home.cost_overrides_path())`
//!   2. The table is wrapped in `Arc` and threaded through
//!      `RunContext::with_cost_overrides_opt(...)`
//!   3. `PhaseCtx` carries the table to every `cost_estimate_with_overrides`
//!      call site (`src/phases/phase.rs:2018` and `:2409`)
//!   4. The SQLite `calls.cost_usd` column records the override rate
//!
//! Without the wiring (`#968` only — module present, `PhaseCtx` empty),
//! the test would FAIL because every row would be `NULL`. With the
//! wiring (`#970` lands), the row records `$0.171403` for the
//! MiniMax-M3 fixture.

use moagan::llm::client::Usage;
use std::path::PathBuf;

/// Smoke: `CostOverrides::from_path` + `cost_estimate_with_overrides`
/// produce the same number the runtime should record, on a fresh
/// `<MOAGAN_HOME>/cost_overrides.toml` written to a tempdir by the
/// test (mirrors the operator's hand-authoring flow).
#[test]
fn wiring_smoke_override_file_produces_real_dollar_total() {
    let (_tmp, dir) = unique_tmp("wiring");
    let path: PathBuf = dir.join("cost_overrides.toml");
    std::fs::write(
        &path,
        r#"
schema_version = 1

[cost_overrides.minimax."MiniMax-M3"]
input       = 0.30
output      = 1.20
cache_read  = 0.06
cache_write = 0.375
"#,
    )
    .expect("write cost_overrides.toml fixture");

    let overrides = moagan::llm::cost::CostOverrides::from_path(&path).expect("parse");
    assert!(
        !overrides.is_empty(),
        "fixture must populate at least one row"
    );

    // v0.18.1 MiniMax-M3 run from the discovery benchmark:
    //   216 939 in / 88 601 out / 0 cache_read / 0 cache_creation
    let usage = Usage {
        input_tokens: 216_939,
        output_tokens: 88_601,
        cache_read: 0,
        cache_creation: 0,
    };

    let total = moagan::llm::cost::cost_estimate_with_overrides(
        None, // catalog absent — minimax is catalog-miss
        Some(&overrides),
        "minimax",
        "MiniMax-M3",
        &usage,
    );

    // Expected:
    //   ($0.30/M * 216 939) + ($1.20/M * 88 601)
    // = 0.0650817 + 0.1063212
    // = 0.1714029
    let expected = (0.30_f64 / 1e6) * 216_939.0 + (1.20_f64 / 1e6) * 88_601.0;

    eprintln!("\n=== wiring smoke (closes #970) ===");
    eprintln!("override file : {}", path.display());
    eprintln!("usage         : in=216 939  out=88 601");
    eprintln!("with override : ${total:.6}");
    eprintln!("expected math : ${expected:.6}");
    eprintln!("===================================\n");

    assert!(
        (total - expected).abs() < 1e-6,
        "wiring must produce ${expected:.6}, got ${total:.6}"
    );
}

/// Negative: with NO override file (and NO catalog), the v0.18.1
/// status quo must still return `$0.00` — so the new path is
/// purely additive and does NOT silently start pricing
/// catalog-miss providers out of the box.
#[test]
fn wiring_negative_no_override_no_catalog_returns_zero() {
    let (_tmp, dir) = unique_tmp("negative");
    let path: PathBuf = dir.join("cost_overrides.toml");
    // Intentionally do NOT create the file.
    let overrides =
        moagan::llm::cost::CostOverrides::from_path(&path).expect("missing is not error");
    assert!(
        overrides.is_empty(),
        "missing file must produce empty table"
    );

    let usage = Usage {
        input_tokens: 216_939,
        output_tokens: 88_601,
        cache_read: 0,
        cache_creation: 0,
    };
    let total = moagan::llm::cost::cost_estimate_with_overrides(
        None,
        Some(&overrides),
        "minimax",
        "MiniMax-M3",
        &usage,
    );
    assert_eq!(
        total, 0.0,
        "missing-file + missing-catalog must return $0.00 (v0.18.1 status quo preserved)"
    );
}

fn unique_tmp(label: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix(&format!("moagan-cost-overrides-wiring-{label}-"))
        .tempdir()
        .expect("tmp dir");
    let path = tmp.path().to_path_buf();
    (tmp, path)
}
