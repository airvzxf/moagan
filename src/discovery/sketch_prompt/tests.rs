use super::*;

use crate::discovery::matrix::DISCOVERY_DIMENSIONS_SCHEMA_VERSION;
use crate::phases::util::write_json;

const PROMPT: &str = "Diseña el catálogo de refacciones.\nRegla R2: ninguna cifra inventada.";

fn cell(dimension_id: &str, facet_id: &str, label: &str) -> MatrixCell {
    MatrixCell {
        dimension_id: dimension_id.into(),
        facet_id: facet_id.into(),
        label: label.into(),
    }
}

fn pricing_cell() -> MatrixCell {
    cell("pricing", "list-price", "Pricing / List price")
}

fn context() -> SketchPromptContext {
    SketchPromptContext {
        operator_prompt: PROMPT.into(),
        constraints: vec!["Presupuesto fijo".into(), "Sin crédito a clientes".into()],
        descriptions: vec![DimensionFacetDescription::new(
            "pricing",
            "list-price",
            "How list prices are set per SKU.",
        )],
    }
}

fn write_brief(run_dir: &Path, brief: serde_json::Value) {
    std::fs::write(run_dir.join("brief.json"), brief.to_string()).unwrap();
}

fn write_dimensions(run_dir: &Path, descriptions: Vec<DimensionFacetDescription>) {
    let sidecar = DiscoveryDimensions {
        schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.into(),
        brief_hash: "hash".into(),
        dimensions: Vec::new(),
        descriptions,
        created_unix: 1,
    };
    write_json(&run_dir.join(DISCOVERY_DIMENSIONS_FILENAME), &sidecar).unwrap();
}

#[test]
fn payload_has_the_documented_layout() {
    let payload = context().user_payload(&pricing_cell(), 7);
    let expected = "<operator_prompt>\n\
         Diseña el catálogo de refacciones.\n\
         Regla R2: ninguna cifra inventada.\n\
         </operator_prompt>\n\
         \n\
         Hard constraints (answer hard_constraint_check with exactly these keys):\n\
         C1: Presupuesto fijo\n\
         C2: Sin crédito a clientes\n\
         \n\
         Exploration cell: Pricing / List price (pricing:list-price)\n\
         How list prices are set per SKU.\n\
         \n\
         Produce idea #7 for this cell. Write in the language of the operator prompt.";
    assert_eq!(payload, expected);
}

#[test]
fn payload_starts_with_the_verbatim_operator_prompt() {
    let payload = context().user_payload(&pricing_cell(), 0);
    assert!(payload.starts_with(&format!(
        "<operator_prompt>\n{PROMPT}\n</operator_prompt>\n"
    )));
}

#[test]
fn payload_numbers_every_constraint_in_brief_order() {
    let payload = context().user_payload(&pricing_cell(), 0);
    let c1 = payload.find("\nC1: Presupuesto fijo\n").unwrap();
    let c2 = payload.find("\nC2: Sin crédito a clientes\n").unwrap();
    assert!(c1 < c2);
}

#[test]
fn payload_says_so_when_the_brief_has_no_constraints() {
    let ctx = SketchPromptContext {
        constraints: Vec::new(),
        ..context()
    };
    let payload = ctx.user_payload(&pricing_cell(), 0);
    assert!(payload.contains("\nHard constraints: none (answer hard_constraint_check with {}).\n"));
    assert!(!payload.contains("\nC1: "));
}

#[test]
fn payload_carries_the_facet_description_of_its_cell() {
    let payload = context().user_payload(&pricing_cell(), 0);
    assert!(payload.contains(
        "\nExploration cell: Pricing / List price (pricing:list-price)\nHow list prices are set per SKU.\n"
    ));
}

#[test]
fn payload_skips_the_description_line_when_the_cell_has_none() {
    let other = cell("pricing", "margin", "Pricing / Margin");
    let payload = context().user_payload(&other, 3);
    assert!(payload.contains(
        "\nExploration cell: Pricing / Margin (pricing:margin)\n\nProduce idea #3 for this cell."
    ));
}

#[test]
fn payload_differs_per_iteration_so_replicas_never_share_a_cache_entry() {
    let ctx = context();
    assert_ne!(
        ctx.user_payload(&pricing_cell(), 0),
        ctx.user_payload(&pricing_cell(), 1)
    );
}

#[test]
fn payload_prefix_before_the_cell_is_identical_across_cells() {
    let ctx = context();
    let a = ctx.user_payload(&pricing_cell(), 0);
    let b = ctx.user_payload(&cell("checkout", "cfdi", "Checkout / CFDI"), 9);
    let prefix = |p: &str| p[..p.find("Exploration cell:").unwrap()].to_owned();
    assert_eq!(prefix(&a), prefix(&b));
}

#[test]
fn payload_is_deterministic() {
    let ctx = context();
    assert_eq!(
        ctx.user_payload(&pricing_cell(), 4),
        ctx.user_payload(&pricing_cell(), 4)
    );
}

#[test]
fn description_for_a_cell_without_one_is_empty() {
    let ctx = context();
    assert_eq!(
        ctx.description_for(&pricing_cell()),
        "How list prices are set per SKU."
    );
    assert_eq!(ctx.description_for(&cell("x", "y", "X / Y")), "");
}

#[test]
fn load_prefers_prompt_md_over_the_brief_paraphrase() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), PROMPT).unwrap();
    write_brief(
        dir.path(),
        serde_json::json!({"problem": "p", "raw_prompt": "paraphrase"}),
    );
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert_eq!(ctx.operator_prompt, PROMPT);
}

#[test]
fn load_falls_back_to_the_brief_raw_prompt_for_runs_without_prompt_md() {
    let dir = tempfile::tempdir().unwrap();
    write_brief(
        dir.path(),
        serde_json::json!({"problem": "p", "raw_prompt": "older run prompt"}),
    );
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert_eq!(ctx.operator_prompt, "older run prompt");
}

#[test]
fn load_falls_back_to_the_brief_when_prompt_md_is_blank() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), "  \n").unwrap();
    write_brief(
        dir.path(),
        serde_json::json!({"problem": "p", "raw_prompt": "older run prompt"}),
    );
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert_eq!(ctx.operator_prompt, "older run prompt");
}

#[test]
fn load_fails_when_no_operator_prompt_exists() {
    let dir = tempfile::tempdir().unwrap();
    write_brief(dir.path(), serde_json::json!({"problem": "p"}));
    assert!(SketchPromptContext::load(dir.path()).is_err());
}

#[test]
fn load_fails_when_the_brief_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), PROMPT).unwrap();
    assert!(SketchPromptContext::load(dir.path()).is_err());
}

#[test]
fn load_reads_the_constraints_from_an_intake_shaped_brief() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), PROMPT).unwrap();
    write_brief(
        dir.path(),
        serde_json::json!({
            "problem": "p",
            "objectives": ["o"],
            "constraints": ["Presupuesto fijo", "Sin crédito a clientes"],
            "non_goals": ["n"],
            "open_questions": [],
            "raw_prompt": PROMPT
        }),
    );
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert_eq!(
        ctx.constraints,
        vec!["Presupuesto fijo", "Sin crédito a clientes"]
    );
}

#[test]
fn load_reads_the_constraints_from_a_clarify_shaped_brief() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), PROMPT).unwrap();
    write_brief(
        dir.path(),
        serde_json::json!({
            "problem": "p",
            "deliverables": ["d"],
            "constraints": ["Presupuesto fijo"],
            "acceptance": ["a"],
            "risks": ["r"]
        }),
    );
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert_eq!(ctx.constraints, vec!["Presupuesto fijo"]);
}

#[test]
fn load_reads_the_facet_descriptions_from_the_dimensions_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), PROMPT).unwrap();
    write_brief(dir.path(), serde_json::json!({"problem": "p"}));
    write_dimensions(
        dir.path(),
        vec![DimensionFacetDescription::new(
            "pricing",
            "list-price",
            "How list prices are set per SKU.",
        )],
    );
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert_eq!(
        ctx.description_for(&pricing_cell()),
        "How list prices are set per SKU."
    );
}

#[test]
fn load_without_a_dimensions_sidecar_has_no_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prompt.md"), PROMPT).unwrap();
    write_brief(dir.path(), serde_json::json!({"problem": "p"}));
    let ctx = SketchPromptContext::load(dir.path()).unwrap();
    assert!(ctx.descriptions.is_empty());
}
