use super::*;

use crate::discovery::curation::RawCuration;
use crate::phases::util::{RepairKind, parse_model_json_traced};

fn traced_value(raw: &str) -> (crate::error::Result<serde_json::Value>, Vec<RepairKind>) {
    let mut kinds = Vec::new();
    let result = parse_model_json_traced::<serde_json::Value, _>(raw, |ev| kinds.push(ev.kind));
    (result, kinds)
}

/// The S2 shape "one `]` too many after the duplicate pairs".
const EXTRA_BRACKET: &str = r#"{"groups":[{"label":"Floor price","summary":"One rule.","representative":0}],"assign":[{"n":0,"g":0},{"n":1,"g":0}],"duplicates":[[1,0],[2,0]]],"tensions":[[0,1,"Thesis 0 fixes the price, thesis 1 lets it float."]]}"#;

/// The S2 shape "a `]}` that closes the answer after the groups".
const EARLY_CLOSE: &str = r#"{"groups":[{"label":"Floor price","summary":"One rule.","representative":0}]}],
"assign":[{"n":0,"g":0},{"n":1,"g":0}],"duplicates":[],"tensions":[]}"#;

/// The S2 shape "a `}` that closes the answer before the tensions".
const STRAY_BRACE: &str = r#"{"groups":[{"label":"Floor price","summary":"One rule.","representative":0}],"assign":[{"n":0,"g":0},{"n":1,"g":0}],"duplicates":[[1,0]]},"tensions":[[0,1,"Thesis 0 fixes the price."]]}"#;

/// The S2 shape "tension notes without their opening quote".
const BARE_NOTES: &str = r#"{"groups":[{"label":"Floor price","summary":"One rule.","representative":0}],"assign":[{"n":0,"g":0},{"n":1,"g":0}],"duplicates":[],"tensions":[[0,1,0 fixes the price while 1 lets it float."],[1,2,1 keeps a weekly list, 2 reprices daily."]]}"#;

#[test]
fn text_that_goes_on_after_a_value_continues_it() {
    assert!(continues_after(r#"],"assign":[]}"#));
    assert!(continues_after("\n , \"b\": 1}"));
    assert!(continues_after("}"));
    assert!(!continues_after(""));
    assert!(!continues_after("  \nHope that helps!"));
}

#[test]
fn a_closer_that_matches_no_open_container_is_dropped() {
    assert_eq!(
        drop_stray_closers(r#"{"d":[[1,2],[3,4]]],"t":[]}"#).as_deref(),
        Some(r#"{"d":[[1,2],[3,4]],"t":[]}"#)
    );
}

#[test]
fn a_closer_that_ends_the_answer_while_it_goes_on_is_dropped() {
    assert_eq!(
        drop_stray_closers(r#"{"g":[{"a":1}]}],"assign":[{"n":0}]}"#).as_deref(),
        Some(r#"{"g":[{"a":1}],"assign":[{"n":0}]}"#)
    );
    assert_eq!(
        drop_stray_closers(r#"{"d":[[5,0]]},"t":[[1,2,"x"]]}"#).as_deref(),
        Some(r#"{"d":[[5,0]],"t":[[1,2,"x"]]}"#)
    );
}

#[test]
fn closers_inside_strings_and_balanced_text_are_left_alone() {
    assert_eq!(drop_stray_closers(r#"{"a":"]]}","b":[1,[2]]}"#), None);
    assert_eq!(drop_stray_closers(r#"{"a":1} and prose after it"#), None);
}

#[test]
fn a_string_value_without_its_opening_quote_gets_one() {
    assert_eq!(
        quote_bare_strings(r#"{"t":[[3,5,3 keeps the price."],[4,6,4 moves it."]]}"#).as_deref(),
        Some(r#"{"t":[[3,5,"3 keeps the price."],[4,6,"4 moves it."]]}"#)
    );
    assert_eq!(
        quote_bare_strings(r#"{"label": Floor price", "n": 1}"#).as_deref(),
        Some(r#"{"label": "Floor price", "n": 1}"#)
    );
}

#[test]
fn literals_and_quoted_values_are_not_quoted() {
    assert_eq!(
        quote_bare_strings(r#"{"a":[1, -2.5, 1e3, true, false, null, "x", {"b": []}]}"#),
        None
    );
}

#[test]
fn a_bare_word_without_a_closing_quote_is_left_alone() {
    assert_eq!(quote_bare_strings(r#"{"a":[1, 2, note]}"#), None);
    assert_eq!(quote_bare_strings("{\"a\":[1, note\n\"b\"]}"), None);
}

#[test]
fn an_extra_bracket_after_the_duplicates_is_repaired() {
    let (result, kinds) = traced_value(EXTRA_BRACKET);
    let value = result.expect("the extra `]` is dropped");
    assert_eq!(value["duplicates"], serde_json::json!([[1, 0], [2, 0]]));
    assert_eq!(
        value["tensions"][0][2],
        "Thesis 0 fixes the price, thesis 1 lets it float."
    );
    assert!(kinds.contains(&RepairKind::Bracket), "got {kinds:?}");
}

#[test]
fn an_answer_closed_early_keeps_every_field() {
    // Before the fix the tolerant extractor returned only `groups`.
    let (result, kinds) = traced_value(EARLY_CLOSE);
    let value = result.expect("the stray `}]` is dropped");
    assert_eq!(value["assign"].as_array().map(Vec::len), Some(2));
    assert!(kinds.contains(&RepairKind::Bracket), "got {kinds:?}");
}

#[test]
fn a_stray_brace_before_the_tensions_keeps_the_tensions() {
    // Before the fix the tolerant extractor dropped `tensions`.
    let (result, _) = traced_value(STRAY_BRACE);
    let value = result.expect("the stray `}` is dropped");
    assert_eq!(value["duplicates"], serde_json::json!([[1, 0]]));
    assert_eq!(value["tensions"][0][2], "Thesis 0 fixes the price.");
}

#[test]
fn tension_notes_without_their_opening_quote_are_repaired() {
    let (result, kinds) = traced_value(BARE_NOTES);
    let value = result.expect("the missing quotes are inserted");
    assert_eq!(
        value["tensions"][1],
        serde_json::json!([1, 2, "1 keeps a weekly list, 2 reprices daily."])
    );
    assert!(kinds.contains(&RepairKind::Quote), "got {kinds:?}");
}

#[test]
fn the_curator_schema_accepts_every_repaired_shape() {
    for raw in [EXTRA_BRACKET, EARLY_CLOSE, STRAY_BRACE, BARE_NOTES] {
        let curation = parse_model_json_traced::<RawCuration, _>(raw, |_| {})
            .unwrap_or_else(|e| panic!("{raw} must parse: {e}"));
        assert_eq!(curation.assign.as_array().map(Vec::len), Some(2), "{raw}");
    }
}

#[test]
fn two_objects_in_a_row_still_give_the_first_one() {
    // The old last resort: the first complete value wins.
    let (result, _) = traced_value(r#"{"a": 1}, {"b": 2}"#);
    assert_eq!(result.expect("first object"), serde_json::json!({"a": 1}));
}

#[test]
fn the_quote_repair_has_its_own_label() {
    assert_eq!(RepairKind::Quote.as_str(), "quote");
}
