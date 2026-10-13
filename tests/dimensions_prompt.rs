//! The dimension deriver derives product decisions, not the structure
//! of the operator's prompt (#1000). Measured against MiniMax-M3 at the
//! sampling it really runs with (temperature 0.6, top_p 0.95), 5 trials
//! on each of two real structured prompts: trials whose dimensions were
//! all product decisions went from 5 of 10 with the old text to 8 of 10.

use moagan::llm::Role;
use moagan::llm::prompts::system_prompt;

fn prompt() -> &'static str {
    system_prompt(Role::DimensionDeriver)
}

#[test]
fn named_axes_become_one_dimension_each() {
    assert!(
        prompt().contains("output exactly one dimension per named axis"),
        "{}",
        prompt()
    );
}

#[test]
fn facets_are_alternatives_inside_a_dimension() {
    assert!(
        prompt().contains("Facets are alternatives inside a dimension"),
        "{}",
        prompt()
    );
}

#[test]
fn a_dimension_is_never_about_the_prompt_itself() {
    let text = prompt();
    assert!(
        text.contains("A dimension is never about the prompt itself"),
        "{text}"
    );
    for section in ["MVP vs. annex", "deliverables", "evaluated or falsified"] {
        assert!(
            text.contains(section),
            "names {section:?} as a non-dimension"
        );
    }
    assert!(text.contains("Constraints bound every cell; they are not cells."));
}
