//! `moagan discover --explain` counts the curator calls: one per matrix
//! cell with at least two theses (one per chunk of at most 140 theses),
//! each retried at most once.

use moagan::cli::discover_explain::{ExplainInput, ValueSource, format_explain};

fn input(
    cells: usize,
    sketches_per_cell: usize,
    temperatures: usize,
    replicas: usize,
) -> ExplainInput {
    ExplainInput {
        cells,
        sketches_per_cell,
        temperatures,
        replicas,
        source_cells: ValueSource::Spec,
        source_sketches_per_cell: ValueSource::Flag,
        source_temperatures: ValueSource::Flag,
        source_replicas: ValueSource::Flag,
    }
}

#[test]
fn every_cell_with_two_or_more_theses_costs_one_curator_call() {
    assert_eq!(input(24, 10, 4, 2).curator_requests(), 24);
    assert_eq!(input(27, 2, 2, 1).curator_requests(), 27);
    assert_eq!(input(3, 2, 1, 1).curator_requests(), 3);
}

#[test]
fn cells_with_a_single_thesis_cost_no_curator_call() {
    assert_eq!(input(8, 1, 1, 1).curator_requests(), 0);
}

#[test]
fn a_cell_with_more_theses_than_one_call_takes_is_split_into_chunks() {
    assert_eq!(input(3, 140, 1, 1).curator_requests(), 3);
    assert_eq!(input(3, 141, 1, 1).curator_requests(), 6);
    assert_eq!(input(2, 10, 7, 3).curator_requests(), 4);
}

#[test]
fn the_results_block_names_the_curator_calls_and_their_retries() {
    let out = format_explain(&input(27, 2, 2, 1));
    assert!(
        out.ends_with("Requests LLM = 108\nSurviving sketches = ≤ 108\nCurator requests = 27 (+ up to 27 retries)"),
        "{out}"
    );
}

#[test]
fn the_results_block_says_why_there_is_no_curator_call() {
    let out = format_explain(&input(8, 1, 1, 1));
    assert!(
        out.ends_with("Curator requests = 0 (cells with fewer than 2 theses are not curated)"),
        "{out}"
    );
}

#[test]
fn an_llm_derived_matrix_shows_no_curator_line() {
    let out = format_explain(&input(0, 10, 1, 1));
    assert!(!out.contains("Curator requests"), "{out}");
}
