use super::*;

use crate::discovery::matrix::{Dimension, Facet};
use crate::domain::{Intake, SketchProvenance};

fn dimension(id: &str, facets: &[(&str, &str)]) -> Dimension {
    Dimension {
        id: id.into(),
        label: id.to_uppercase(),
        facets: facets
            .iter()
            .map(|(id, label)| Facet {
                id: (*id).into(),
                label: (*label).into(),
            })
            .collect(),
    }
}

fn provenance(index: usize) -> SketchProvenance {
    SketchProvenance {
        section: "minimax".into(),
        model: "MiniMax-M3".into(),
        temperature: 1.0,
        replica: 0,
        index,
        source_run: None,
        source_sketch: None,
    }
}

fn sketch(id: &str, angle: &str) -> Sketch {
    Sketch {
        id: id.into(),
        thesis: format!("Thesis of {id} in cell {angle}, long enough to keep."),
        angle: angle.into(),
        provenance: Some(provenance(0)),
        ..Sketch::default()
    }
}

fn source(run_id: &str, dimensions: Vec<Dimension>, sketches: Vec<Sketch>) -> SourceRun {
    SourceRun {
        run_id: run_id.into(),
        dir: PathBuf::from(run_id),
        inputs: RunInputs {
            matrix: ExplorationMatrix::new(dimensions, 2),
            descriptions: Vec::new(),
            brief: Intake {
                problem: "Sign-in".into(),
                constraints: vec!["Fixed budget".into(), "No passwords".into()],
                ..Intake::default()
            },
            sketches,
        },
    }
}

fn two_sources() -> Vec<SourceRun> {
    vec![
        source(
            "run-a",
            vec![dimension(
                "auth",
                &[("oauth", "OAuth"), ("api-key", "API key")],
            )],
            vec![
                sketch("sk_0000", "auth:oauth"),
                sketch("sk_0001", "auth:api-key"),
            ],
        ),
        source(
            "run-b",
            vec![
                dimension(
                    "auth",
                    &[("oauth", "OAuth 2"), ("magic-link", "Magic link")],
                ),
                dimension("storage", &[("sqlite", "SQLite")]),
            ],
            vec![
                sketch("sk_0000", "auth:magic-link"),
                sketch("sk_0001", "storage:sqlite"),
                sketch("sk_0002", "auth:oauth"),
            ],
        ),
    ]
}

#[test]
fn sources_with_the_same_constraints_can_be_merged() {
    assert!(check_same_constraints(&two_sources()).is_ok());
    assert!(check_same_constraints(&[]).is_ok());
}

#[test]
fn a_source_with_other_constraints_is_refused_with_the_first_difference() {
    let mut sources = two_sources();
    sources[1].inputs.brief.constraints = vec!["Fixed budget".into(), "Passwords allowed".into()];
    match check_same_constraints(&sources) {
        Err(Error::InvalidArgs(message)) => {
            assert!(
                message.contains("run run-b does not share the brief constraints of run run-a"),
                "{message}"
            );
            assert!(message.contains("(they differ from C2)"), "{message}");
            assert!(message.contains("--brief <run>/brief.json"), "{message}");
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

#[test]
fn a_source_with_an_extra_constraint_is_refused() {
    let mut sources = two_sources();
    sources[1]
        .inputs
        .brief
        .constraints
        .push("Spanish only".into());
    match check_same_constraints(&sources) {
        Err(Error::InvalidArgs(message)) => {
            assert!(message.contains("(they differ from C3)"), "{message}");
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

#[test]
fn the_merged_matrix_is_the_union_of_cells_in_first_seen_order() {
    let merged = merge_matrices(&two_sources()).unwrap();
    let cells: Vec<String> = merged
        .iter_cells()
        .map(|c| format!("{}:{} {}", c.dimension_id, c.facet_id, c.label))
        .collect();
    assert_eq!(
        cells,
        [
            "auth:oauth AUTH / OAuth",
            "auth:api-key AUTH / API key",
            "auth:magic-link AUTH / Magic link",
            "storage:sqlite STORAGE / SQLite",
        ]
    );
    assert_eq!(merged.sketches_per_cell, 2);
    assert!(merge_matrices(&[]).is_none());
}

#[test]
fn each_cell_keeps_the_first_description_found() {
    let mut sources = two_sources();
    sources[0].inputs.descriptions = vec![DimensionFacetDescription::new("auth", "oauth", "A")];
    sources[1].inputs.descriptions = vec![
        DimensionFacetDescription::new("auth", "oauth", "B"),
        DimensionFacetDescription::new("storage", "sqlite", "C"),
    ];
    let merged: Vec<(String, String, String)> = merge_descriptions(&sources)
        .into_iter()
        .map(|d| (d.dimension_id, d.facet_id, d.description))
        .collect();
    assert_eq!(
        merged,
        [
            ("auth".into(), "oauth".into(), "A".into()),
            ("storage".into(), "sqlite".into(), "C".into()),
        ]
    );
}

#[test]
fn pooled_sketches_are_renumbered_and_name_their_source() {
    let pooled = pool_sketches(&two_sources()).unwrap();
    let rows: Vec<(String, String, String, String)> = pooled
        .iter()
        .map(|s| {
            let p = s.provenance.as_ref().unwrap();
            (
                s.id.clone(),
                s.angle.clone(),
                p.source_run.clone().unwrap(),
                p.source_sketch.clone().unwrap(),
            )
        })
        .collect();
    let expected: Vec<(String, String, String, String)> = [
        ("sk_0000", "auth:oauth", "run-a", "sk_0000"),
        ("sk_0001", "auth:api-key", "run-a", "sk_0001"),
        ("sk_0002", "auth:magic-link", "run-b", "sk_0000"),
        ("sk_0003", "storage:sqlite", "run-b", "sk_0001"),
        ("sk_0004", "auth:oauth", "run-b", "sk_0002"),
    ]
    .iter()
    .map(|(a, b, c, d)| ((*a).into(), (*b).into(), (*c).into(), (*d).into()))
    .collect();
    assert_eq!(rows, expected);
    assert_eq!(
        pooled[4].thesis,
        "Thesis of sk_0002 in cell auth:oauth, long enough to keep."
    );
}

#[test]
fn a_sketch_pooled_twice_keeps_its_original_source() {
    let mut sources = two_sources();
    if let Some(p) = sources[1].inputs.sketches[0].provenance.as_mut() {
        p.source_run = Some("run-0".into());
        p.source_sketch = Some("sk_0042".into());
    }
    let pooled = pool_sketches(&sources).unwrap();
    let p = pooled[2].provenance.as_ref().unwrap();
    assert_eq!(p.source_run.as_deref(), Some("run-0"));
    assert_eq!(p.source_sketch.as_deref(), Some("sk_0042"));
}

#[test]
fn a_sketch_without_provenance_cannot_be_pooled() {
    let mut sources = two_sources();
    sources[1].inputs.sketches[1].provenance = None;
    match pool_sketches(&sources) {
        Err(Error::InvalidArgs(message)) => assert!(
            message.contains("sketch sk_0001 of run run-b records no provenance"),
            "{message}"
        ),
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}
