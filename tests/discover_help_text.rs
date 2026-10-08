//! `moagan discover --help` describes the current pipeline and carries
//! no planning archaeology.

use clap::CommandFactory;
use moagan::cli::Cli;

fn discover_help() -> String {
    let mut cli = Cli::command();
    let discover = cli
        .find_subcommand_mut("discover")
        .expect("discover subcommand");
    discover.render_long_help().to_string()
}

#[test]
fn the_help_names_every_phase_of_the_pipeline_in_order() {
    let help = discover_help();
    let positions: Vec<Option<usize>> = [
        "intake",
        "discover_dimensions",
        "discover_sketches",
        "discover_curate",
        "discover_render",
    ]
    .iter()
    .map(|phase| help.find(phase))
    .collect();
    assert!(positions.iter().all(Option::is_some), "{help}");
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{help}");
}

#[test]
fn the_help_points_to_the_catalogue_and_the_operator_guide() {
    let help = discover_help();
    assert!(help.contains("<run>/final/"), "{help}");
    assert!(help.contains("docs/discover-guide.md"), "{help}");
}

#[test]
fn the_help_carries_no_planning_archaeology() {
    let help = discover_help();
    for stale in [
        "Track G.2",
        "Plan B",
        "knowledge base",
        "PR-D1",
        "Tanda",
        "F1 ",
        "F2 ",
        "F3 ",
    ] {
        assert!(!help.contains(stale), "`{stale}` in:\n{help}");
    }
}

#[test]
fn explain_promises_sketch_and_curator_counts() {
    let help = discover_help();
    assert!(
        help.contains("Print how many sketch and curator calls the run would make"),
        "{help}"
    );
}
