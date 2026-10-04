//! Outcome tests of the discover sketch fan-out driven through
//! `DiscoveryCoordinator::run_with_ctx` with a scripted LLM.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::*;
use crate::llm::ProviderRegistry;
use crate::llm::client::{LlmClient, LlmRequest, ScriptedLlmClient, ScriptedLlmResponse};
use crate::test_support::with_moagan_home;

const PROMPT: &str = "Diseña el catálogo de refacciones.\nRegla R2: ninguna cifra inventada.";

/// A valid sketch answer whose model-chosen id is non-canonical.
fn sketch_answer() -> String {
    serde_json::json!({
        "id": "642",
        "thesis": "Publish list prices per SKU and keep counter discounts offline.",
        "key_decisions": ["list price per SKU", "offline counter discounts"],
        "architecture_outline": "The storefront shows the list price; the counter keeps its own rules.",
        "assumptions": ["prices change weekly"],
        "strengths": ["simple"],
        "weaknesses": ["two price sources"],
        "hard_constraint_check": {"C1": true},
        "expected_validation": "Compare a week of web and counter tickets."
    })
    .to_string()
}

/// Scripted client whose router records the user payload of every
/// request and answers each one with `answer`.
fn recording_client(
    answer: String,
) -> (Arc<ScriptedLlmClient>, Arc<parking_lot::Mutex<Vec<String>>>) {
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen_by_router = Arc::clone(&seen);
    let mut client = ScriptedLlmClient::empty();
    client.set_router(move |req: &LlmRequest| {
        seen_by_router.lock().push(req.user.clone());
        ScriptedLlmResponse::accepted(answer.clone())
    });
    (Arc::new(client), seen)
}

/// `RunContext` on the `mock` provider with a 1-dimension matrix
/// (`facets` cells) and `per_cell` sketches per cell.
fn run_ctx(
    home: MoaganHome,
    client: Arc<ScriptedLlmClient>,
    facets: &str,
    per_cell: usize,
) -> Arc<RunContext> {
    let dyn_client: Arc<dyn LlmClient> = client;
    let mut registry = ProviderRegistry::default();
    registry.insert("mock".into(), dyn_client);
    let mut cfg = crate::config::Config::default();
    cfg.discovery_matrix.matrix_spec = vec![format!("pricing={facets}")];
    cfg.discovery_matrix.sketches_per_cell = per_cell;
    Arc::new(RunContext::new_with_config(
        RunId::new(),
        Arc::new(home),
        Arc::new(registry),
        "mock".to_owned(),
        "mock-model".to_owned(),
        crate::execution::Parallelism::new(1),
        crate::telemetry::Telemetry::noop(),
        String::new(),
        "standard".to_owned(),
        Arc::new(cfg),
    ))
}

/// Seed the run dir the way intake leaves it, then run the fan-out.
fn run_fan_out(
    label: &str,
    client: Arc<ScriptedLlmClient>,
    facets: &str,
    per_cell: usize,
) -> (DiscoveryOutcome, Vec<Sketch>) {
    with_moagan_home(label, |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        std::fs::write(run_dir.root().join("prompt.md"), PROMPT).unwrap();
        let brief = serde_json::json!({
            "problem": "Catálogo de refacciones",
            "objectives": ["Vender en línea"],
            "constraints": ["Presupuesto fijo", "Sin crédito a clientes"],
            "non_goals": ["Facturación"],
            "open_questions": [],
            "raw_prompt": PROMPT
        });
        std::fs::write(run_dir.brief(), brief.to_string()).unwrap();
        let coordinator = DiscoveryCoordinator::new(
            home.clone(),
            run_id,
            Cancel::new(),
            Brief::default(),
            "unused".to_owned(),
            Mode::Standard,
        );
        let ctx = run_ctx(home.clone(), client, facets, per_cell);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    coordinator.run_with_ctx(ctx),
                )
                .await
            })
            .expect("fan-out exceeded 30 s (retry budget too large?)")
            .unwrap();
        let mut sketches: Vec<Sketch> =
            crate::phases::util::primary_json_paths(&run_dir.sketches())
                .unwrap()
                .into_iter()
                .map(|path| crate::phases::util::read_json(&path).unwrap())
                .collect();
        sketches.sort_by(|a, b| a.id.cmp(&b.id));
        (outcome, sketches)
    })
}

#[test]
fn sketch_files_are_named_by_the_coordinator_not_by_the_model() {
    let (client, _) = recording_client(sketch_answer());
    let (outcome, sketches) = run_fan_out("sketch-ids", client, "list-price,margin", 2);
    assert_eq!(outcome.sketches_completed, 4);
    let ids: Vec<&str> = sketches.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["sk_0000", "sk_0001", "sk_0002", "sk_0003"]);
}

#[test]
fn every_sketch_call_carries_the_operator_prompt_and_numbered_constraints() {
    let (client, seen) = recording_client(sketch_answer());
    run_fan_out("sketch-payload", client, "list-price,margin", 2);
    let payloads = seen.lock().clone();
    assert_eq!(payloads.len(), 4);
    for payload in &payloads {
        assert!(
            payload.starts_with(&format!(
                "<operator_prompt>\n{PROMPT}\n</operator_prompt>\n"
            )),
            "{payload}"
        );
        assert!(
            payload.contains("\nC1: Presupuesto fijo\nC2: Sin crédito a clientes\n"),
            "{payload}"
        );
    }
    let distinct: BTreeSet<&String> = payloads.iter().collect();
    assert_eq!(distinct.len(), 4, "each iteration needs its own payload");
}

#[test]
fn a_sketch_that_never_parses_is_abandoned_after_three_attempts() {
    let (client, seen) = recording_client("this is not json".to_owned());
    let (outcome, sketches) = run_fan_out("sketch-retries", client, "list-price", 1);
    assert_eq!(outcome.sketches_failed, 1);
    assert!(sketches.is_empty());
    assert_eq!(seen.lock().len(), 3, "1 attempt + 2 retries");
}

#[test]
fn every_sketch_records_its_fan_out_coordinates() {
    let (client, _) = recording_client(sketch_answer());
    let (_, sketches) = run_fan_out("sketch-provenance", client, "list-price,margin", 2);
    let mut cells_and_indexes = BTreeSet::new();
    for sketch in &sketches {
        let provenance = sketch.provenance.as_ref().expect("provenance is recorded");
        assert_eq!(provenance.section, "mock");
        assert_eq!(provenance.model, "mock-model");
        assert!((provenance.temperature - 1.0).abs() < f32::EPSILON);
        assert_eq!(provenance.replica, 0);
        cells_and_indexes.insert((sketch.angle.clone(), provenance.index));
    }
    let expected: BTreeSet<(String, usize)> = ["pricing:list-price", "pricing:margin"]
        .iter()
        .flat_map(|cell| (0..2).map(move |index| (cell.to_string(), index)))
        .collect();
    assert_eq!(cells_and_indexes, expected);
}
