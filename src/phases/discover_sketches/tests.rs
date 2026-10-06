//! Behaviour of the discover sketch fan-out phase: deterministic
//! points, resume by sketch file, and the per-point outcomes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::ProviderRegistry;
use crate::llm::client::{LlmClient, LlmRequest, ScriptedLlmClient, ScriptedLlmResponse};
use crate::test_support::with_moagan_home;

const PROMPT: &str = "Diseña el catálogo de refacciones.\nRegla R2: ninguna cifra inventada.";

fn cell(dimension: &str, facet: &str) -> MatrixCell {
    MatrixCell {
        dimension_id: dimension.to_owned(),
        facet_id: facet.to_owned(),
        label: format!("{dimension} / {facet}"),
    }
}

fn profile(temperatures: &[f32], replicas: usize) -> TemperatureProfile {
    TemperatureProfile {
        temperatures: temperatures.to_vec(),
        replicas_per_temperature: replicas,
    }
}

fn pair(section: &str, model: &str, p: TemperatureProfile) -> (String, String, TemperatureProfile) {
    (section.to_owned(), model.to_owned(), p)
}

fn sketch_with_thesis(thesis: &str) -> Sketch {
    Sketch {
        thesis: thesis.to_owned(),
        ..Sketch::default()
    }
}

// ---------------------------------------------------------------- pure

#[test]
fn the_fan_out_runs_pair_then_cell_then_temperature_then_replica_then_index() {
    let pairs = [
        pair("a", "m1", profile(&[0.7, 1.0], 1)),
        pair("b", "m2", profile(&[0.5], 2)),
    ];
    let cells = [cell("d", "x"), cell("d", "y")];
    let points = fanout(&pairs, &cells, 2);
    let got: Vec<(usize, &str, &str, f32, usize, usize)> = points
        .iter()
        .map(|p| {
            (
                p.n,
                p.section.as_str(),
                p.cell.facet_id.as_str(),
                p.temperature,
                p.replica,
                p.index,
            )
        })
        .collect();
    let expected = vec![
        (0, "a", "x", 0.7, 0, 0),
        (1, "a", "x", 0.7, 0, 1),
        (2, "a", "x", 1.0, 0, 0),
        (3, "a", "x", 1.0, 0, 1),
        (4, "a", "y", 0.7, 0, 0),
        (5, "a", "y", 0.7, 0, 1),
        (6, "a", "y", 1.0, 0, 0),
        (7, "a", "y", 1.0, 0, 1),
        (8, "b", "x", 0.5, 0, 0),
        (9, "b", "x", 0.5, 0, 1),
        (10, "b", "x", 0.5, 1, 0),
        (11, "b", "x", 0.5, 1, 1),
        (12, "b", "y", 0.5, 0, 0),
        (13, "b", "y", 0.5, 0, 1),
        (14, "b", "y", 0.5, 1, 0),
        (15, "b", "y", 0.5, 1, 1),
    ];
    assert_eq!(got, expected);
    assert!(
        points
            .iter()
            .all(|p| p.model == if p.section == "a" { "m1" } else { "m2" })
    );
}

#[test]
fn building_the_fan_out_twice_gives_the_same_points() {
    let pairs = [pair("mock", "mock-model", profile(&[0.2, 0.9], 2))];
    let cells = [cell("d", "x"), cell("e", "y"), cell("e", "z")];
    assert_eq!(fanout(&pairs, &cells, 3), fanout(&pairs, &cells, 3));
    assert_eq!(fanout(&pairs, &cells, 3).len(), 3 * 2 * 2 * 3);
}

#[test]
fn replicas_and_sketches_per_cell_below_one_count_as_one() {
    let pairs = [pair("mock", "mock-model", profile(&[1.0], 0))];
    let points = fanout(&pairs, &[cell("d", "x")], 0);
    assert_eq!(points.len(), 1);
    assert_eq!((points[0].replica, points[0].index), (0, 0));
}

#[test]
fn an_empty_matrix_has_no_points() {
    let pairs = [pair("mock", "mock-model", profile(&[1.0], 1))];
    assert!(fanout(&pairs, &[], 4).is_empty());
}

#[test]
fn the_sketch_id_is_the_position_padded_to_four_digits() {
    let mut point = fanout(
        &[pair("mock", "mock-model", profile(&[1.0], 1))],
        &[cell("d", "x")],
        1,
    )
    .remove(0);
    assert_eq!(point.sketch_id(), "sk_0000");
    point.n = 7;
    assert_eq!(point.sketch_id(), "sk_0007");
    point.n = 12_345;
    assert_eq!(point.sketch_id(), "sk_12345");
    assert_eq!(
        sketch_path(Path::new("/run/sketches"), &point),
        PathBuf::from("/run/sketches/sk_12345.json")
    );
}

#[test]
fn only_points_without_a_sketch_file_are_pending() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sk_0001.json"), "{}").unwrap();
    std::fs::write(dir.path().join("sk_0003.json.meta.json"), "{}").unwrap();
    std::fs::create_dir(dir.path().join("sk_0002.json")).unwrap();
    std::fs::write(dir.path().join("642.json"), "{}").unwrap();
    let points = fanout(
        &[pair("mock", "mock-model", profile(&[1.0], 1))],
        &[cell("d", "x")],
        4,
    );
    let ids: Vec<String> = pending(points, dir.path())
        .iter()
        .map(FanoutPoint::sketch_id)
        .collect();
    assert_eq!(ids, ["sk_0000", "sk_0002", "sk_0003"]);
}

#[test]
fn a_thesis_of_thirty_bytes_is_kept_and_a_shorter_one_is_rejected() {
    let thirty = "x".repeat(30);
    assert!(matches!(
        classify(Ok(sketch_with_thesis(&thirty))),
        PointOutcome::Accepted(s) if s.thesis == thirty
    ));
    assert!(matches!(
        classify(Ok(sketch_with_thesis(&format!("  {}  ", "x".repeat(29))))),
        PointOutcome::Rejected { thesis_len: 29 }
    ));
    assert!(matches!(
        classify(Err(Error::InvalidState("boom".into()))),
        PointOutcome::Failed(_)
    ));
}

// --------------------------------------------------------------- phase

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

/// Every request the scripted client saw: (user payload, temperature).
type Seen = Arc<parking_lot::Mutex<Vec<(String, Option<f32>)>>>;

/// Scripted client answering every request with `answer` and recording it.
fn recording_client(answer: String) -> (Arc<ScriptedLlmClient>, Seen) {
    let seen: Seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen_by_router = Arc::clone(&seen);
    let mut client = ScriptedLlmClient::empty();
    client.set_router(move |req: &LlmRequest| {
        seen_by_router
            .lock()
            .push((req.user.clone(), req.temperature));
        ScriptedLlmResponse::accepted(answer.clone())
    });
    (Arc::new(client), seen)
}

/// Discovery config block: one `pricing` dimension with `facets`.
fn matrix_config(facets: &str, per_cell: usize) -> crate::config::Config {
    let mut cfg = crate::config::Config::default();
    cfg.discovery_matrix.matrix_spec = vec![format!("pricing={facets}")];
    cfg.discovery_matrix.sketches_per_cell = per_cell;
    cfg
}

/// `RunContext` of run `run_id` on the `mock` provider.
fn run_ctx(
    home: &MoaganHome,
    run_id: RunId,
    client: Arc<ScriptedLlmClient>,
    cfg: crate::config::Config,
) -> RunContext {
    let dyn_client: Arc<dyn LlmClient> = client;
    let mut registry = ProviderRegistry::default();
    registry.insert("mock".into(), dyn_client);
    RunContext::new_with_config(
        run_id,
        Arc::new(home.clone()),
        Arc::new(registry),
        "mock".to_owned(),
        "mock-model".to_owned(),
        crate::execution::Parallelism::new(2),
        crate::telemetry::Telemetry::noop(),
        String::new(),
        "discover".to_owned(),
        Arc::new(cfg),
    )
}

/// Seed `prompt.md` and `brief.json` the way intake leaves them.
fn seed_run_dir(home: &MoaganHome, run_id: RunId) -> PathBuf {
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
    run_dir.root().to_path_buf()
}

/// Run the phase once and return the paths it reports as written.
fn run_phase(ctx: &RunContext) -> Result<Vec<PathBuf>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                DiscoverSketchesPhase.execute(ctx),
            )
            .await
        })
        .expect("phase exceeded 30 s")?;
    match output {
        PhaseOutput::Sketches(paths) => Ok(paths),
        other => panic!("unexpected phase output {other:?}"),
    }
}

/// Primary sketch file names under `<run_dir>/sketches`, sorted.
fn sketch_names(run_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(run_dir.join("sketches"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json") && !n.ends_with(".meta.json"))
        .collect();
    names.sort();
    names
}

fn file_names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_phase_is_named_discover_sketches() {
    assert_eq!(DiscoverSketchesPhase.name(), "discover_sketches");
}

#[test]
fn a_fresh_run_writes_one_sketch_per_point_named_by_position() {
    with_moagan_home("sketches-fresh", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let (client, seen) = recording_client(sketch_answer());
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price,margin", 2),
        ))
        .unwrap();
        let expected = [
            "sk_0000.json",
            "sk_0001.json",
            "sk_0002.json",
            "sk_0003.json",
        ];
        assert_eq!(file_names(&written), expected);
        assert_eq!(sketch_names(&run_dir), expected);
        assert_eq!(seen.lock().len(), 4);
        let sketch: Sketch = read_json(&run_dir.join("sketches/sk_0002.json")).unwrap();
        assert_eq!(sketch.id, "sk_0002");
        assert_eq!(sketch.angle, "pricing:margin");
    });
}

#[test]
fn the_built_matrix_is_persisted() {
    with_moagan_home("sketches-matrix-built", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let (client, _) = recording_client(sketch_answer());
        run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price", 3),
        ))
        .unwrap();
        let matrix: ExplorationMatrix =
            read_json(&run_dir.join(EXPLORATION_MATRIX_FILENAME)).unwrap();
        assert_eq!(matrix.sketches_per_cell, 3);
        let cells: Vec<(String, String)> = matrix
            .iter_cells()
            .map(|c| (c.dimension_id, c.facet_id))
            .collect();
        assert_eq!(cells, [("pricing".to_owned(), "list-price".to_owned())]);
    });
}

#[test]
fn a_second_run_on_the_same_run_dir_makes_no_call_and_changes_no_file() {
    with_moagan_home("sketches-rerun", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let (first, _) = recording_client(sketch_answer());
        run_phase(&run_ctx(
            &home,
            run_id,
            first,
            matrix_config("list-price,margin", 2),
        ))
        .unwrap();
        let before: Vec<Vec<u8>> = sketch_names(&run_dir)
            .iter()
            .map(|n| std::fs::read(run_dir.join("sketches").join(n)).unwrap())
            .collect();

        let (second, seen) = recording_client(sketch_answer());
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            second,
            matrix_config("list-price,margin", 2),
        ))
        .unwrap();
        assert!(written.is_empty(), "nothing is pending: {written:?}");
        assert!(seen.lock().is_empty(), "no model call on a complete run");
        let after: Vec<Vec<u8>> = sketch_names(&run_dir)
            .iter()
            .map(|n| std::fs::read(run_dir.join("sketches").join(n)).unwrap())
            .collect();
        assert_eq!(before, after);
    });
}

#[test]
fn deleting_sketches_reruns_exactly_those_points() {
    with_moagan_home("sketches-delete-k", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let (first, _) = recording_client(sketch_answer());
        run_phase(&run_ctx(
            &home,
            run_id,
            first,
            matrix_config("list-price,margin", 2),
        ))
        .unwrap();
        let kept = std::fs::read(run_dir.join("sketches/sk_0000.json")).unwrap();
        std::fs::remove_file(run_dir.join("sketches/sk_0001.json")).unwrap();
        std::fs::remove_file(run_dir.join("sketches/sk_0003.json")).unwrap();

        let (second, _) = recording_client(sketch_answer());
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            second,
            matrix_config("list-price,margin", 2),
        ))
        .unwrap();
        assert_eq!(file_names(&written), ["sk_0001.json", "sk_0003.json"]);
        assert_eq!(
            sketch_names(&run_dir),
            [
                "sk_0000.json",
                "sk_0001.json",
                "sk_0002.json",
                "sk_0003.json"
            ]
        );
        assert_eq!(
            std::fs::read(run_dir.join("sketches/sk_0000.json")).unwrap(),
            kept
        );
    });
}

#[test]
fn an_existing_matrix_is_used_verbatim_not_rebuilt_from_the_config() {
    with_moagan_home("sketches-matrix-verbatim", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let spec = crate::discovery::MatrixSpec::parse_one("pricing=list-price").unwrap();
        let matrix = ExplorationMatrix::from_spec(spec, 1);
        write_json(&run_dir.join(EXPLORATION_MATRIX_FILENAME), &matrix).unwrap();
        let bytes = std::fs::read(run_dir.join(EXPLORATION_MATRIX_FILENAME)).unwrap();

        let (client, seen) = recording_client(sketch_answer());
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price,margin,volume", 3),
        ))
        .unwrap();
        assert_eq!(file_names(&written), ["sk_0000.json"]);
        assert_eq!(seen.lock().len(), 1);
        assert_eq!(
            std::fs::read(run_dir.join(EXPLORATION_MATRIX_FILENAME)).unwrap(),
            bytes
        );
    });
}

#[test]
fn sketch_files_outside_the_fan_out_are_left_untouched() {
    with_moagan_home("sketches-foreign", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        std::fs::create_dir_all(run_dir.join("sketches")).unwrap();
        std::fs::write(run_dir.join("sketches/642.json"), "legacy").unwrap();
        let (client, _) = recording_client(sketch_answer());
        run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price", 1),
        ))
        .unwrap();
        assert_eq!(sketch_names(&run_dir), ["642.json", "sk_0000.json"]);
        assert_eq!(
            std::fs::read_to_string(run_dir.join("sketches/642.json")).unwrap(),
            "legacy"
        );
    });
}

#[test]
fn a_short_thesis_leaves_no_file() {
    with_moagan_home("sketches-rejected", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let short = serde_json::json!({"id": "x", "thesis": "too short"}).to_string();
        let (client, seen) = recording_client(short);
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price", 1),
        ))
        .unwrap();
        assert!(written.is_empty());
        assert!(sketch_names(&run_dir).is_empty());
        assert_eq!(seen.lock().len(), 1, "a parsed sketch is not retried");
    });
}

#[test]
fn a_sketch_that_never_parses_is_abandoned_after_three_attempts() {
    with_moagan_home("sketches-retries", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let (client, seen) = recording_client("this is not json".to_owned());
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price", 1),
        ))
        .unwrap();
        assert!(written.is_empty());
        assert!(sketch_names(&run_dir).is_empty());
        assert_eq!(seen.lock().len(), 3, "1 attempt + 2 retries");
    });
}

#[test]
fn a_sketch_that_cannot_be_written_counts_as_failed_and_the_rest_continue() {
    with_moagan_home("sketches-write-failure", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        std::fs::create_dir_all(run_dir.join("sketches/sk_0000.json")).unwrap();
        let (client, seen) = recording_client(sketch_answer());
        let written = run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price,margin", 1),
        ))
        .unwrap();
        assert_eq!(file_names(&written), ["sk_0001.json"]);
        assert_eq!(seen.lock().len(), 2);
        assert!(run_dir.join("sketches/sk_0000.json").is_dir());
    });
}

#[test]
fn every_sketch_call_carries_the_operator_prompt_and_numbered_constraints() {
    with_moagan_home("sketches-payload", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        seed_run_dir(&home, run_id);
        let (client, seen) = recording_client(sketch_answer());
        run_phase(&run_ctx(
            &home,
            run_id,
            client,
            matrix_config("list-price,margin", 2),
        ))
        .unwrap();
        let payloads: Vec<String> = seen.lock().iter().map(|(u, _)| u.clone()).collect();
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
        let ideas: BTreeSet<&str> = payloads
            .iter()
            .map(|p| {
                let start = p.find("Produce idea #").unwrap();
                &p[start..start + "Produce idea #0".len()]
            })
            .collect();
        assert_eq!(
            ideas,
            BTreeSet::from([
                "Produce idea #0",
                "Produce idea #1",
                "Produce idea #2",
                "Produce idea #3"
            ])
        );
    });
}

#[test]
fn every_sketch_records_its_fan_out_coordinates() {
    with_moagan_home("sketches-provenance", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let run_dir = seed_run_dir(&home, run_id);
        let mut cfg = matrix_config("list-price", 1);
        cfg.discovery_matrix
            .temperature_profiles
            .insert("mock::mock-model".to_owned(), profile(&[0.3, 0.9], 2));
        let (client, seen) = recording_client(sketch_answer());
        run_phase(&run_ctx(&home, run_id, client, cfg)).unwrap();
        let mut coordinates = Vec::new();
        for name in sketch_names(&run_dir) {
            let sketch: Sketch = read_json(&run_dir.join("sketches").join(&name)).unwrap();
            let p = sketch.provenance.expect("provenance is recorded");
            assert_eq!(
                (p.section.as_str(), p.model.as_str()),
                ("mock", "mock-model")
            );
            coordinates.push((sketch.id, p.temperature, p.replica, p.index));
        }
        assert_eq!(
            coordinates,
            [
                ("sk_0000".to_owned(), 0.3, 0, 0),
                ("sk_0001".to_owned(), 0.3, 1, 0),
                ("sk_0002".to_owned(), 0.9, 0, 0),
                ("sk_0003".to_owned(), 0.9, 1, 0),
            ]
        );
        let mut temperatures: Vec<Option<f32>> = seen.lock().iter().map(|(_, t)| *t).collect();
        temperatures.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(temperatures, [Some(0.3), Some(0.3), Some(0.9), Some(0.9)]);
    });
}
