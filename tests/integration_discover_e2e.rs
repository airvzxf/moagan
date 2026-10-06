//! Outcome invariants of a full `moagan discover` mock run.
//!
//! Each test drives the real binary against an adversarial copy of
//! `tests/fixtures/mock_provider/` and asserts on the artefacts the
//! run leaves on disk, never on source text or log lines. The copy is
//! built per test (see `adversarial_mock_dir`) so the shared fixtures
//! used by linear-mode tests stay untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
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
    run_mock_discover_with(|_| {})
}

/// Like [`run_mock_discover`], after `tweak` edits the mock dir.
fn run_mock_discover_with(tweak: impl FnOnce(&Path)) -> MockRun {
    let work = tempfile::tempdir().unwrap();
    let mock = adversarial_mock_dir(work.path());
    tweak(&mock);
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
/// and is the heading of exactly one entry in exactly one facet file
/// (`### <id>`, or `### ★ <id>` for a group's representative), also
/// when the curator groups the cells and folds duplicates.
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
        let starred = format!("### ★ {id}");
        let hits: usize = texts
            .iter()
            .map(|t| t.lines().filter(|l| *l == heading || *l == starred).count())
            .sum();
        assert_eq!(hits, 1, "{heading} must appear in exactly one facet file");
    }
    assert!(
        texts.iter().all(|t| t.contains("<summary>1 duplicate of ")),
        "each cell folds its duplicate"
    );
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

/// Run `moagan continue --kind discovery` on the run of `run`, with
/// the run's `--runs-dir` as `MOAGAN_HOME`, plus `extra` arguments.
fn continue_discover(run: &MockRun, extra: &[&str]) -> std::process::Output {
    let runs = run.run_dir.parent().unwrap().parent().unwrap();
    let run_id = run
        .run_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    Command::new(moagan_bin())
        .env("MOAGAN_HOME", runs)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .args(["continue", "--kind", "discovery", "--run-id", &run_id])
        .args([
            "--non-interactive",
            "--log-format",
            "json",
            "--event-format",
            "jsonl",
        ])
        .args(extra)
        .output()
        .unwrap()
}

/// Every row of `telemetry/calls.jsonl.gz` (an appended multi-member gzip).
fn call_rows(run_dir: &Path) -> Vec<serde_json::Value> {
    let path = run_dir.join("telemetry").join("calls.jsonl.gz");
    let bytes = std::fs::read(&path).unwrap_or_default();
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn sketch_call_count(run_dir: &Path) -> usize {
    role_call_count(run_dir, "sketch")
}

fn role_call_count(run_dir: &Path, role: &str) -> usize {
    call_rows(run_dir)
        .iter()
        .filter(|row| row["role"] == role)
        .count()
}

/// Every primary file under `dir` (no `.meta.json` seals, which carry
/// their own write time), keyed by its path relative to `dir`.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if !path.to_string_lossy().ends_with(".meta.json") {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// A fresh run records the operator's choices for `continue`.
#[test]
fn a_fresh_run_records_its_choices_in_discover_run_json() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let spec = run.read_json("discover_run.json");
    assert_eq!(spec["provider"], "mock:mock-model");
    assert_eq!(spec["sketches_per_cell"], 3);
    assert_eq!(
        spec["matrix_spec"],
        serde_json::json!(["auth=oauth,api-key"])
    );
    assert!(
        spec["mock_dir"]
            .as_str()
            .is_some_and(|d| d.ends_with("mock")),
        "{spec}"
    );
}

/// I5: continuing a complete run makes no model call and re-renders
/// the same `final/`.
#[test]
fn i5_continuing_a_complete_run_makes_no_call_and_rerenders_the_same_catalogue() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let calls_before = call_rows(&run.run_dir).len();
    let final_before = snapshot(&run.run_dir.join("final"));
    std::fs::remove_file(run.run_dir.join("final").join("README.md")).unwrap();
    std::fs::remove_file(run.run_dir.join("final").join("catalog.json")).unwrap();

    let output = continue_discover(&run, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        call_rows(&run.run_dir).len(),
        calls_before,
        "a complete run must not call the model"
    );
    assert_eq!(snapshot(&run.run_dir.join("final")), final_before);
}

/// I6: deleting k sketch files and continuing makes exactly k sketch
/// calls and leaves every other sketch untouched.
#[test]
fn i6_continuing_after_deleting_k_sketches_makes_exactly_k_sketch_calls() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let names_before: Vec<PathBuf> = run.sketch_files();
    let kept: BTreeMap<PathBuf, Vec<u8>> = names_before
        .iter()
        .filter(|p| !p.ends_with("sk_0001.json") && !p.ends_with("sk_0004.json"))
        .map(|p| (p.clone(), std::fs::read(p).unwrap()))
        .collect();
    assert_eq!(kept.len(), EXPECTED_SKETCHES - 2);
    let sketch_calls_before = sketch_call_count(&run.run_dir);
    let curator_calls_before = role_call_count(&run.run_dir, "curator");
    let sketches = run.run_dir.join("sketches");
    std::fs::remove_file(sketches.join("sk_0001.json")).unwrap();
    std::fs::remove_file(sketches.join("sk_0004.json")).unwrap();

    let output = continue_discover(&run, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(sketch_call_count(&run.run_dir), sketch_calls_before + 2);
    assert_eq!(
        role_call_count(&run.run_dir, "curator"),
        curator_calls_before,
        "the regenerated sketches keep their ids, so no cell is curated again"
    );
    assert_eq!(run.sketch_files(), names_before);
    for (path, bytes) in &kept {
        assert_eq!(
            &std::fs::read(path).unwrap(),
            bytes,
            "{} changed",
            path.display()
        );
    }
    let catalog = run.read_json("final/catalog.json");
    assert_eq!(
        catalog["sketches"].as_array().unwrap().len(),
        EXPECTED_SKETCHES
    );
}

/// `continue --kind discovery` refuses the provider-switch flags: a
/// discover run resumes with the provider it started with.
#[test]
fn continuing_a_discover_run_refuses_the_switch_flags() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let output = continue_discover(&run, &["--switch-provider", "mock"]);
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("do not apply to --kind discovery"),
        "stderr:\n{stderr}"
    );
}

/// Every matrix cell gets one curator call; its curation file is
/// written and the facet file shows the curator's groups.
#[test]
fn every_cell_is_curated_once_and_its_facet_file_shows_the_groups() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    assert_eq!(role_call_count(&run.run_dir, "curator"), 2);
    for file in ["auth__oauth.json", "auth__api-key.json"] {
        let curation = run.read_json(&format!("curation/{file}"));
        assert_eq!(curation["status"], "ok", "{file}: {curation}");
        assert_eq!(curation["members"].as_array().unwrap().len(), 3);
    }
    let oauth = std::fs::read_to_string(run.run_dir.join("final/auth/oauth.md")).unwrap();
    assert!(oauth.contains("\n3 theses in 2 groups.\n"), "{oauth}");
    assert!(oauth.contains("\n## Colors listed in one line\n"));
    assert!(oauth.contains("\n### ★ sk_0000\n"));
    assert!(oauth.contains("\n## Tensions\n"));
    let readme = std::fs::read_to_string(run.run_dir.join("final/README.md")).unwrap();
    assert!(
        readme.contains("\n- Grouped cells: 2 of 2 with theses\n"),
        "{readme}"
    );
    let catalog = run.read_json("final/catalog.json");
    let duplicates: Vec<(&str, &str)> = catalog["sketches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| Some((s["id"].as_str()?, s["duplicate_of"].as_str()?)))
        .collect();
    assert_eq!(duplicates, [("sk_0002", "sk_0000"), ("sk_0005", "sk_0003")]);
}

/// I7: a cell whose curation fails twice is saved as failed, listed
/// flat with a visible warning, and still lists every thesis.
#[test]
fn i7_a_failed_curation_leaves_the_cell_flat_with_a_visible_warning() {
    let run = run_mock_discover_with(|mock| {
        std::fs::write(
            mock.join("curator").join("35-curator.json"),
            r#"{"text": "I cannot group these theses.", "finish_reason": "end_turn"}"#,
        )
        .unwrap();
    });
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    assert_eq!(role_call_count(&run.run_dir, "curator"), 4);
    assert_eq!(
        run.read_json("curation/auth__oauth.json")["status"],
        "failed"
    );
    let oauth = std::fs::read_to_string(run.run_dir.join("final/auth/oauth.md")).unwrap();
    assert!(
        oauth.contains("\n⚠ Grouping failed for this cell; its theses are listed without groups.\n\n## All theses\n"),
        "{oauth}"
    );
    for id in ["sk_0000", "sk_0001", "sk_0002"] {
        assert!(oauth.contains(&format!("\n### {id}\n")), "{id}");
    }
    let readme = std::fs::read_to_string(run.run_dir.join("final/README.md")).unwrap();
    assert!(
        readme.contains("- ⚠ Grouping failed in 2 cell(s)"),
        "{readme}"
    );
    let catalog = run.read_json("final/catalog.json");
    assert_eq!(
        catalog["sketches"].as_array().unwrap().len(),
        EXPECTED_SKETCHES
    );
}

/// Deleting one curation file and continuing makes exactly one curator
/// call and no sketch call.
#[test]
fn continuing_after_deleting_a_curation_file_makes_exactly_one_curator_call() {
    let run = run_mock_discover();
    assert_eq!(run.exit_code, Some(0), "stderr:\n{}", run.stderr);
    let sketch_calls_before = sketch_call_count(&run.run_dir);
    let curation = run.run_dir.join("curation").join("auth__api-key.json");
    let before = std::fs::read(&curation).unwrap();
    std::fs::remove_file(&curation).unwrap();

    let output = continue_discover(&run, &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(role_call_count(&run.run_dir, "curator"), 3);
    assert_eq!(sketch_call_count(&run.run_dir), sketch_calls_before);
    assert_eq!(std::fs::read(&curation).unwrap(), before);
}
