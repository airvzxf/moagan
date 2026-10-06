//! Behaviour of the curation checks: whatever the curator answers,
//! every thesis of the cell ends in exactly one group.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::discovery::catalog::build_catalog;
use crate::discovery::matrix::{Dimension, ExplorationMatrix, Facet};

fn ids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("sk_{:04}", i * 3)).collect()
}

fn raw(value: serde_json::Value) -> RawCuration {
    serde_json::from_value(value).unwrap()
}

fn listed(groups: &[Group]) -> Vec<String> {
    let mut all: Vec<String> = groups
        .iter()
        .flat_map(|g| g.members.iter())
        .map(|m| m.sketch_id.clone())
        .collect();
    all.sort();
    all
}

fn member_ids(group: &Group) -> Vec<&str> {
    group.members.iter().map(|m| m.sketch_id.as_str()).collect()
}

fn duplicate_of<'a>(groups: &'a [Group], id: &str) -> Option<&'a str> {
    groups
        .iter()
        .flat_map(|g| g.members.iter())
        .find(|m| m.sketch_id == id)
        .and_then(|m| m.duplicate_of.as_deref())
}

/// Four theses, two groups, one duplicate, one tension: a clean answer.
fn clean_answer() -> RawCuration {
    raw(json!({
        "groups": [
            {"label": "Opaque id in the QR", "summary": "Short ids resolved by the server.", "representative": 2},
            {"label": "Readable path", "summary": "The label spells the location.", "representative": 1}
        ],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 1}, {"n": 2, "g": 0}, {"n": 3, "g": 1}],
        "duplicates": [[3, 1]],
        "tensions": [[0, 1, "An opaque id and a readable path cannot both be the label."]]
    }))
}

// ------------------------------------------------------------ groups

#[test]
fn a_clean_answer_keeps_the_models_groups_in_order() {
    let chunk = normalise(&ids(4), &clean_answer());
    let labels: Vec<&str> = chunk.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, ["Opaque id in the QR", "Readable path"]);
    assert_eq!(chunk.groups[0].summary, "Short ids resolved by the server.");
    assert_eq!(chunk.report.unassigned, 0);
    assert!(chunk.report.is_acceptable());
}

#[test]
fn the_representative_is_listed_first_and_the_rest_by_id() {
    let chunk = normalise(&ids(4), &clean_answer());
    assert_eq!(chunk.groups[0].representative.as_deref(), Some("sk_0006"));
    assert_eq!(member_ids(&chunk.groups[0]), ["sk_0006", "sk_0000"]);
    assert_eq!(chunk.groups[1].representative.as_deref(), Some("sk_0003"));
    assert_eq!(member_ids(&chunk.groups[1]), ["sk_0003", "sk_0009"]);
}

#[test]
fn a_representative_outside_its_group_becomes_the_first_member() {
    let answer = raw(json!({
        "groups": [{"label": "A", "summary": "", "representative": 1}, {"label": "B", "summary": ""}],
        "assign": [{"n": 0, "g": 0}, {"n": 2, "g": 0}, {"n": 1, "g": 1}]
    }));
    let chunk = normalise(&ids(3), &answer);
    assert_eq!(chunk.groups[0].representative.as_deref(), Some("sk_0000"));
    assert_eq!(chunk.groups[1].representative.as_deref(), Some("sk_0003"));
}

#[test]
fn every_group_always_has_a_representative_among_its_members() {
    let chunk = normalise(
        &ids(4),
        &raw(json!({"groups": [{"label": "A"}], "assign": [{"n": 1, "g": 0}]})),
    );
    for group in &chunk.groups {
        let rep = group.representative.as_deref().unwrap();
        assert_eq!(member_ids(group)[0], rep);
    }
}

#[test]
fn unassigned_theses_go_to_a_last_ungrouped_group() {
    let answer = raw(json!({
        "groups": [{"label": "A", "summary": "s", "representative": 0}],
        "assign": [{"n": 0, "g": 0}, {"n": 2, "g": 0}]
    }));
    let chunk = normalise(&ids(4), &answer);
    let last = chunk.groups.last().unwrap();
    assert_eq!(last.label, UNGROUPED_LABEL);
    assert_eq!(member_ids(last), ["sk_0003", "sk_0009"]);
    assert_eq!(last.representative.as_deref(), Some("sk_0003"));
    assert_eq!(chunk.report.unassigned, 2);
}

#[test]
fn unknown_thesis_or_group_numbers_and_malformed_entries_are_ignored() {
    let answer = raw(json!({
        "groups": [{"label": "A"}],
        "assign": [{"n": 0, "g": 0}, {"n": 9, "g": 0}, {"n": 1, "g": 4}, {"n": "2", "g": 0}, [2, 0], {"n": -1, "g": 0}]
    }));
    let chunk = normalise(&ids(3), &answer);
    assert_eq!(chunk.report.unknown, 5);
    assert_eq!(chunk.report.unassigned, 2);
    assert_eq!(listed(&chunk.groups), ids(3));
}

#[test]
fn a_thesis_assigned_twice_stays_in_its_first_group() {
    let answer = raw(json!({
        "groups": [{"label": "A"}, {"label": "B"}],
        "assign": [{"n": 0, "g": 1}, {"n": 0, "g": 0}, {"n": 1, "g": 0}]
    }));
    let chunk = normalise(&ids(2), &answer);
    assert_eq!(chunk.report.repeated, 1);
    assert_eq!(member_ids(&chunk.groups[0]), ["sk_0003"]);
    assert_eq!(member_ids(&chunk.groups[1]), ["sk_0000"]);
}

#[test]
fn groups_without_members_are_dropped() {
    let answer = raw(json!({
        "groups": [{"label": "Empty"}, {"label": "Full"}],
        "assign": [{"n": 0, "g": 1}, {"n": 1, "g": 1}]
    }));
    let chunk = normalise(&ids(2), &answer);
    let labels: Vec<&str> = chunk.groups.iter().map(|g| g.label.as_str()).collect();
    assert_eq!(labels, ["Full"]);
}

#[test]
fn labels_and_summaries_are_one_line_and_an_empty_label_gets_a_number() {
    let answer = raw(json!({
        "groups": [{"label": "  ", "summary": "two\n  lines"}, {"label": "Real\nlabel"}],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 1}]
    }));
    let chunk = normalise(&ids(2), &answer);
    assert_eq!(chunk.groups[0].label, "Group 1");
    assert_eq!(chunk.groups[0].summary, "two lines");
    assert_eq!(chunk.groups[1].label, "Real label");
}

#[test]
fn more_than_a_fifth_unassigned_is_not_acceptable() {
    let one_of_five = normalise(
        &ids(5),
        &raw(
            json!({"groups": [{"label": "A"}], "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 0}, {"n": 2, "g": 0}, {"n": 3, "g": 0}]}),
        ),
    );
    assert!(one_of_five.report.is_acceptable());
    let two_of_five = normalise(
        &ids(5),
        &raw(
            json!({"groups": [{"label": "A"}], "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 0}, {"n": 2, "g": 0}]}),
        ),
    );
    assert!(!two_of_five.report.is_acceptable());
}

#[test]
fn an_answer_without_assign_or_groups_is_not_acceptable() {
    for answer in [
        json!({"groups": [{"label": "A"}]}),
        json!({"assign": [{"n": 0, "g": 0}]}),
        json!({"groups": "none", "assign": null}),
        json!({}),
    ] {
        let chunk = normalise(&ids(3), &raw(answer.clone()));
        assert!(!chunk.report.is_acceptable(), "{answer}");
        assert_eq!(listed(&chunk.groups), ids(3), "{answer}");
    }
}

// -------------------------------------------------------- duplicates

#[test]
fn a_duplicate_is_folded_under_the_thesis_it_repeats() {
    let chunk = normalise(&ids(4), &clean_answer());
    assert_eq!(duplicate_of(&chunk.groups, "sk_0009"), Some("sk_0003"));
    assert_eq!(duplicate_of(&chunk.groups, "sk_0003"), None);
    assert_eq!(listed(&chunk.groups), ids(4));
}

#[test]
fn a_representative_marked_as_duplicate_keeps_its_place_and_the_other_thesis_folds_under_it() {
    let answer = raw(json!({
        "groups": [{"label": "A", "representative": 0}],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 0}, {"n": 2, "g": 0}],
        "duplicates": [[0, 2]]
    }));
    let chunk = normalise(&ids(3), &answer);
    assert_eq!(chunk.groups[0].representative.as_deref(), Some("sk_0000"));
    assert_eq!(duplicate_of(&chunk.groups, "sk_0000"), None);
    assert_eq!(duplicate_of(&chunk.groups, "sk_0006"), Some("sk_0000"));
}

#[test]
fn chained_duplicates_fold_under_one_head() {
    let answer = raw(json!({
        "groups": [{"label": "A", "representative": 3}],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 0}, {"n": 2, "g": 0}, {"n": 3, "g": 0}],
        "duplicates": [[2, 1], [1, 0]]
    }));
    let chunk = normalise(&ids(4), &answer);
    assert_eq!(duplicate_of(&chunk.groups, "sk_0000"), None);
    assert_eq!(duplicate_of(&chunk.groups, "sk_0003"), Some("sk_0000"));
    assert_eq!(duplicate_of(&chunk.groups, "sk_0006"), Some("sk_0000"));
    assert_eq!(duplicate_of(&chunk.groups, "sk_0009"), None);
}

#[test]
fn duplicates_across_groups_unknown_or_self_pairs_are_dropped() {
    let answer = raw(json!({
        "groups": [{"label": "A"}, {"label": "B"}],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 1}, {"n": 2, "g": 0}],
        "duplicates": [[0, 1], [0, 7], [2, 2], [0], {"n": 0, "m": 2}]
    }));
    let chunk = normalise(&ids(3), &answer);
    assert_eq!(chunk.report.dropped_duplicates, 5);
    assert!(
        chunk
            .groups
            .iter()
            .flat_map(|g| g.members.iter())
            .all(|m| m.duplicate_of.is_none())
    );
}

#[test]
fn ungrouped_theses_are_never_duplicates() {
    let answer = raw(json!({
        "groups": [{"label": "A"}],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 0}, {"n": 2, "g": 0}, {"n": 3, "g": 0}],
        "duplicates": [[4, 5]]
    }));
    let chunk = normalise(&ids(6), &answer);
    assert_eq!(chunk.report.dropped_duplicates, 1);
}

// ---------------------------------------------------------- tensions

#[test]
fn a_tension_names_both_theses_and_its_note() {
    let chunk = normalise(&ids(4), &clean_answer());
    assert_eq!(
        chunk.tensions,
        [Tension {
            a: "sk_0000".into(),
            b: "sk_0003".into(),
            note: "An opaque id and a readable path cannot both be the label.".into(),
        }]
    );
}

#[test]
fn tensions_with_unknown_or_equal_theses_or_repeated_pairs_are_dropped() {
    let answer = raw(json!({
        "groups": [{"label": "A"}],
        "assign": [{"n": 0, "g": 0}, {"n": 1, "g": 0}, {"n": 2, "g": 0}],
        "tensions": [[0, 2, "x"], [2, 0, "again"], [1, 1, "self"], [0, 9, "far"], "text", [1, 2]]
    }));
    let chunk = normalise(&ids(3), &answer);
    assert_eq!(chunk.report.dropped_tensions, 4);
    let pairs: Vec<(&str, &str, &str)> = chunk
        .tensions
        .iter()
        .map(|t| (t.a.as_str(), t.b.as_str(), t.note.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [("sk_0000", "sk_0006", "x"), ("sk_0003", "sk_0006", "")]
    );
}

#[test]
fn normalising_twice_gives_the_same_curation() {
    assert_eq!(
        normalise(&ids(4), &clean_answer()),
        normalise(&ids(4), &clean_answer())
    );
}

proptest! {
    /// Whatever the answer, every thesis is listed exactly once and
    /// every group has a representative that is its first member.
    #[test]
    fn every_thesis_is_listed_exactly_once_whatever_the_answer(
        n in 1usize..40,
        groups in 0usize..8,
        assign in proptest::collection::vec((0u64..50, 0u64..10), 0..80),
        duplicates in proptest::collection::vec((0u64..50, 0u64..50), 0..20),
        reps in proptest::collection::vec(0u64..50, 0..8),
    ) {
        let answer = raw(json!({
            "groups": (0..groups)
                .map(|g| json!({"label": format!("G{g}"), "representative": reps.get(g).copied()}))
                .collect::<Vec<_>>(),
            "assign": assign.iter().map(|(n, g)| json!({"n": n, "g": g})).collect::<Vec<_>>(),
            "duplicates": duplicates.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
        }));
        let ids = ids(n);
        let chunk = normalise(&ids, &answer);
        prop_assert_eq!(listed(&chunk.groups), ids.clone());
        for group in &chunk.groups {
            prop_assert!(!group.members.is_empty());
            prop_assert_eq!(group.representative.as_deref(), Some(group.members[0].sketch_id.as_str()));
            for member in &group.members {
                if let Some(head) = &member.duplicate_of {
                    let head_member = group.members.iter().find(|m| &m.sketch_id == head);
                    prop_assert!(head_member.is_some_and(|m| m.duplicate_of.is_none()));
                }
            }
        }
        prop_assert_eq!(
            chunk.report.unassigned,
            chunk.groups.iter().filter(|g| g.label == UNGROUPED_LABEL).map(|g| g.members.len()).sum::<usize>()
        );
    }
}

// ------------------------------------------------------------ chunks

#[test]
fn small_cells_are_one_chunk_and_large_ones_split_evenly() {
    assert_eq!(chunk_ranges(0, 140), Vec::<Range<usize>>::new());
    assert_eq!(chunk_ranges(4, 140).len(), 1);
    assert_eq!(chunk_ranges(4, 140)[0], 0..4);
    assert_eq!(chunk_ranges(140, 140).len(), 1);
    assert_eq!(chunk_ranges(140, 140)[0], 0..140);
    assert_eq!(chunk_ranges(141, 140), [0..70, 70..141]);
    assert_eq!(chunk_ranges(300, 140), [0..100, 100..200, 200..300]);
    assert_eq!(chunk_ranges(3, 0), [0..1, 1..2, 2..3]);
}

// ----------------------------------------------------------- payload

fn cell_entry() -> CellEntry {
    let matrix = ExplorationMatrix::new(
        vec![Dimension {
            id: "qr".into(),
            label: "QR and the physical world".into(),
            facets: vec![Facet {
                id: "content".into(),
                label: "QR content".into(),
            }],
        }],
        2,
    );
    build_catalog(&matrix, &[], &[]).cells.remove(0)
}

fn thesis(id: &str, text: &str, decisions: usize, checks: &[(&str, bool)]) -> Sketch {
    Sketch {
        id: id.into(),
        thesis: text.into(),
        key_decisions: (0..decisions).map(|i| format!("decision {i}")).collect(),
        hard_constraint_check: checks
            .iter()
            .map(|(k, v)| ((*k).to_owned(), *v))
            .collect::<BTreeMap<_, _>>(),
        angle: "qr:content".into(),
        ..Sketch::default()
    }
}

#[test]
fn the_payload_numbers_the_theses_and_never_names_a_sketch_id() {
    let a = thesis(
        "sk_0042",
        "Print an opaque id.",
        7,
        &[("C2", false), ("C1", true), ("C10", false)],
    );
    let b = thesis("sk_0043", "Print the path.", 1, &[]);
    let text = curator_payload(
        &cell_entry(),
        &["No invented figures".into(), "AGPL".into()],
        &[&a, &b],
    );
    assert!(!text.contains("sk_00"), "{text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value,
        json!({
            "cell": {"dimension": "QR and the physical world", "facet": "QR content", "description": ""},
            "constraints": ["C1: No invented figures", "C2: AGPL"],
            "theses": [
                {"n": 0, "thesis": "Print an opaque id.",
                 "key_decisions": ["decision 0", "decision 1", "decision 2", "decision 3", "decision 4"],
                 "violates": ["C10", "C2"]},
                {"n": 1, "thesis": "Print the path.", "key_decisions": ["decision 0"], "violates": []}
            ]
        })
    );
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["cell", "constraints", "theses"]);
}

#[test]
fn the_same_inputs_give_the_same_payload_bytes() {
    let a = thesis("sk_0001", "One.", 2, &[("C1", false)]);
    let one = curator_payload(&cell_entry(), &["X".into()], &[&a]);
    let two = curator_payload(&cell_entry(), &["X".into()], &[&a]);
    assert_eq!(one, two);
}

// ---------------------------------------------------------- curation

fn ok_chunk(ids: &[String]) -> ChunkCuration {
    normalise(
        ids,
        &raw(json!({
            "groups": [{"label": "A", "representative": 0}],
            "assign": (0..ids.len()).map(|n| json!({"n": n, "g": 0})).collect::<Vec<_>>(),
            "tensions": [[0, 1, "x"]]
        })),
    )
}

#[test]
fn merging_chunks_concatenates_their_groups_and_tensions() {
    let all = ids(4);
    let curation = Curation::merge(
        "qr:content",
        all.clone(),
        vec![Some(ok_chunk(&all[0..2])), Some(ok_chunk(&all[2..4]))],
    );
    assert_eq!(curation.status, CurationStatus::Ok);
    assert_eq!(curation.groups.len(), 2);
    assert_eq!(curation.tensions.len(), 2);
    assert_eq!(curation.reports.len(), 2);
    assert_eq!(listed(&curation.groups), all);
    assert!(curation.covers("qr:content", &all));
}

#[test]
fn one_failed_chunk_fails_the_whole_cell_without_groups() {
    let all = ids(4);
    let curation = Curation::merge(
        "qr:content",
        all.clone(),
        vec![Some(ok_chunk(&all[0..2])), None],
    );
    assert_eq!(curation.status, CurationStatus::Failed);
    assert!(curation.groups.is_empty());
    assert!(curation.tensions.is_empty());
    assert!(curation.covers("qr:content", &all));
}

#[test]
fn a_curation_covers_only_its_cell_and_exactly_its_theses() {
    let all = ids(3);
    let curation = Curation::merge("qr:content", all.clone(), vec![Some(ok_chunk(&all))]);
    assert!(curation.covers("qr:content", &all));
    assert!(!curation.covers("qr:print", &all));
    assert!(!curation.covers("qr:content", &all[..2]));
    let mut more = all.clone();
    more.push("sk_0100".into());
    assert!(!curation.covers("qr:content", &more));
    let mut broken = curation.clone();
    broken.groups[0].members.pop();
    assert!(!broken.covers("qr:content", &all));
}

#[test]
fn the_curation_file_round_trips_and_names_its_status_in_lowercase() {
    let all = ids(2);
    let curation = Curation::merge("qr:content", all.clone(), vec![Some(ok_chunk(&all))]);
    let text = serde_json::to_string(&curation).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["cell"], "qr:content");
    let back: Curation = serde_json::from_str(&text).unwrap();
    assert_eq!(back, curation);
    let failed = Curation::merge("qr:content", all, vec![None]);
    assert_eq!(serde_json::to_value(&failed).unwrap()["status"], "failed");
}

#[test]
fn the_curation_file_is_named_after_the_cell_and_stays_in_its_directory() {
    let dir = Path::new("/run/curation");
    assert_eq!(
        curation_path(dir, "e2-qr", "contenido-del-qr"),
        PathBuf::from("/run/curation/e2-qr__contenido-del-qr.json")
    );
    assert_eq!(
        curation_path(dir, "../x", "a/b"),
        PathBuf::from("/run/curation/---x__a-b.json")
    );
}

// ----------------------------------------------------------- catalog

fn two_cell_catalog() -> (Catalog, Vec<Sketch>) {
    let matrix = ExplorationMatrix::new(
        vec![Dimension {
            id: "qr".into(),
            label: "QR".into(),
            facets: vec![
                Facet {
                    id: "content".into(),
                    label: "Content".into(),
                },
                Facet {
                    id: "print".into(),
                    label: "Print".into(),
                },
            ],
        }],
        2,
    );
    let sketches = vec![
        thesis("sk_0000", "a", 0, &[]),
        thesis("sk_0001", "b", 0, &[]),
        thesis("sk_0002", "c", 0, &[]),
        Sketch {
            angle: "qr:print".into(),
            ..thesis("sk_0003", "d", 0, &[])
        },
    ];
    (build_catalog(&matrix, &[], &sketches), sketches)
}

#[test]
fn a_built_catalogue_is_flat_with_no_representative_and_no_tension() {
    let (catalog, _) = two_cell_catalog();
    for cell in &catalog.cells {
        assert_eq!(cell.curation, CellCuration::Flat);
        assert!(cell.tensions.is_empty());
        assert!(cell.groups.iter().all(|g| g.representative.is_none()));
    }
    assert_eq!(cell_key(&catalog.cells[0]), "qr:content");
    assert_eq!(
        cell_member_ids(&catalog.cells[0]),
        ["sk_0000", "sk_0001", "sk_0002"]
    );
}

#[test]
fn a_covering_curation_replaces_the_groups_and_the_catalogue_stays_valid() {
    let (mut catalog, sketches) = two_cell_catalog();
    let members = cell_member_ids(&catalog.cells[0]);
    let curation = Curation::merge(
        "qr:content",
        members.clone(),
        vec![Some(ok_chunk(&members))],
    );
    apply_curations(&mut catalog, std::slice::from_ref(&curation));
    assert_eq!(catalog.cells[0].curation, CellCuration::Grouped);
    assert_eq!(catalog.cells[0].groups, curation.groups);
    assert_eq!(catalog.cells[0].tensions, curation.tensions);
    assert_eq!(catalog.cells[1].curation, CellCuration::Flat);
    catalog.validate(&sketches).unwrap();
}

#[test]
fn a_failed_curation_keeps_the_flat_group_and_marks_the_cell() {
    let (mut catalog, _) = two_cell_catalog();
    let before = catalog.cells[0].groups.clone();
    let members = cell_member_ids(&catalog.cells[0]);
    apply_curations(
        &mut catalog,
        &[Curation::merge("qr:content", members, vec![None])],
    );
    assert_eq!(catalog.cells[0].curation, CellCuration::Failed);
    assert_eq!(catalog.cells[0].groups, before);
}

#[test]
fn a_stale_curation_is_ignored() {
    let (mut catalog, _) = two_cell_catalog();
    let before = catalog.clone();
    let old = ids(2);
    apply_curations(
        &mut catalog,
        &[Curation::merge(
            "qr:content",
            old.clone(),
            vec![Some(ok_chunk(&old))],
        )],
    );
    assert_eq!(catalog, before);
}

#[test]
fn an_old_catalogue_without_the_new_fields_still_parses() {
    let old = json!({"cells": [{
        "dimension_id": "qr", "facet_id": "content", "dimension_label": "QR",
        "facet_label": "Content", "description": "",
        "groups": [{"label": "All theses", "summary": "", "members": [{"sketch_id": "sk_0000", "duplicate_of": null}]}]
    }]});
    let catalog: Catalog = serde_json::from_value(old).unwrap();
    assert_eq!(catalog.cells[0].curation, CellCuration::Flat);
    assert!(catalog.cells[0].groups[0].representative.is_none());
}
