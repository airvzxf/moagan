//! Behaviour of the discover curate phase: one curator call per cell,
//! one retry, skip by curation file, chunking, and failures that never
//! stop the run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::discovery::matrix::{Dimension, ExplorationMatrix, Facet};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::ProviderRegistry;
use crate::llm::client::{LlmClient, LlmRequest, ScriptedLlmClient, ScriptedLlmResponse};
use crate::test_support::with_moagan_home;

/// What the scripted curator saw: (role, user payload, temperature, top_p).
type Seen = Arc<parking_lot::Mutex<Vec<(Role, String, Option<f32>, Option<f32>)>>>;

/// A valid answer for `user`: even theses in group 0, odd ones in group
/// 1, thesis 0 and 1 the representatives, no duplicate, one tension.
fn good_answer(user: &str) -> String {
    let payload: serde_json::Value = serde_json::from_str(user).unwrap();
    let n = payload["theses"].as_array().unwrap().len();
    serde_json::json!({
        "groups": [
            {"label": "Even approach", "summary": "Even theses.", "representative": 0},
            {"label": "Odd approach", "summary": "Odd theses.", "representative": 1}
        ],
        "assign": (0..n).map(|i| serde_json::json!({"n": i, "g": i % 2})).collect::<Vec<_>>(),
        "duplicates": [],
        "tensions": [[0, 1, "Even and odd cannot both win."]]
    })
    .to_string()
}

/// Scripted client: every curator request is recorded and answered by
/// `answer(call_number, user)`; any other role gets an empty object.
fn curator_client(
    answer: impl Fn(usize, &str) -> String + Send + Sync + 'static,
) -> (Arc<ScriptedLlmClient>, Seen) {
    let seen: Seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let seen_by_router = Arc::clone(&seen);
    let calls = AtomicUsize::new(0);
    let mut client = ScriptedLlmClient::empty();
    client.set_router(move |req: &LlmRequest| {
        seen_by_router
            .lock()
            .push((req.role, req.user.clone(), req.temperature, req.top_p));
        if req.role != Role::Curator {
            return ScriptedLlmResponse::accepted("{}");
        }
        let call = calls.fetch_add(1, Ordering::SeqCst);
        ScriptedLlmResponse::accepted(answer(call, &req.user))
    });
    (Arc::new(client), seen)
}

fn run_ctx(home: &MoaganHome, run_id: RunId, client: Arc<ScriptedLlmClient>) -> RunContext {
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
        Arc::new(crate::config::Config::default()),
    )
}

fn sketch(id: &str, angle: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "thesis": format!("Thesis number {}: keep one label per box.", &id[3..]),
        "key_decisions": ["one label", "printed once", "server id", "no path", "short", "sixth"],
        "hard_constraint_check": {"C1": true, "C2": false},
        "angle": angle
    })
}

/// Run dir with matrix `qr:{content, print, scan}`, a brief with two
/// constraints, and `per_cell[i]` sketches in facet `i`
/// (`sk_0000`, `sk_0001`, … numbered across cells).
fn seed(home: &MoaganHome, run_id: RunId, per_cell: [usize; 3]) -> PathBuf {
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    let root = run_dir.root().to_path_buf();
    let facets = ["content", "print", "scan"];
    let matrix = ExplorationMatrix::new(
        vec![Dimension {
            id: "qr".into(),
            label: "QR".into(),
            facets: facets
                .iter()
                .map(|f| Facet {
                    id: (*f).into(),
                    label: format!("Facet {f}"),
                })
                .collect(),
        }],
        2,
    );
    write_json(&root.join("exploration_matrix.json"), &matrix).unwrap();
    let brief = serde_json::json!({
        "problem": "Label boxes",
        "objectives": [],
        "constraints": ["No invented figures", "AGPL"],
        "non_goals": [],
        "open_questions": [],
        "raw_prompt": "Label boxes"
    });
    std::fs::write(root.join("brief.json"), brief.to_string()).unwrap();
    let sketches = root.join("sketches");
    std::fs::create_dir_all(&sketches).unwrap();
    let mut n = 0;
    for (facet, count) in facets.iter().zip(per_cell) {
        for _ in 0..count {
            let id = format!("sk_{n:04}");
            std::fs::write(
                sketches.join(format!("{id}.json")),
                sketch(&id, &format!("qr:{facet}")).to_string(),
            )
            .unwrap();
            n += 1;
        }
    }
    root
}

fn run_phase(ctx: &RunContext) -> Result<Vec<PathBuf>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                DiscoverCuratePhase.execute(ctx),
            )
            .await
        })
        .expect("phase exceeded 30 s")?;
    match output {
        PhaseOutput::Curations(paths) => Ok(paths),
        other => panic!("unexpected phase output {other:?}"),
    }
}

fn names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn curation(root: &Path, file: &str) -> Curation {
    read_json(&root.join(CURATION_DIR).join(file)).unwrap()
}

fn curator_calls(seen: &Seen) -> usize {
    seen.lock()
        .iter()
        .filter(|(role, ..)| *role == Role::Curator)
        .count()
}

#[test]
fn the_phase_is_named_discover_curate() {
    assert_eq!(DiscoverCuratePhase.name(), "discover_curate");
}

#[test]
fn every_cell_with_two_or_more_theses_gets_one_curator_call() {
    with_moagan_home("curate-fresh", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [3, 1, 2]);
        let (client, seen) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(names(&written), ["qr__content.json", "qr__scan.json"]);
        assert_eq!(curator_calls(&seen), 2);
        let content = curation(&root, "qr__content.json");
        assert_eq!(content.cell, "qr:content");
        assert_eq!(content.status, CurationStatus::Ok);
        assert_eq!(content.members, ["sk_0000", "sk_0001", "sk_0002"]);
        let labels: Vec<&str> = content.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["Even approach", "Odd approach"]);
        assert_eq!(content.tensions.len(), 1);
        assert!(!root.join(CURATION_DIR).join("qr__print.json").exists());
    });
}

#[test]
fn every_curator_call_uses_the_curator_role_at_low_temperature_without_top_p() {
    with_moagan_home("curate-sampling", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        seed(&home, run_id, [2, 0, 0]);
        let (client, seen) = curator_client(|_, user| good_answer(user));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        let seen = seen.lock();
        assert_eq!(seen.len(), 1);
        let (role, _, temperature, top_p) = &seen[0];
        assert_eq!(*role, Role::Curator);
        assert_eq!(*temperature, Some(0.2));
        assert_eq!(*top_p, None);
    });
}

#[test]
fn the_curator_sees_numbered_theses_and_the_brief_constraints() {
    with_moagan_home("curate-payload", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        seed(&home, run_id, [2, 0, 0]);
        let (client, seen) = curator_client(|_, user| good_answer(user));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        let user = seen.lock()[0].1.clone();
        assert!(!user.contains("sk_0"), "{user}");
        let payload: serde_json::Value = serde_json::from_str(&user).unwrap();
        assert_eq!(payload["cell"]["facet"], "Facet content");
        assert_eq!(
            payload["constraints"],
            serde_json::json!(["C1: No invented figures", "C2: AGPL"])
        );
        let theses = payload["theses"].as_array().unwrap();
        assert_eq!(theses.len(), 2);
        assert_eq!(theses[1]["n"], 1);
        assert_eq!(
            theses[1]["thesis"],
            "Thesis number 0001: keep one label per box."
        );
        assert_eq!(theses[0]["key_decisions"].as_array().unwrap().len(), 5);
        assert_eq!(theses[0]["violates"], serde_json::json!(["C2"]));
    });
}

#[test]
fn a_second_run_skips_every_curated_cell_and_changes_no_file() {
    with_moagan_home("curate-skip", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [3, 0, 2]);
        let (client, _) = curator_client(|_, user| good_answer(user));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        let before = std::fs::read(root.join(CURATION_DIR).join("qr__content.json")).unwrap();

        let (client, seen) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert!(written.is_empty());
        assert_eq!(seen.lock().len(), 0);
        assert_eq!(
            std::fs::read(root.join(CURATION_DIR).join("qr__content.json")).unwrap(),
            before
        );
    });
}

#[test]
fn a_cell_whose_theses_changed_is_curated_again() {
    with_moagan_home("curate-changed", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [2, 0, 2]);
        let (client, _) = curator_client(|_, user| good_answer(user));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        std::fs::write(
            root.join("sketches").join("sk_0900.json"),
            sketch("sk_0900", "qr:scan").to_string(),
        )
        .unwrap();

        let (client, seen) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(names(&written), ["qr__scan.json"]);
        assert_eq!(curator_calls(&seen), 1);
        assert_eq!(
            curation(&root, "qr__scan.json").members,
            ["sk_0002", "sk_0003", "sk_0900"]
        );
    });
}

#[test]
fn a_deleted_curation_file_costs_exactly_one_call() {
    with_moagan_home("curate-deleted", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [2, 2, 2]);
        let (client, _) = curator_client(|_, user| good_answer(user));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        std::fs::remove_file(root.join(CURATION_DIR).join("qr__print.json")).unwrap();

        let (client, _) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(names(&written), ["qr__print.json"]);
        assert_eq!(curation(&root, "qr__print.json").status, CurationStatus::Ok);
    });
}

#[test]
fn an_unusable_answer_is_retried_once_then_the_cell_is_saved_as_failed() {
    with_moagan_home("curate-failed", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [3, 0, 0]);
        let (client, seen) = curator_client(|_, _| "I cannot group these.".to_owned());
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(names(&written), ["qr__content.json"]);
        assert_eq!(curator_calls(&seen), 2);
        let failed = curation(&root, "qr__content.json");
        assert_eq!(failed.status, CurationStatus::Failed);
        assert!(failed.groups.is_empty());
        assert_eq!(failed.members, ["sk_0000", "sk_0001", "sk_0002"]);
    });
}

#[test]
fn too_many_unassigned_theses_trigger_the_retry_and_its_answer_is_kept() {
    with_moagan_home("curate-retry", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [4, 0, 0]);
        let (client, seen) = curator_client(|call, user| {
            if call == 0 {
                r#"{"groups":[{"label":"Partial"}],"assign":[{"n":0,"g":0},{"n":1,"g":0}]}"#
                    .to_owned()
            } else {
                good_answer(user)
            }
        });
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(curator_calls(&seen), 2);
        let ok = curation(&root, "qr__content.json");
        assert_eq!(ok.status, CurationStatus::Ok);
        assert_eq!(ok.groups[0].label, "Even approach");
    });
}

#[test]
fn a_failed_cell_is_tried_again_on_the_next_run() {
    with_moagan_home("curate-failed-again", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [2, 0, 0]);
        let (client, _) = curator_client(|_, _| "not json".to_owned());
        run_phase(&run_ctx(&home, run_id, client)).unwrap();

        let (client, seen) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(names(&written), ["qr__content.json"]);
        // The first attempt is answered from the cache (the old answer);
        // the uncached retry reaches the model.
        assert_eq!(curator_calls(&seen), 1);
        assert_eq!(
            curation(&root, "qr__content.json").status,
            CurationStatus::Ok
        );
    });
}

#[test]
fn an_answer_in_a_json_fence_is_accepted() {
    with_moagan_home("curate-fence", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [2, 0, 0]);
        let (client, seen) =
            curator_client(|_, user| format!("```json\n{}\n```", good_answer(user)));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(curator_calls(&seen), 1);
        assert_eq!(
            curation(&root, "qr__content.json").status,
            CurationStatus::Ok
        );
    });
}

#[test]
fn a_cell_larger_than_one_call_is_split_into_numbered_chunks() {
    with_moagan_home("curate-chunks", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [MAX_THESES_PER_CALL + 1, 0, 0]);
        let (client, seen) = curator_client(|_, user| good_answer(user));
        run_phase(&run_ctx(&home, run_id, client)).unwrap();
        let sizes: Vec<usize> = seen
            .lock()
            .iter()
            .map(|(_, user, ..)| {
                let payload: serde_json::Value = serde_json::from_str(user).unwrap();
                payload["theses"].as_array().unwrap().len()
            })
            .collect();
        assert_eq!(sizes, [70, 71]);
        let curated = curation(&root, "qr__content.json");
        assert_eq!(curated.status, CurationStatus::Ok);
        assert_eq!(curated.members.len(), MAX_THESES_PER_CALL + 1);
        assert_eq!(curated.groups.len(), 4);
        assert_eq!(curated.reports.len(), 2);
        assert!(curated.covers("qr:content", &curated.members));
    });
}

#[test]
fn sketches_outside_the_matrix_are_not_curated() {
    with_moagan_home("curate-outside", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [0, 0, 0]);
        for id in ["sk_0001", "sk_0002"] {
            std::fs::write(
                root.join("sketches").join(format!("{id}.json")),
                sketch(id, "qr:unknown-facet").to_string(),
            )
            .unwrap();
        }
        let (client, seen) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert!(written.is_empty());
        assert_eq!(seen.lock().len(), 0);
    });
}

#[test]
fn a_curation_that_cannot_be_written_does_not_stop_the_other_cells() {
    with_moagan_home("curate-unwritable", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        let root = seed(&home, run_id, [2, 2, 0]);
        std::fs::create_dir_all(root.join(CURATION_DIR).join("qr__content.json")).unwrap();
        let (client, seen) = curator_client(|_, user| good_answer(user));
        let written = run_phase(&run_ctx(&home, run_id, client)).unwrap();
        assert_eq!(names(&written), ["qr__print.json"]);
        assert_eq!(curator_calls(&seen), 2);
        assert!(root.join(CURATION_DIR).join("qr__content.json").is_dir());
    });
}

#[test]
fn a_run_dir_without_matrix_is_an_error() {
    with_moagan_home("curate-no-matrix", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        let run_id = RunId::new();
        home.run_dir(run_id).ensure().unwrap();
        let (client, _) = curator_client(|_, user| good_answer(user));
        assert!(run_phase(&run_ctx(&home, run_id, client)).is_err());
    });
}

#[test]
fn pending_cells_skip_small_and_covered_cells_but_not_failed_ones() {
    let dir = tempfile::tempdir().unwrap();
    let matrix = ExplorationMatrix::new(
        vec![Dimension {
            id: "qr".into(),
            label: "QR".into(),
            facets: ["a", "b", "c", "d"]
                .iter()
                .map(|f| Facet {
                    id: (*f).into(),
                    label: (*f).into(),
                })
                .collect(),
        }],
        2,
    );
    let sketches: Vec<Sketch> = [
        ("sk_0000", "qr:a"),
        ("sk_0001", "qr:a"),
        ("sk_0002", "qr:b"),
        ("sk_0003", "qr:c"),
        ("sk_0004", "qr:c"),
        ("sk_0005", "qr:d"),
        ("sk_0006", "qr:d"),
    ]
    .iter()
    .map(|(id, angle)| Sketch {
        id: (*id).into(),
        angle: (*angle).into(),
        ..Sketch::default()
    })
    .collect();
    let catalog = build_catalog(&matrix, &[], &sketches);
    let ok = crate::discovery::curation::normalise(
        &["sk_0003".to_owned(), "sk_0004".to_owned()],
        &serde_json::from_str(
            r#"{"groups":[{"label":"A"}],"assign":[{"n":0,"g":0},{"n":1,"g":0}]}"#,
        )
        .unwrap(),
    );
    write_json(
        &curation_path(dir.path(), "qr", "c"),
        &Curation::merge(
            "qr:c",
            vec!["sk_0003".into(), "sk_0004".into()],
            vec![Some(ok)],
        ),
    )
    .unwrap();
    write_json(
        &curation_path(dir.path(), "qr", "d"),
        &Curation::merge("qr:d", vec!["sk_0005".into(), "sk_0006".into()], vec![None]),
    )
    .unwrap();
    let keys: Vec<String> = pending_cells(&catalog.cells, dir.path())
        .iter()
        .map(cell_key)
        .collect();
    assert_eq!(keys, ["qr:a", "qr:d"]);
}

#[test]
fn load_curations_reads_every_parsable_file_in_name_order() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        load_curations(&dir.path().join("missing"))
            .unwrap()
            .is_empty()
    );
    for (file, cell) in [("qr__b.json", "qr:b"), ("qr__a.json", "qr:a")] {
        write_json(
            &dir.path().join(file),
            &Curation::merge(cell, vec!["sk_0000".into()], vec![None]),
        )
        .unwrap();
    }
    std::fs::write(dir.path().join("qr__c.json"), "not json").unwrap();
    let cells: Vec<String> = load_curations(dir.path())
        .unwrap()
        .into_iter()
        .map(|c| c.cell)
        .collect();
    assert_eq!(cells, ["qr:a", "qr:b"]);
}
