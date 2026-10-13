//! `moagan discover merge` pools finished runs into one catalogue (#1009).
//!
//! Two mock discover runs share one runs dir; the merge writes a third
//! run whose sketches are every source sketch renumbered, each naming
//! the run and id it came from, curated once per cell.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PROMPT: &str = "Enumera los 7 colores del arcoiris en orden";

fn moagan_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

fn mock_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider")
}

struct Runs {
    work: tempfile::TempDir,
}

impl Runs {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("home")).unwrap();
        std::fs::create_dir_all(work.path().join("runs")).unwrap();
        Self { work }
    }

    fn runs(&self) -> PathBuf {
        self.work.path().join("runs")
    }

    fn moagan(&self) -> Command {
        let mut cmd = Command::new(moagan_bin());
        cmd.env("MOAGAN_HOME", self.work.path().join("home"))
            .env_remove("MOAGAN_QUIET")
            .env_remove("MOAGAN_DECISION_FORMAT")
            .args(["--log-format", "json", "--event-format", "jsonl"]);
        cmd
    }

    fn run_ids(&self) -> BTreeSet<String> {
        match std::fs::read_dir(self.runs().join(".runs")) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => BTreeSet::new(),
        }
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.runs().join(".runs").join(id)
    }

    /// One mock discover run; returns its run id.
    fn discover(&self) -> String {
        let before = self.run_ids();
        let out = self
            .moagan()
            .args(["discover", "--non-interactive", "--prompt", PROMPT])
            .args(["--provider", "mock:mock-model", "--mock-dir"])
            .arg(mock_dir())
            .args(["--matrix-spec", "auth=oauth,api-key"])
            .args(["--sketches-per-cell", "3", "--runs-dir"])
            .arg(self.runs())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.new_run(&before)
    }

    fn merge(&self, ids: &[&str]) -> Output {
        self.moagan()
            .args(["discover", "merge"])
            .args(ids)
            .arg("--runs-dir")
            .arg(self.runs())
            .output()
            .unwrap()
    }

    fn new_run(&self, before: &BTreeSet<String>) -> String {
        let mut added: Vec<String> = self.run_ids().difference(before).cloned().collect();
        assert_eq!(added.len(), 1, "expected exactly one new run dir");
        added.remove(0)
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

/// Primary sketch files of a run (`.meta.json` seals excluded), sorted.
fn sketch_files(run_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(run_dir.join("sketches"))
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

fn role_calls(run_dir: &Path, role: &str) -> usize {
    let bytes = std::fs::read(run_dir.join("telemetry").join("calls.jsonl.gz")).unwrap_or_default();
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|row| row["role"] == role)
        .count()
}

/// Every primary file under `dir`, keyed by its relative path.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if !path.to_string_lossy().ends_with(".meta.json") {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn merging_two_runs_pools_every_sketch_and_curates_each_cell_once() {
    let runs = Runs::new();
    let a = runs.discover();
    let b = runs.discover();
    let before = runs.run_ids();
    let out = runs.merge(&[&a, &b]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let merged = runs.run_dir(&runs.new_run(&before));

    let sources: Vec<(String, Vec<PathBuf>)> = [&a, &b]
        .iter()
        .map(|id| ((*id).clone(), sketch_files(&runs.run_dir(id))))
        .collect();
    let total: usize = sources.iter().map(|(_, files)| files.len()).sum();
    assert_eq!(total, 12, "two runs of 2 cells x 3 sketches");
    let pooled = sketch_files(&merged);
    assert_eq!(pooled.len(), total);

    let mut n = 0;
    for (run_id, files) in &sources {
        for file in files {
            let original = read_json(file);
            let copy = read_json(&pooled[n]);
            assert_eq!(copy["id"], format!("sk_{n:04}"));
            assert_eq!(copy["thesis"], original["thesis"]);
            assert_eq!(copy["angle"], original["angle"]);
            assert_eq!(copy["provenance"]["source_run"], run_id.as_str());
            assert_eq!(copy["provenance"]["source_sketch"], original["id"]);
            n += 1;
        }
    }

    let spec = read_json(&merged.join("discover_run.json"));
    assert_eq!(spec["merged_from"], serde_json::json!([a, b]));
    assert_eq!(
        read_json(&merged.join("brief.json")),
        read_json(&runs.run_dir(&a).join("brief.json"))
    );

    assert_eq!(role_calls(&merged, "curator"), 2, "one curation per cell");
    assert_eq!(role_calls(&merged, "sketch"), 0);
    assert_eq!(role_calls(&merged, "intake"), 0);
    for cell in ["auth__oauth.json", "auth__api-key.json"] {
        let curation = read_json(&merged.join("curation").join(cell));
        assert_eq!(curation["members"].as_array().unwrap().len(), 6, "{cell}");
    }

    let catalog = read_json(&merged.join("final").join("catalog.json"));
    let ids: BTreeSet<&str> = catalog["sketches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), total, "every pooled sketch listed once");
    let facet_text =
        std::fs::read_to_string(merged.join("final").join("auth").join("oauth.md")).unwrap();
    assert!(
        facet_text.contains(&format!(" · from {a} sk_0000\n")),
        "{facet_text}"
    );
}

#[test]
fn continuing_a_merged_run_makes_no_call_and_rerenders_the_same_catalogue() {
    let runs = Runs::new();
    let a = runs.discover();
    let b = runs.discover();
    let before = runs.run_ids();
    assert!(runs.merge(&[&a, &b]).status.success());
    let id = runs.new_run(&before);
    let merged = runs.run_dir(&id);
    let curator_calls = role_calls(&merged, "curator");
    let catalogue = snapshot(&merged.join("final"));

    let out = runs
        .moagan()
        .env("MOAGAN_HOME", runs.runs())
        .args([
            "continue",
            "--kind",
            "discovery",
            "--run-id",
            &id,
            "--non-interactive",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(role_calls(&merged, "curator"), curator_calls);
    assert_eq!(role_calls(&merged, "sketch"), 0);
    assert_eq!(snapshot(&merged.join("final")), catalogue);
}

#[test]
fn runs_with_different_brief_constraints_are_not_merged() {
    let runs = Runs::new();
    let a = runs.discover();
    let b = runs.discover();
    let brief_path = runs.run_dir(&b).join("brief.json");
    let mut brief = read_json(&brief_path);
    brief["constraints"] = serde_json::json!(["Only six colors"]);
    std::fs::write(&brief_path, brief.to_string()).unwrap();

    let before = runs.run_ids();
    let out = runs.merge(&[&a, &b]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "run {b} does not share the brief constraints of run {a}"
        )),
        "{stderr}"
    );
    assert_eq!(runs.run_ids(), before, "no merged run dir is left behind");
}

#[test]
fn a_run_listed_twice_is_refused() {
    let runs = Runs::new();
    let a = runs.discover();
    let out = runs.merge(&[&a, &a]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!("run {a} is listed twice")),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
