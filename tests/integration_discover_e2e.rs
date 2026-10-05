//! Outcome invariants of a full `moagan discover` mock run.
//!
//! Each test drives the real binary against an adversarial copy of
//! `tests/fixtures/mock_provider/` and asserts on the artefacts the
//! run leaves on disk, never on source text or log lines. The copy is
//! built per test (see `adversarial_mock_dir`) so the shared fixtures
//! used by linear-mode tests stay untouched.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Prompt the mock run receives. Intake must persist it verbatim.
const PROMPT: &str = "Enumera los 7 colores del arcoiris en orden";

/// `--matrix-spec auth=oauth,api-key` × `--sketches-per-cell 3`.
const EXPECTED_SKETCHES: usize = 6;

/// Intake answer whose `raw_prompt` is a paraphrase and whose lists
/// differ from the clarify fixture, so a brief overwritten by clarify
/// is detectable.
const INTAKE_FIXTURE: &str = r#"{
  "text": "{\"problem\":\"Enumerate the seven colors of the rainbow in order\",\"objectives\":[\"List the colors in standard order\",\"Keep the answer short\"],\"constraints\":[\"Standard ROYGBIV order\",\"Spanish color names\",\"No extra commentary\"],\"non_goals\":[\"Physics or wavelengths\",\"Color theory\"],\"open_questions\":[],\"raw_prompt\":\"List the rainbow colors (model paraphrase)\"}",
  "input_tokens": 100,
  "output_tokens": 120,
  "finish_reason": "end_turn"
}"#;

/// Sketch answers carrying model-chosen ids: two non-canonical ids,
/// one of them repeated, as observed in the 2026-10-02 MiniMax run.
const SKETCH_FIXTURE_IDS: [&str; 3] = ["642", "sketch-402", "sketch-402"];

fn moagan_bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_moagan")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("moagan")
        })
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn sketch_fixture(id: &str, n: usize) -> String {
    let body = serde_json::json!({
        "id": id,
        "thesis": format!("Idea {n}: list the seven colors from red to violet in one line."),
        "key_decisions": ["one line", "canonical order"],
        "architecture_outline": "Print the canonical sequence and stop; nothing else is needed.",
        "assumptions": ["the seven-color model"],
        "strengths": ["short"],
        "weaknesses": ["no nuance"],
        "hard_constraint_check": {"C1": true},
        "expected_validation": "A reader checks the order against ROYGBIV."
    });
    serde_json::json!({
        "text": body.to_string(),
        "input_tokens": 600,
        "output_tokens": 350,
        "finish_reason": "end_turn"
    })
    .to_string()
}

/// Copy the shared mock fixtures into `work/mock` and replace the
/// intake and sketch answers with the adversarial ones above.
fn adversarial_mock_dir(work: &Path) -> PathBuf {
    let shared = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider");
    let mock = work.join("mock");
    copy_dir(&shared, &mock);
    std::fs::write(mock.join("intake").join("01-intake.json"), INTAKE_FIXTURE).unwrap();
    let sketch_dir = mock.join("sketch");
    std::fs::remove_dir_all(&sketch_dir).unwrap();
    std::fs::create_dir_all(&sketch_dir).unwrap();
    for (n, id) in SKETCH_FIXTURE_IDS.iter().enumerate() {
        std::fs::write(
            sketch_dir.join(format!("04-sketch-{n:03}.json")),
            sketch_fixture(id, n),
        )
        .unwrap();
    }
    mock
}

/// Artefacts of one finished mock run.
struct MockRun {
    _work: tempfile::TempDir,
    exit_code: Option<i32>,
    run_dir: PathBuf,
    stderr: String,
}

impl MockRun {
    fn read_json(&self, rel: &str) -> serde_json::Value {
        let path = self.run_dir.join(rel);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    /// Primary sketch artefacts (`*.json` without the `.meta.json` seals).
    fn sketch_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.run_dir.join("sketches"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.ends_with(".json") && !name.ends_with(".meta.json")
            })
            .collect();
        files.sort();
        files
    }
}

/// Run the roadmap's mock discover command in a fresh temp dir.
fn run_mock_discover() -> MockRun {
    let work = tempfile::tempdir().unwrap();
    let mock = adversarial_mock_dir(work.path());
    let home = work.path().join("home");
    let runs = work.path().join("runs");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&runs).unwrap();
    let output = Command::new(moagan_bin())
        .env("MOAGAN_HOME", &home)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .args(["discover", "--non-interactive", "--prompt", PROMPT])
        .args(["--provider", "mock:mock-model", "--mock-dir"])
        .arg(&mock)
        .args([
            "--matrix-spec",
            "auth=oauth,api-key",
            "--sketches-per-cell",
            "3",
        ])
        .arg("--runs-dir")
        .arg(&runs)
        .args(["--log-format", "json", "--event-format", "jsonl"])
        .output()
        .unwrap();
    let mut run_dirs: Vec<PathBuf> = std::fs::read_dir(runs.join(".runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(run_dirs.len(), 1, "expected exactly one run dir");
    MockRun {
        _work: work,
        exit_code: output.status.code(),
        run_dir: run_dirs.remove(0),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn string_list(value: &serde_json::Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn is_canonical_sketch_id(id: &str) -> bool {
    id.strip_prefix("sk_")
        .is_some_and(|digits| digits.len() >= 4 && digits.chars().all(|c| c.is_ascii_digit()))
}

#[test]
fn mock_discover_run_completes_with_sketches_and_a_brief() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    assert!(run.run_dir.join("brief.json").is_file());
    assert!(run.run_dir.join("final").join("intake.json").is_file());
    assert!(!run.sketch_files().is_empty(), "no sketch was persisted");
}

#[test]
fn intake_persists_the_verbatim_operator_prompt() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let prompt_md = std::fs::read_to_string(run.run_dir.join("prompt.md")).unwrap();
    assert_eq!(prompt_md, PROMPT);
    let intake = run.read_json("final/intake.json");
    assert_eq!(intake["raw_prompt"], PROMPT);
}

/// I1: the brief the sketches read keeps every non-empty intake list.
#[test]
fn i1_brief_keeps_every_intake_list() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let intake = run.read_json("final/intake.json");
    let brief = run.read_json("brief.json");
    for field in ["objectives", "constraints", "non_goals", "open_questions"] {
        let expected = string_list(&intake, field);
        if expected.is_empty() {
            continue;
        }
        assert_eq!(
            string_list(&brief, field),
            expected,
            "brief.json lost or rewrote intake field `{field}`"
        );
    }
}

/// I2: every sketch file is named after its id, the id is canonical,
/// and no iteration overwrote another one.
#[test]
fn i2_sketch_ids_are_canonical_and_unique() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let files = run.sketch_files();
    let mut ids = BTreeSet::new();
    for path in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let sketch: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let id = sketch["id"].as_str().unwrap_or_default().to_owned();
        assert_eq!(id, stem, "sketch id must match its file name");
        assert!(
            is_canonical_sketch_id(&id),
            "non-canonical sketch id {id:?}"
        );
        ids.insert(id);
    }
    assert_eq!(
        ids.len(),
        EXPECTED_SKETCHES,
        "one sketch file per fan-out iteration; got {files:?}"
    );
}

/// Facet files of the catalogue: every `final/<dimension>/<facet>.md`.
fn facet_files(final_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(final_dir).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_some_and(|e| e == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// I3: every sketch id is listed exactly once in `final/catalog.json`
/// and is the heading of exactly one entry in exactly one facet file.
#[test]
fn i3_every_sketch_appears_exactly_once_in_the_catalogue() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let ids: Vec<String> = run
        .sketch_files()
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), EXPECTED_SKETCHES);

    let catalog = run.read_json("final/catalog.json");
    let mut listed: Vec<String> = catalog["sketches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect();
    listed.sort();
    assert_eq!(listed, ids, "catalog.json must list every sketch once");

    let final_dir = run.run_dir.join("final");
    let facets = facet_files(&final_dir);
    assert_eq!(
        facets.len(),
        2,
        "one facet file per matrix cell: {facets:?}"
    );
    let texts: Vec<String> = facets
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    for id in &ids {
        let heading = format!("### {id}");
        let hits: usize = texts
            .iter()
            .map(|t| t.lines().filter(|l| *l == heading).count())
            .sum();
        assert_eq!(hits, 1, "{heading} must appear in exactly one facet file");
    }
}

/// I4: the coverage line of `final/README.md` is computed from the catalogue.
#[test]
fn i4_readme_coverage_is_computed_from_the_catalogue() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let readme = std::fs::read_to_string(run.run_dir.join("final").join("README.md")).unwrap();
    assert!(
        readme.contains("\n- Theses in this catalogue: 6 of 6 sketches (100.0 %)\n"),
        "README.md:\n{readme}"
    );
}

/// The catalogue replaces the LLM category documents and summary.
#[test]
fn discover_no_longer_writes_category_documents_or_a_summary() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let names: Vec<String> = std::fs::read_dir(run.run_dir.join("final"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| !n.starts_with("cat_") && !n.starts_with("summary.")),
        "final/ still holds legacy files: {names:?}"
    );
}
