use super::*;

use crate::domain::Intake;
use crate::error::Error;
use crate::phases::util::{parse_model_json, parse_model_json_traced};

fn traced_intake(raw: &str) -> crate::error::Result<Intake> {
    parse_model_json_traced::<Intake, _>(raw, |_| {})
}

fn assert_fragment_error(result: crate::error::Result<Intake>) {
    match result {
        Err(Error::SchemaViolation(message)) => assert!(
            message.contains("fragment"),
            "the error must say the answer is a fragment, got {message:?}"
        ),
        Err(other) => panic!("expected Error::SchemaViolation, got {other:?}"),
        Ok(intake) => panic!("a fragment must not parse, got {intake:?}"),
    }
}

#[test]
fn an_object_closed_by_the_text_is_closed() {
    assert!(closes_object(r#"{"a": 1}"#, 0));
    assert!(closes_object(r#"Here: {"a": [1, {"b": 2}]} tail"#, 6));
}

#[test]
fn an_object_the_text_never_closes_is_not_closed() {
    assert!(!closes_object(r#"{"a": 1,"#, 0));
    assert!(!closes_object(r#"{"a": [1, 2]"#, 0));
}

#[test]
fn braces_inside_strings_do_not_close_the_object() {
    assert!(!closes_object(r#"{"a": "}", "b": "\"}""#, 0));
    assert!(closes_object(r#"{"a": "}", "b": "\"}"}"#, 0));
}

#[test]
fn a_position_that_is_not_an_open_brace_is_not_an_object() {
    assert!(!closes_object(r#"["a"]"#, 0));
    assert!(!closes_object(r#"{"a": 1}"#, 3));
    assert!(!closes_object("", 0));
}

#[test]
fn a_bare_field_fragment_is_a_parse_failure() {
    assert_fragment_error(traced_intake(
        r#""problem": "Design the spare-parts price list","#,
    ));
}

#[test]
fn a_fragment_with_a_list_is_a_parse_failure() {
    assert_fragment_error(traced_intake(
        r#""problem": "Design the price list", "constraints": ["Fixed budget"]"#,
    ));
}

#[test]
fn a_fragment_with_an_inner_object_is_a_parse_failure() {
    // Without the check, the tolerant extractor would pick the inner
    // object and return an Intake whose fields are all empty.
    assert_fragment_error(traced_intake(
        r#""problem": "Design the price list", "meta": {"source": "prompt"}"#,
    ));
}

#[test]
fn a_fragment_after_a_prose_line_is_a_parse_failure() {
    assert_fragment_error(traced_intake(
        "Here is the brief:\n\"problem\": \"Design the price list\",",
    ));
}

#[test]
fn the_untraced_parser_rejects_a_fragment_too() {
    let result = parse_model_json::<Intake>(r#""problem": "Design the price list","#);
    assert!(
        matches!(result, Err(Error::SchemaViolation(_))),
        "got {result:?}"
    );
}

#[test]
fn an_answer_missing_only_its_open_brace_still_parses() {
    let intake = traced_intake(
        "\"problem\": \"Design the price list\",\n\"constraints\": [\"Fixed budget\"]\n}",
    )
    .expect("an object the model closed itself must parse");
    assert_eq!(intake.problem, "Design the price list");
    assert_eq!(intake.constraints, vec!["Fixed budget".to_owned()]);
}

#[test]
fn a_complete_answer_is_untouched() {
    let intake =
        traced_intake(r#"{"problem": "Design the price list", "constraints": ["A", "B"]}"#)
            .expect("valid JSON parses");
    assert_eq!(intake.constraints.len(), 2);
}
