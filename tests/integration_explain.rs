//! F3 (Track G.2 `discover --explain`): integration tests for
//! the `--explain` short-circuit.
//!
//! The explain path must:
//!
//! 1. Print the explain table for the resolved config WITHOUT
//!    launching the discovery pipeline.
//! 2. Reject `--sketches-per-cell < 1` exactly the same way the
//!    real run does (so the operator gets the same error message).
//! 3. Reject a malformed `--matrix-spec` exactly the same way the
//!    real run does (so the explain path never silently papers
//!    over a typo).
//! 4. Surface `cells = 0` (Source = `Llm`) when the operator only
//!    passes `--llm-derive` — no fake numbers are fabricated.
//!
//! The tests are pure: they call `build_explain_input` /
//! `build_and_format` directly and exercise clap's `--explain`
//! parsing. No filesystem, no `MOAGAN_HOME`, no real LLM — the
//! dispatcher unit tests already cover the wiring.

use moagan::cli::discover::DiscoverOptions;
use moagan::cli::discover_explain::{
    ExplainInput, ValueSource, build_and_format, build_explain_input, format_explain,
};
use moagan::config::Config;

fn opts_minimal() -> DiscoverOptions {
    DiscoverOptions {
        provider: "mock".to_string(),
        prompt: "x".to_string(),
        home: None,
        mock_dir: None,
        sketches_per_cell: 10,
        max_parallelism: None,
        dimensions: None,
        facets_per_dimension: None,
        matrix_spec: Vec::new(),
        llm_derive: false,
        cluster_threshold: 0.7,
        out_dir: None,
        non_interactive: false,
        cache_facets: false,
        temperature_profiles: Vec::new(),
        explain: true,
    }
}

/// F3: `moagan discover --explain --matrix-spec '...'` resolves
/// cells = sum(spec facets) with `Source::Spec`. The integration
/// test confirms the same arithmetic the plan documents.
#[test]
fn explain_with_matrix_spec_reports_spec_source() {
    let mut opts = opts_minimal();
    opts.matrix_spec = vec!["auth=oauth,api-key;scaling=vertical,horizontal,auto".to_string()];
    let cfg = Config::default();
    let input = build_explain_input(&opts, &cfg).unwrap();
    assert_eq!(input.cells, 5);
    assert_eq!(input.source_cells, ValueSource::Spec);
    // 5 cells × 10 spc × 1 × 1 = 50.
    assert_eq!(input.requests_llm(), 50);

    let rendered = format_explain(&input);
    assert!(rendered.contains("| facets            |    5 | Spec    |"));
    assert!(rendered.contains("5 × 10 × 1 × 1 = 50"));
    assert!(rendered.contains("Requests LLM = 50"));
    assert!(rendered.contains("Surviving sketches = ≤ 50"));
}

/// F3: a bare `moagan discover --explain` (no flags) reports the
/// historical 4×2 fallback as `Default` source. This is the
/// "honest fallback" the plan documents — operators see the
/// budget they would have seen before F1 made facet counts
/// asymmetric.
#[test]
fn explain_no_flags_uses_default_fallback() {
    let cfg = Config::default();
    let input = build_explain_input(&opts_minimal(), &cfg).unwrap();
    assert_eq!(input.cells, 8);
    assert_eq!(input.source_cells, ValueSource::Default);
    assert_eq!(input.source_sketches_per_cell, ValueSource::Default);
    assert_eq!(input.source_temperatures, ValueSource::Default);
    assert_eq!(input.source_replicas, ValueSource::Default);
    assert_eq!(input.requests_llm(), 80);
}

/// F3: `--llm-derive` alone reports cells = 0 with `Source::Llm`
/// so the operator is not lied to. The "Results" block is
/// omitted from the formatted output so no fake `0 × ... = 0`
/// number leaks.
#[test]
fn explain_llm_derive_omits_results_block() {
    let mut opts = opts_minimal();
    opts.llm_derive = true;
    let cfg = Config::default();
    let input = build_explain_input(&opts, &cfg).unwrap();
    assert_eq!(input.cells, 0);
    assert_eq!(input.source_cells, ValueSource::Llm);

    let rendered = format_explain(&input);
    assert!(
        !rendered.contains("Results\n-------"),
        "Results block must be omitted when cells == 0; got:\n{rendered}"
    );
    assert!(
        !rendered.contains("Requests LLM"),
        "Requests LLM line must be omitted when cells == 0; got:\n{rendered}"
    );
}

/// F3: a malformed `--matrix-spec` is rejected at the explain
/// layer (the same `Error::InvalidArgs` the real run would
/// produce). The integration test confirms the error surfaces
/// through `build_and_format` and is not silently turned into a
/// default cells count.
#[test]
fn explain_invalid_spec_returns_invalid_args_error() {
    let mut opts = opts_minimal();
    opts.matrix_spec = vec!["not-a-spec".to_string()];
    let cfg = Config::default();
    let err = build_and_format(&opts, &cfg).unwrap_err();
    assert!(
        err.to_string().contains("missing `=`"),
        "invalid spec must surface a parse error; got {err}"
    );
}

/// F3: `moagan discover --explain --sketches-per-cell 0` errors
/// with the floor message (now "below the minimum of 1"), exactly as the real run would.
/// The dispatcher enforces this for the pipeline; the explain
/// helper re-checks it so the contract is the same on both
/// surfaces. v0.13.2 lowered the floor from 10 to 1, so the
/// only rejected value is `0`.
#[test]
fn explain_below_minimum_sketches_per_cell_rejects() {
    let mut opts = opts_minimal();
    opts.sketches_per_cell = 0;
    let cfg = Config::default();
    let err = build_and_format(&opts, &cfg).unwrap_err();
    assert!(
        err.to_string().contains("below the minimum of 1"),
        "explain path must enforce the operator-facing floor; got {err}"
    );
}

/// F3: the CLI flag is parsed by clap and surfaces as
/// `Cmd::Discover { explain, .. }`. The dispatcher wires it to
/// the explain short-circuit BEFORE `discover::run` is invoked.
#[test]
fn clap_parses_explain_flag() {
    use clap::Parser;
    let cli = moagan::cli::Cli::try_parse_from([
        "moagan",
        "discover",
        "--provider",
        "mock",
        "--prompt",
        "x",
        "--explain",
    ])
    .expect("clap must accept --explain");
    match cli.cmd {
        moagan::cli::Cmd::Discover { explain, .. } => {
            assert!(explain, "--explain must round-trip to Cmd::Discover");
        }
        other => panic!("expected Cmd::Discover, got {other:?}"),
    }
}

/// F3: omitting `--explain` keeps the default `false`. Operators
/// who never set the flag keep the existing pipeline-launch
/// behaviour.
#[test]
fn clap_omitted_explain_defaults_to_false() {
    use clap::Parser;
    let cli = moagan::cli::Cli::try_parse_from([
        "moagan",
        "discover",
        "--provider",
        "mock",
        "--prompt",
        "x",
    ])
    .expect("clap must parse");
    match cli.cmd {
        moagan::cli::Cmd::Discover { explain, .. } => {
            assert!(!explain, "default for --explain must remain false");
        }
        other => panic!("expected Cmd::Discover, got {other:?}"),
    }
}

/// F3: `ExplainInput::requests_llm` matches the matrix's
/// `cells() × sketches_per_cell × temperatures × replicas` formula
/// for non-trivial inputs. The "24 × 10 × 4 × 2 = 1920" line in
/// the plan is the canonical example.
#[test]
fn requests_llm_matches_plan_example() {
    let input = ExplainInput {
        cells: 24,
        sketches_per_cell: 10,
        temperatures: 4,
        replicas: 2,
        source_cells: ValueSource::Default,
        source_sketches_per_cell: ValueSource::Flag,
        source_temperatures: ValueSource::Env,
        source_replicas: ValueSource::Env,
    };
    assert_eq!(input.requests_llm(), 1920);
}

/// F3: the `Value` column header reads "facets" but the numeric
/// value is `cells` (= sum of facet counts per dimension). This
/// pins the "facets ≠ dims × facets" contract: the F1 spec
/// collapses the sum so the formula shows a single number, not
/// a misleading Cartesian product.
#[test]
fn facets_column_uses_cells_not_product() {
    // Asymmetric matrix: 2+3+4 = 9 cells. If the explain ever
    // accidentally computed dims × facets (3 dims × ? facets),
    // the cells value would silently drift from 9.
    let mut opts = opts_minimal();
    opts.matrix_spec = vec!["a=x,y;b=p,q,r;s=1,2,3,4".to_string()];
    let cfg = Config::default();
    let input = build_explain_input(&opts, &cfg).unwrap();
    assert_eq!(
        input.cells, 9,
        "cells must be the sum (2+3+4), not dims × facets"
    );
    assert_eq!(input.source_cells, ValueSource::Spec);
}
