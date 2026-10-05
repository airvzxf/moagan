use super::*;

use std::collections::HashMap;
use std::path::Path;

use crate::discovery::catalog::build_catalog;
use crate::discovery::matrix::{
    Dimension, DimensionFacetDescription, ExplorationMatrix, Facet, TemperatureProfile,
};

const GOLDEN_README: &str = include_str!("../../../tests/fixtures/discover_render/README.md");
const GOLDEN_LIST: &str = include_str!("../../../tests/fixtures/discover_render/pricing/list.md");
const GOLDEN_MARGIN: &str =
    include_str!("../../../tests/fixtures/discover_render/pricing/margin.md");
const GOLDEN_ANNEX: &str =
    include_str!("../../../tests/fixtures/discover_render/constraints-annex.md");
const GOLDEN_CATALOG: &str = include_str!("../../../tests/fixtures/discover_render/catalog.json");

fn facet(id: &str, label: &str) -> Facet {
    Facet {
        id: id.into(),
        label: label.into(),
    }
}

/// One dimension, two cells; two sorted-on-render profiles.
fn matrix() -> ExplorationMatrix {
    let mut m = ExplorationMatrix::new(
        vec![Dimension {
            id: "pricing".into(),
            label: "Pricing".into(),
            facets: vec![facet("list", "List price"), facet("margin", "Margin")],
        }],
        2,
    );
    let mut profiles = HashMap::new();
    profiles.insert(
        "minimax::MiniMax-M3".to_owned(),
        TemperatureProfile {
            temperatures: vec![0.7, 1.0],
            replicas_per_temperature: 1,
        },
    );
    profiles.insert(
        "deepseek::deepseek-v4".to_owned(),
        TemperatureProfile {
            temperatures: vec![0.2],
            replicas_per_temperature: 2,
        },
    );
    m.temperature_profiles = profiles;
    m
}

fn descriptions() -> Vec<DimensionFacetDescription> {
    vec![DimensionFacetDescription::new(
        "pricing",
        "margin",
        "How the margin is protected when costs move.",
    )]
}

fn constraints() -> Vec<String> {
    vec!["Fixed budget".into(), "No customer credit".into()]
}

fn checks(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
}

/// sk_0001: full sketch with provenance and two failed constraints.
/// sk_0002: old sketch (no provenance), thesis only.
/// sk_0003: second cell, every constraint met.
fn sketches() -> Vec<Sketch> {
    vec![
        Sketch {
            id: "sk_0003".into(),
            thesis: "Protect the margin with a floor price per supplier.".into(),
            key_decisions: vec!["Floor price per supplier".into()],
            architecture_outline: "A nightly job recomputes floors.".into(),
            hard_constraint_check: checks(&[("C10", true), ("C2", true), ("C1", true)]),
            angle: "pricing:margin".into(),
            provenance: Some(SketchProvenance {
                section: "minimax".into(),
                model: "MiniMax-M3".into(),
                temperature: 1.0,
                replica: 0,
                index: 0,
            }),
            ..Sketch::default()
        },
        Sketch {
            id: "sk_0001".into(),
            thesis: "Publish list prices from the supplier feed, rounded to the cent.".into(),
            key_decisions: vec![
                "Supplier feed is the source".into(),
                "Round to the cent".into(),
            ],
            architecture_outline: "Import the feed, apply one rule, publish.".into(),
            assumptions: vec!["The feed is daily".into()],
            strengths: vec!["Simple".into()],
            weaknesses: vec!["Feed errors reach the shop".into()],
            hard_constraint_check: checks(&[
                ("C1", true),
                ("C2", false),
                ("C10", false),
                ("legal", false),
            ]),
            expected_validation: "Compare 50 published prices with the feed.".into(),
            angle: "pricing:list".into(),
            provenance: Some(SketchProvenance {
                section: "minimax".into(),
                model: "MiniMax-M3".into(),
                temperature: 0.7,
                replica: 0,
                index: 1,
            }),
        },
        Sketch {
            id: "sk_0002".into(),
            thesis: "Let the sales team set | list prices by hand.".into(),
            angle: "pricing:list".into(),
            ..Sketch::default()
        },
    ]
}

fn render_fixture() -> Vec<(PathBuf, String)> {
    let matrix = matrix();
    let sketches = sketches();
    let catalog = build_catalog(&matrix, &descriptions(), &sketches);
    let constraints = constraints();
    render(&RenderInput {
        run_id: "run-0001",
        problem: "Design the spare-parts price list.",
        constraints: &constraints,
        matrix: &matrix,
        catalog: &catalog,
        sketches: &sketches,
    })
}

fn file(files: &[(PathBuf, String)], path: &str) -> String {
    files
        .iter()
        .find(|(p, _)| p == Path::new(path))
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| panic!("no rendered file {path}"))
}

#[test]
fn files_come_in_a_fixed_order() {
    let paths: Vec<PathBuf> = render_fixture().into_iter().map(|(p, _)| p).collect();
    let expected: Vec<PathBuf> = [
        "README.md",
        "pricing/list.md",
        "pricing/margin.md",
        "constraints-annex.md",
        "catalog.json",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(paths, expected);
}

#[test]
fn readme_matches_the_golden_file() {
    assert_eq!(file(&render_fixture(), "README.md"), GOLDEN_README);
}

#[test]
fn facet_file_with_two_theses_matches_the_golden_file() {
    assert_eq!(file(&render_fixture(), "pricing/list.md"), GOLDEN_LIST);
}

#[test]
fn facet_file_with_a_description_matches_the_golden_file() {
    assert_eq!(file(&render_fixture(), "pricing/margin.md"), GOLDEN_MARGIN);
}

#[test]
fn constraint_annex_matches_the_golden_file() {
    assert_eq!(
        file(&render_fixture(), "constraints-annex.md"),
        GOLDEN_ANNEX
    );
}

#[test]
fn catalog_json_matches_the_golden_file() {
    assert_eq!(file(&render_fixture(), "catalog.json"), GOLDEN_CATALOG);
}

#[test]
fn rendering_twice_is_byte_identical() {
    assert_eq!(render_fixture(), render_fixture());
}

#[test]
fn readme_counts_come_from_the_catalogue_not_from_the_sketch_list() {
    let matrix = matrix();
    let all = sketches();
    let listed: Vec<Sketch> = all.iter().filter(|s| s.id != "sk_0002").cloned().collect();
    let catalog = build_catalog(&matrix, &[], &listed);
    let files = render(&RenderInput {
        run_id: "run-0001",
        problem: "p",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &all,
    });
    let readme = file(&files, "README.md");
    assert!(
        readme.contains("- Theses in this catalogue: 2 of 3 sketches (66.7 %)\n"),
        "{readme}"
    );
    assert!(readme.contains("| Pricing | [List price](pricing/list.md) | 1 | 1 | 0 | 1 |\n"));
}

#[test]
fn profiles_are_listed_in_sorted_key_order() {
    let readme = file(&render_fixture(), "README.md");
    let deepseek = readme.find("- Profile `deepseek::deepseek-v4`").unwrap();
    let minimax = readme.find("- Profile `minimax::MiniMax-M3`").unwrap();
    assert!(deepseek < minimax);
}

#[test]
fn a_matrix_without_profiles_shows_the_default_profile() {
    let mut matrix = matrix();
    matrix.temperature_profiles.clear();
    let catalog = build_catalog(&matrix, &[], &[]);
    let files = render(&RenderInput {
        run_id: "r",
        problem: "p",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &[],
    });
    let readme = file(&files, "README.md");
    assert!(readme.contains("\n- Profile `default`: T=1.0 × 1 replica(s)\n"));
    assert!(readme.contains("- Theses in this catalogue: 0 of 0 sketches (0.0 %)\n"));
}

#[test]
fn an_old_sketch_has_no_provenance_line() {
    let list = file(&render_fixture(), "pricing/list.md");
    let old = &list[list.find("### sk_0002").unwrap()..];
    assert!(!old.contains("T="));
    assert!(list.contains("T=0.7 · minimax/MiniMax-M3 · replica 0 · index 1"));
}

#[test]
fn constraint_checks_use_natural_order() {
    let list = file(&render_fixture(), "pricing/list.md");
    assert!(list.contains("**Constraint check.** C1 ✓ · C2 ✗ · C10 ✗ · legal ✗\n"));
}

#[test]
fn an_empty_problem_shows_a_placeholder() {
    let matrix = matrix();
    let catalog = build_catalog(&matrix, &[], &[]);
    let files = render(&RenderInput {
        run_id: "r",
        problem: "  ",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &[],
    });
    assert!(file(&files, "README.md").contains("\n_No problem statement in brief.json._\n"));
}

#[test]
fn an_annex_without_failed_checks_says_so() {
    let matrix = matrix();
    let only_ok: Vec<Sketch> = sketches()
        .into_iter()
        .filter(|s| s.id == "sk_0003")
        .collect();
    let catalog = build_catalog(&matrix, &[], &only_ok);
    let files = render(&RenderInput {
        run_id: "r",
        problem: "p",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &only_ok,
    });
    assert!(
        file(&files, "constraints-annex.md")
            .ends_with("\n\nNo thesis marks a constraint as not met.\n")
    );
    assert!(file(&files, "README.md").contains(
        "[Constraint annex](constraints-annex.md): 0 theses mark at least one constraint as not met.\n"
    ));
}

#[test]
fn an_empty_cell_still_gets_a_facet_file() {
    let matrix = matrix();
    let catalog = build_catalog(&matrix, &[], &[]);
    let files = render(&RenderInput {
        run_id: "r",
        problem: "p",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &[],
    });
    assert_eq!(
        file(&files, "pricing/list.md"),
        "# Pricing → List price\n\n0 theses.\n"
    );
}

#[test]
fn sketches_outside_the_matrix_get_their_own_facet_file() {
    let matrix = matrix();
    let mut all = sketches();
    all[1].angle = "pricing:retired".into();
    let catalog = build_catalog(&matrix, &[], &all);
    let files = render(&RenderInput {
        run_id: "r",
        problem: "p",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &all,
    });
    let outside = file(&files, "outside-matrix/unmatched.md");
    assert!(outside.starts_with("# Outside the matrix → Unmatched angle\n\n1 thesis.\n"));
    assert!(outside.contains("\n### sk_0001\n"));
    let readme = file(&files, "README.md");
    assert!(readme.contains("- Cells: 2 (2 with theses)\n"));
    assert!(readme.contains(
        "| Outside the matrix | [Unmatched angle](outside-matrix/unmatched.md) | 1 | 1 | 0 | 1 |\n"
    ));
}

#[test]
fn facet_paths_never_leave_the_final_directory() {
    assert_eq!(
        facet_path("pricing", "list"),
        PathBuf::from("pricing/list.md")
    );
    assert_eq!(facet_path("../etc", "a b"), PathBuf::from("---etc/a-b.md"));
    assert_eq!(facet_path("", "x/y"), PathBuf::from("-/x-y.md"));
}

#[test]
fn every_rendered_file_ends_with_one_newline() {
    for (path, text) in render_fixture() {
        assert!(
            text.ends_with('\n') && !text.ends_with("\n\n"),
            "{}",
            path.display()
        );
    }
}
