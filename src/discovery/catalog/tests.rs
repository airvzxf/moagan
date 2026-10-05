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

fn sketch(id: &str, angle: &str) -> Sketch {
    Sketch {
        id: id.into(),
        thesis: format!("Thesis of {id}"),
        angle: angle.into(),
        ..Sketch::default()
    }
}

fn sketches() -> Vec<Sketch> {
    vec![
        sketch("sk_0003", "pricing:list"),
        sketch("sk_0001", "logistics:pickup"),
        sketch("sk_0002", "pricing:list"),
    ]
}

fn ids(cell: &CellEntry) -> Vec<&str> {
    cell.groups
        .iter()
        .flat_map(|g| g.members.iter())
        .map(|m| m.sketch_id.as_str())
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
    let catalog = build_catalog(&matrix(), &[], &sketches());
    assert_eq!(
        keys(&catalog),
        ["pricing:list", "pricing:margin", "logistics:pickup"]
    );
}

#[test]
fn cell_labels_come_from_the_matrix() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    let cell = &catalog.cells[0];
    assert_eq!(cell.dimension_label, "Pricing");
    assert_eq!(cell.facet_label, "List price");
}

#[test]
fn every_sketch_lands_in_the_cell_named_by_its_angle() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    assert_eq!(ids(&catalog.cells[0]), ["sk_0002", "sk_0003"]);
    assert_eq!(ids(&catalog.cells[2]), ["sk_0001"]);
}

#[test]
fn members_are_ordered_by_sketch_id() {
    let input = vec![
        sketch("sk_0010", "pricing:list"),
        sketch("sk_0002", "pricing:list"),
        sketch("sk_0007", "pricing:list"),
    ];
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(ids(&catalog.cells[0]), ["sk_0002", "sk_0007", "sk_0010"]);
}

#[test]
fn a_cell_without_sketches_has_no_groups() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    assert!(catalog.cells[1].groups.is_empty());
}

#[test]
fn a_non_empty_cell_has_one_implicit_group_without_duplicates() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    let groups = &catalog.cells[0].groups;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].label, DEFAULT_GROUP_LABEL);
    assert_eq!(groups[0].label, "All theses");
    assert_eq!(groups[0].summary, "");
    assert!(groups[0].members.iter().all(|m| m.duplicate_of.is_none()));
}

#[test]
fn a_sketch_with_an_unknown_angle_goes_to_a_trailing_outside_cell() {
    let mut input = sketches();
    input.push(sketch("sk_0009", "pricing:unknown"));
    input.push(sketch("sk_0004", ""));
    let catalog = build_catalog(&matrix(), &[], &input);
    let last = catalog.cells.last().unwrap();
    assert_eq!(catalog.cells.len(), 4);
    assert_eq!(last.dimension_id, OUTSIDE_DIMENSION_ID);
    assert_eq!(last.facet_id, OUTSIDE_FACET_ID);
    assert_eq!(last.dimension_label, "Outside the matrix");
    assert_eq!(last.facet_label, "Unmatched angle");
    assert_eq!(last.description, "");
    assert_eq!(ids(last), ["sk_0004", "sk_0009"]);
}

#[test]
fn the_outside_cell_exists_only_when_a_sketch_needs_it() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
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
    let catalog = build_catalog(&matrix(), &descriptions, &sketches());
    assert_eq!(catalog.cells[0].description, "");
    assert_eq!(catalog.cells[1].description, "How margin is protected.");
    assert_eq!(catalog.cells[2].description, "");
}

#[test]
fn old_non_canonical_ids_are_kept_as_they_are() {
    let input = vec![
        sketch("642", "pricing:margin"),
        sketch("sketch-402", "pricing:margin"),
    ];
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(ids(&catalog.cells[1]), ["642", "sketch-402"]);
    assert_eq!(catalog.validate(&input), Ok(()));
}

#[test]
fn a_repeated_matrix_cell_receives_sketches_only_once() {
    let mut m = matrix();
    m.dimensions[1].facets.push(facet("pickup", "Pickup again"));
    let input = vec![sketch("sk_0001", "logistics:pickup")];
    let catalog = build_catalog(&m, &[], &input);
    assert_eq!(catalog.cells.len(), 4);
    assert_eq!(ids(&catalog.cells[2]), ["sk_0001"]);
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
fn validate_accepts_a_catalogue_built_from_the_same_sketches() {
    let input = sketches();
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(catalog.validate(&input), Ok(()));
}

#[test]
fn validate_reports_a_sketch_missing_from_the_catalogue() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    let mut input = sketches();
    input.push(sketch("sk_0005", "pricing:list"));
    assert_eq!(
        catalog.validate(&input),
        Err(CatalogError::Missing("sk_0005".into()))
    );
}

#[test]
fn validate_reports_a_sketch_listed_twice() {
    let input = sketches();
    let mut catalog = build_catalog(&matrix(), &[], &input);
    let copy = catalog.cells[0].groups.clone();
    catalog.cells[1].groups = copy;
    assert_eq!(
        catalog.validate(&input),
        Err(CatalogError::Repeated("sk_0003".into(), 2))
    );
}

#[test]
fn validate_reports_an_id_that_is_not_a_sketch_of_the_run() {
    let input = sketches();
    let catalog = build_catalog(&matrix(), &[], &input);
    let fewer: Vec<Sketch> = input.into_iter().filter(|s| s.id != "sk_0001").collect();
    assert_eq!(
        catalog.validate(&fewer),
        Err(CatalogError::Unknown("sk_0001".into()))
    );
}

#[test]
fn two_sketches_with_the_same_id_fail_validation() {
    let input = vec![
        sketch("sk_0001", "pricing:list"),
        sketch("sk_0001", "pricing:margin"),
    ];
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(
        catalog.validate(&input),
        Err(CatalogError::Repeated("sk_0001".into(), 2))
    );
}

#[test]
fn catalogue_errors_read_as_sentences() {
    assert_eq!(
        CatalogError::Missing("sk_0001".into()).to_string(),
        "sketch sk_0001 is missing from the catalogue"
    );
    assert_eq!(
        CatalogError::Repeated("sk_0001".into(), 3).to_string(),
        "sketch sk_0001 appears 3 times in the catalogue"
    );
    assert_eq!(
        CatalogError::Unknown("sk_9999".into()).to_string(),
        "catalogue lists sk_9999, which is not a sketch of this run"
    );
}

#[test]
fn member_count_counts_every_member_of_every_cell() {
    let mut input = sketches();
    input.push(sketch("sk_0008", "nowhere:at-all"));
    let catalog = build_catalog(&matrix(), &[], &input);
    assert_eq!(catalog.member_count(), 4);
}

#[test]
fn cell_of_finds_the_cell_that_lists_a_sketch() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    let cell = catalog.cell_of("sk_0001").unwrap();
    assert_eq!(cell.facet_id, "pickup");
    assert!(catalog.cell_of("sk_0404").is_none());
}

#[test]
fn building_twice_yields_equal_catalogues() {
    let a = build_catalog(&matrix(), &[], &sketches());
    let b = build_catalog(&matrix(), &[], &sketches());
    assert_eq!(a, b);
}

#[test]
fn the_catalogue_round_trips_through_json() {
    let catalog = build_catalog(&matrix(), &[], &sketches());
    let json = serde_json::to_string(&catalog).unwrap();
    let back: Catalog = serde_json::from_str(&json).unwrap();
    assert_eq!(back, catalog);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let member = &value["cells"][0]["groups"][0]["members"][0];
    assert_eq!(member["sketch_id"], "sk_0002");
    assert!(member["duplicate_of"].is_null());
}
