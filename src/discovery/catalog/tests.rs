use super::*;

use crate::discovery::matrix::{Dimension, Facet};

fn facet(id: &str, label: &str) -> Facet {
    Facet {
        id: id.into(),
        label: label.into(),
    }
}

/// Two dimensions, three cells: pricing:{list, margin}, logistics:{pickup}.
fn matrix() -> ExplorationMatrix {
    ExplorationMatrix::new(
        vec![
            Dimension {
                id: "pricing".into(),
                label: "Pricing".into(),
                facets: vec![facet("list", "List price"), facet("margin", "Margin")],
            },
            Dimension {
                id: "logistics".into(),
                label: "Logistics".into(),
                facets: vec![facet("pickup", "Store pickup")],
            },
        ],
        2,
    )
}

fn thesis(id: &str, angle: &str) -> Thesis {
    Thesis {
        id: id.into(),
        claim: format!("Thesis of {id}"),
        angle: angle.into(),
        ..Thesis::default()
    }
}

fn theses() -> Vec<Thesis> {
    vec![
        thesis("th_0003", "pricing:list"),
        thesis("th_0001", "logistics:pickup"),
        thesis("th_0002", "pricing:list"),
    ]
}

fn ids(cell: &CellEntry) -> Vec<&str> {
    cell.groups
        .iter()
        .flat_map(|g| g.members.iter())
        .map(|m| m.thesis_id.as_str())
        .collect()
}

fn keys(catalog: &Catalog) -> Vec<String> {
    catalog
        .cells
        .iter()
        .map(|c| format!("{}:{}", c.dimension_id, c.facet_id))
        .collect()
}

#[test]
fn cells_follow_the_matrix_order() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    assert_eq!(
        keys(&catalog),
        ["pricing:list", "pricing:margin", "logistics:pickup"]
    );
}

#[test]
fn cell_labels_come_from_the_matrix() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    let cell = &catalog.cells[0];
    assert_eq!(cell.dimension_label, "Pricing");
    assert_eq!(cell.facet_label, "List price");
}

#[test]
fn every_thesis_lands_in_the_cell_named_by_its_angle() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    assert_eq!(ids(&catalog.cells[0]), ["th_0002", "th_0003"]);
    assert_eq!(ids(&catalog.cells[2]), ["th_0001"]);
}

#[test]
fn members_are_ordered_by_thesis_id() {
    let input = vec![
        thesis("th_0010", "pricing:list"),
        thesis("th_0002", "pricing:list"),
        thesis("th_0007", "pricing:list"),
    ];
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(ids(&catalog.cells[0]), ["th_0002", "th_0007", "th_0010"]);
}

#[test]
fn a_cell_without_theses_has_no_groups() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    assert!(catalog.cells[1].groups.is_empty());
}

#[test]
fn a_non_empty_cell_has_one_implicit_group_without_duplicates() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    let groups = &catalog.cells[0].groups;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].label, DEFAULT_GROUP_LABEL);
    assert_eq!(groups[0].label, "All theses");
    assert_eq!(groups[0].summary, "");
    assert!(groups[0].members.iter().all(|m| m.duplicate_of.is_none()));
}

#[test]
fn a_thesis_with_an_unknown_angle_goes_to_a_trailing_outside_cell() {
    let mut input = theses();
    input.push(thesis("th_0009", "pricing:unknown"));
    input.push(thesis("th_0004", ""));
    let catalog = build_catalog(&matrix(), &[], &input);
    let last = catalog.cells.last().unwrap();
    assert_eq!(catalog.cells.len(), 4);
    assert_eq!(last.dimension_id, OUTSIDE_DIMENSION_ID);
    assert_eq!(last.facet_id, OUTSIDE_FACET_ID);
    assert_eq!(last.dimension_label, "Outside the matrix");
    assert_eq!(last.facet_label, "Unmatched angle");
    assert_eq!(last.description, "");
    assert_eq!(ids(last), ["th_0004", "th_0009"]);
}

#[test]
fn the_outside_cell_exists_only_when_a_thesis_needs_it() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    assert!(
        catalog
            .cells
            .iter()
            .all(|c| c.dimension_id != OUTSIDE_DIMENSION_ID)
    );
}

#[test]
fn facet_descriptions_are_joined_by_dimension_and_facet() {
    let descriptions = vec![
        DimensionFacetDescription::new("pricing", "margin", "How margin is protected."),
        DimensionFacetDescription::new("logistics", "list", "Wrong dimension for list."),
    ];
    let catalog = build_catalog(&matrix(), &descriptions, &theses());
    assert_eq!(catalog.cells[0].description, "");
    assert_eq!(catalog.cells[1].description, "How margin is protected.");
    assert_eq!(catalog.cells[2].description, "");
}

#[test]
fn old_non_canonical_ids_are_kept_as_they_are() {
    let input = vec![
        thesis("642", "pricing:margin"),
        thesis("thesis-402", "pricing:margin"),
    ];
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(ids(&catalog.cells[1]), ["642", "thesis-402"]);
    assert_eq!(catalog.validate(&input), Ok(()));
}

#[test]
fn a_repeated_matrix_cell_receives_theses_only_once() {
    let mut m = matrix();
    m.dimensions[1].facets.push(facet("pickup", "Pickup again"));
    let input = vec![thesis("th_0001", "logistics:pickup")];
    let catalog = build_catalog(&m, &[], &input);
    assert_eq!(catalog.cells.len(), 4);
    assert_eq!(ids(&catalog.cells[2]), ["th_0001"]);
    assert!(catalog.cells[3].groups.is_empty());
    assert_eq!(catalog.validate(&input), Ok(()));
}

#[test]
fn an_empty_run_yields_matrix_cells_without_groups() {
    let catalog = build_catalog(&matrix(), &[], &[]);
    assert_eq!(catalog.cells.len(), 3);
    assert!(catalog.cells.iter().all(|c| c.groups.is_empty()));
    assert_eq!(catalog.member_count(), 0);
    assert_eq!(catalog.validate(&[]), Ok(()));
}

#[test]
fn validate_accepts_a_catalogue_built_from_the_same_theses() {
    let input = theses();
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(catalog.validate(&input), Ok(()));
}

#[test]
fn validate_reports_a_thesis_missing_from_the_catalogue() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    let mut input = theses();
    input.push(thesis("th_0005", "pricing:list"));
    assert_eq!(
        catalog.validate(&input),
        Err(CatalogError::Missing("th_0005".into()))
    );
}

#[test]
fn validate_reports_a_thesis_listed_twice() {
    let input = theses();
    let mut catalog = build_catalog(&matrix(), &[], &input);
    let copy = catalog.cells[0].groups.clone();
    catalog.cells[1].groups = copy;
    assert_eq!(
        catalog.validate(&input),
        Err(CatalogError::Repeated("th_0003".into(), 2))
    );
}

#[test]
fn validate_reports_an_id_that_is_not_a_thesis_of_the_run() {
    let input = theses();
    let catalog = build_catalog(&matrix(), &[], &input);
    let fewer: Vec<Thesis> = input.into_iter().filter(|s| s.id != "th_0001").collect();
    assert_eq!(
        catalog.validate(&fewer),
        Err(CatalogError::Unknown("th_0001".into()))
    );
}

#[test]
fn two_theses_with_the_same_id_fail_validation() {
    let input = vec![
        thesis("th_0001", "pricing:list"),
        thesis("th_0001", "pricing:margin"),
    ];
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(
        catalog.validate(&input),
        Err(CatalogError::Repeated("th_0001".into(), 2))
    );
}

#[test]
fn catalogue_errors_read_as_sentences() {
    assert_eq!(
        CatalogError::Missing("th_0001".into()).to_string(),
        "thesis th_0001 is missing from the catalogue"
    );
    assert_eq!(
        CatalogError::Repeated("th_0001".into(), 3).to_string(),
        "thesis th_0001 appears 3 times in the catalogue"
    );
    assert_eq!(
        CatalogError::Unknown("th_9999".into()).to_string(),
        "catalogue lists th_9999, which is not a thesis of this run"
    );
}

#[test]
fn member_count_counts_every_member_of_every_cell() {
    let mut input = theses();
    input.push(thesis("th_0008", "nowhere:at-all"));
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(catalog.member_count(), 4);
}

#[test]
fn cell_of_finds_the_cell_that_lists_a_thesis() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    let cell = catalog.cell_of("th_0001").unwrap();
    assert_eq!(cell.facet_id, "pickup");
    assert!(catalog.cell_of("th_0404").is_none());
}

#[test]
fn building_twice_yields_equal_catalogues() {
    let a = build_catalog(&matrix(), &[], &theses());
    let b = build_catalog(&matrix(), &[], &theses());
    assert_eq!(a, b);
}

#[test]
fn the_catalogue_round_trips_through_json() {
    let catalog = build_catalog(&matrix(), &[], &theses());
    let json = serde_json::to_string(&catalog).unwrap();
    let back: Catalog = serde_json::from_str(&json).unwrap();
    assert_eq!(back, catalog);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let member = &value["cells"][0]["groups"][0]["members"][0];
    assert_eq!(member["thesis_id"], "th_0002");
    assert!(member["duplicate_of"].is_null());
}
