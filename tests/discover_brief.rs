//! `moagan discover --brief <path>` skips intake and reuses the given
//! brief, so runs started with the same brief share their constraint
//! ids (#1001). Drives the real binary against the shared mock fixtures.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PROMPT: &str = "Design the spare-parts price list.";

/// A brief from an earlier run: three constraints, so `C1..C3`.
const BRIEF: &str = r#"{
  "problem": "Design the spare-parts price list",
  "objectives": ["One list price per part"],
  "constraints": ["Fixed budget", "No customer credit", "Prices in one currency"],
  "non_goals": ["Discount campaigns"],
  "open_questions": [],
  "raw_prompt": "Design the spare-parts price list."
}"#;

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

fn mock_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mock_provider")
}

/// One mock discover run under `work` with `--brief brief`.
fn discover_with_brief(work: &Path, brief: &Path) -> Output {
    let home = work.join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(moagan_bin())
        .env("MOAGAN_HOME", &home)
        .env_remove("MOAGAN_QUIET")
        .env_remove("MOAGAN_DECISION_FORMAT")
        .args(["discover", "--non-interactive", "--prompt", PROMPT])
        .args(["--provider", "mock:mock-model", "--mock-dir"])
        .arg(mock_dir())
        .args([
            "--matrix-spec",
            "pricing=list,margin",
            "--theses-per-cell",
            "1",
        ])
        .arg("--runs-dir")
        .arg(work.join("runs"))
        .arg("--brief")
        .arg(brief)
        .args(["--log-format", "json", "--event-format", "jsonl"])
        .output()
        .unwrap()
}

fn run_dirs(work: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(work.join("runs").join(".runs")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
    dirs.sort();
    dirs
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

#[test]
fn two_runs_with_the_same_brief_have_identical_briefs_and_constraint_ids() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.json");
    std::fs::write(&brief, BRIEF).unwrap();
    for _ in 0..2 {
        let out = discover_with_brief(work.path(), &brief);
        assert!(
            out.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let dirs = run_dirs(work.path());
    assert_eq!(dirs.len(), 2, "expected two run dirs");
    for dir in &dirs {
        let copied = std::fs::read_to_string(dir.join("brief.json")).unwrap();
        assert_eq!(
            copied,
            BRIEF,
            "brief.json of {} is the given brief",
            dir.display()
        );
        assert!(dir.join("final").join("catalog.json").is_file());
    }
}

#[test]
fn a_run_with_a_brief_makes_no_intake_call() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.json");
    std::fs::write(&brief, BRIEF).unwrap();
    let out = discover_with_brief(work.path(), &brief);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dirs = run_dirs(work.path());
    assert_eq!(role_calls(&dirs[0], "intake"), 0);
    assert_eq!(role_calls(&dirs[0], "thesis"), 2);
    assert_eq!(
        std::fs::read_to_string(dirs[0].join("prompt.md")).unwrap(),
        PROMPT
    );
}

#[test]
fn a_brief_without_a_problem_is_rejected_before_any_run_dir_exists() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.json");
    std::fs::write(
        &brief,
        r#"{"problem": "  ", "constraints": ["Fixed budget"]}"#,
    )
    .unwrap();
    let out = discover_with_brief(work.path(), &brief);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--brief"), "stderr:\n{stderr}");
    assert!(run_dirs(work.path()).is_empty(), "no run dir is created");
}

#[test]
fn a_missing_brief_file_is_rejected() {
    let work = tempfile::tempdir().unwrap();
    let out = discover_with_brief(work.path(), &work.path().join("missing.json"));
    assert!(!out.status.success());
    assert!(run_dirs(work.path()).is_empty(), "no run dir is created");
}
