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
fn every_thesis_shows_its_provenance_or_says_it_was_not_recorded() {
    let list = file(&render_fixture(), "pricing/list.md");
    assert!(list.contains("\n### sk_0001\n\nT=0.7 · minimax/MiniMax-M3 · replica 0 · index 1\n"));
    assert!(list.contains(
        "\n### sk_0002\n\n_Provenance not recorded (model, temperature, replica, index)._\n"
    ));
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

// ------------------------------------------------------------ curated

const GOLDEN_CURATED_README: &str =
    include_str!("../../../tests/fixtures/discover_render/curated/README.md");
const GOLDEN_CURATED_LIST: &str =
    include_str!("../../../tests/fixtures/discover_render/curated/pricing/list.md");

/// The fixture sketches plus sk_0004, a second supplier-feed thesis.
fn curated_sketches() -> Vec<Sketch> {
    let mut all = sketches();
    all.push(Sketch {
        id: "sk_0004".into(),
        thesis: "Publish the supplier feed price as the list price.".into(),
        key_decisions: vec!["Supplier feed is the source".into()],
        angle: "pricing:list".into(),
        ..Sketch::default()
    });
    all
}

/// `pricing:list` curated into two groups: sk_0004 repeats sk_0001,
/// and the feed and the sales team are in tension.
fn list_curation(status_ok: bool) -> crate::discovery::curation::Curation {
    use crate::discovery::curation::{Curation, RawCuration, normalise};
    let ids: Vec<String> = ["sk_0001", "sk_0002", "sk_0004"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let raw: RawCuration = serde_json::from_value(serde_json::json!({
        "groups": [
            {"label": "Supplier feed", "summary": "The feed sets the price; members differ on rounding.", "representative": 0},
            {"label": "Hand-set by sales", "summary": "People set the price.", "representative": 1}
        ],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 1}, {"n": 2, "g": 0}],
        "duplicates": [[2, 0]],
        "tensions": [[0, 1, "The feed and the sales team cannot both own the list price."]]
    }))
    .unwrap();
    let chunk = status_ok.then(|| normalise(&ids, &raw));
    Curation::merge("pricing:list", ids, vec![chunk])
}

fn render_curated(status_ok: bool) -> Vec<(PathBuf, String)> {
    let matrix = matrix();
    let sketches = curated_sketches();
    let mut catalog = build_catalog(&matrix, &descriptions(), &sketches);
    crate::discovery::curation::apply_curations(&mut catalog, &[list_curation(status_ok)]);
    catalog.validate(&sketches).unwrap();
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

#[test]
fn a_curated_facet_file_matches_the_golden_file() {
    assert_eq!(
        file(&render_curated(true), "pricing/list.md"),
        GOLDEN_CURATED_LIST
    );
}

#[test]
fn a_curated_readme_matches_the_golden_file() {
    assert_eq!(
        file(&render_curated(true), "README.md"),
        GOLDEN_CURATED_README
    );
}

#[test]
fn the_representative_leads_its_group_with_a_star() {
    let list = file(&render_curated(true), "pricing/list.md");
    assert!(list.contains("\n## Supplier feed\n\nThe feed sets the price; members differ on rounding.\n\n### ★ sk_0001\n"));
    assert!(list.contains("\n## Hand-set by sales\n\nPeople set the price.\n\n### ★ sk_0002\n"));
    assert!(list.contains("\n3 theses in 2 groups.\n"));
}

#[test]
fn a_duplicate_is_folded_under_the_thesis_it_repeats_never_dropped() {
    let list = file(&render_curated(true), "pricing/list.md");
    let head = list.find("### ★ sk_0001").unwrap();
    let fold = list
        .find("<details>\n<summary>1 duplicate of sk_0001</summary>\n\n### sk_0004\n")
        .unwrap();
    let next_group = list.find("## Hand-set by sales").unwrap();
    assert!(head < fold && fold < next_group, "{list}");
}

#[test]
fn every_thesis_of_a_curated_cell_has_exactly_one_heading() {
    let list = file(&render_curated(true), "pricing/list.md");
    for id in ["sk_0001", "sk_0002", "sk_0004"] {
        let hits = list
            .lines()
            .filter(|l| *l == format!("### {id}") || *l == format!("### ★ {id}"))
            .count();
        assert_eq!(hits, 1, "{id}:\n{list}");
    }
}

#[test]
fn tensions_are_listed_at_the_end_of_the_facet_file() {
    let list = file(&render_curated(true), "pricing/list.md");
    assert!(list.ends_with(
        "\n## Tensions\n\n- **sk_0001** ↔ **sk_0002** — The feed and the sales team cannot both own the list price.\n"
    ));
}

#[test]
fn a_failed_curation_shows_a_warning_and_the_flat_group() {
    let files = render_curated(false);
    let list = file(&files, "pricing/list.md");
    assert!(list.starts_with(
        "# Pricing → List price\n\n3 theses.\n\n⚠ Grouping failed for this cell; its theses are listed without groups.\n\n## All theses\n\n### sk_0001\n"
    ));
    assert!(!list.contains('★'));
    let readme = file(&files, "README.md");
    assert!(readme.contains("\n- Grouped cells: 0 of 2 with theses\n- ⚠ Grouping failed in 1 cell(s); their theses are listed flat: [Pricing → List price](pricing/list.md)\n"), "{readme}");
}

#[test]
fn catalog_json_records_the_group_and_duplicate_of_every_curated_thesis() {
    let catalog: serde_json::Value =
        serde_json::from_str(&file(&render_curated(true), "catalog.json")).unwrap();
    let by_id: BTreeMap<&str, &serde_json::Value> = catalog["sketches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s))
        .collect();
    assert_eq!(by_id.len(), 4);
    assert_eq!(by_id["sk_0001"]["group"], "Supplier feed");
    assert!(by_id["sk_0001"]["duplicate_of"].is_null());
    assert_eq!(by_id["sk_0004"]["group"], "Supplier feed");
    assert_eq!(by_id["sk_0004"]["duplicate_of"], "sk_0001");
    assert_eq!(by_id["sk_0002"]["group"], "Hand-set by sales");
    assert_eq!(by_id["sk_0003"]["group"], "All theses");
    let cell = &catalog["cells"][0];
    assert_eq!(cell["curation"], "grouped");
    assert_eq!(cell["groups"][0]["representative"], "sk_0001");
    assert_eq!(cell["tensions"][0]["a"], "sk_0001");
    assert_eq!(catalog["cells"][1]["curation"], "flat");
}

#[test]
fn a_duplicate_whose_head_is_not_in_its_group_gets_its_own_entry() {
    let matrix = matrix();
    let sketches = curated_sketches();
    let mut catalog = build_catalog(&matrix, &[], &sketches);
    catalog.cells[0].groups[0].members[2].duplicate_of = Some("sk_0404".into());
    let files = render(&RenderInput {
        run_id: "r",
        problem: "p",
        constraints: &[],
        matrix: &matrix,
        catalog: &catalog,
        sketches: &sketches,
    });
    let list = file(&files, "pricing/list.md");
    assert!(list.contains("\n### sk_0004\n"), "{list}");
    assert!(!list.contains("<summary>1 duplicate"));
}
