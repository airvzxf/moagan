use super::*;

fn cell(dimension_id: &str, facet_id: &str) -> MatrixCell {
    MatrixCell {
        dimension_id: dimension_id.into(),
        facet_id: facet_id.into(),
        label: format!("{dimension_id} / {facet_id}"),
    }
}

#[test]
fn an_entry_names_a_cell_and_a_text() {
    let exclusion = CellExclusion::parse("platform/database=SQLite").unwrap();
    assert_eq!(
        exclusion,
        CellExclusion {
            dimension_id: "platform".into(),
            facet_id: "database".into(),
            text: "SQLite".into(),
        }
    );
}

#[test]
fn the_text_keeps_every_later_equals_sign_and_slash() {
    let exclusion = CellExclusion::parse(" auth / oauth = a/b test: x=1 ").unwrap();
    assert_eq!(exclusion.dimension_id, "auth");
    assert_eq!(exclusion.facet_id, "oauth");
    assert_eq!(exclusion.text, "a/b test: x=1");
}

#[test]
fn a_malformed_entry_is_an_invalid_argument_naming_the_form() {
    for entry in [
        "",
        "platform/database",
        "platform=SQLite",
        "/database=SQLite",
        "platform/=SQLite",
        "platform/database=  ",
    ] {
        match CellExclusion::parse(entry) {
            Err(Error::InvalidArgs(message)) => {
                assert!(
                    message.contains("expected 'dimension/facet=text'"),
                    "{entry:?}: {message}"
                );
            }
            other => panic!("{entry:?}: expected InvalidArgs, got {other:?}"),
        }
    }
}

#[test]
fn an_exclusion_applies_only_to_its_own_cell() {
    let exclusion = CellExclusion::parse("platform/database=SQLite").unwrap();
    assert!(exclusion.applies_to(&cell("platform", "database")));
    assert!(!exclusion.applies_to(&cell("platform", "queue")));
    assert!(!exclusion.applies_to(&cell("storage", "database")));
}
