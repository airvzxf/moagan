//! Directories nothing writes are not created (#1006): a run dir has
//! no `drafts/` (its writer, the matrix phase, is gone) and the home
//! has no `cache/facets` (the facet cache is gone).

use std::path::{Path, PathBuf};

use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

#[test]
fn a_fresh_home_has_no_facet_cache_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    home.ensure().unwrap();
    assert!(
        home.cross_run_cache_dir().is_dir(),
        "cache/llm is still created"
    );
    assert!(!tmp.path().join("cache/facets").exists());
}

#[test]
fn a_fresh_run_dir_has_no_drafts_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = MoaganHome::at(tmp.path().to_path_buf());
    let run_dir = home.run_dir(RunId::new());
    run_dir.ensure().unwrap();
    assert!(
        run_dir.final_dir().is_dir(),
        "the run layout is still created"
    );
    assert!(!run_dir.root().join("drafts").exists());
}

#[test]
fn a_discover_run_leaves_no_drafts_dir_and_no_facet_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mock_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_provider");
    let out = std::process::Command::new(binary())
        .args([
            "discover",
            "--non-interactive",
            "--provider",
            "mock:mock-model",
            "--mock-dir",
        ])
        .arg(&mock_dir)
        .args([
            "--prompt",
            "Design a parts catalogue",
            "--dimensions",
            "1",
            "--facets-per-dimension",
            "1",
            "--theses-per-cell",
            "1",
        ])
        .current_dir(tmp.path())
        .env("MOAGAN_HOME", &home)
        .env_remove("MOAGAN_CONFIG")
        .env("XDG_CONFIG_HOME", tmp.path().join("xdg"))
        .env("MOAGAN_QUIET", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "discover failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs: Vec<PathBuf> = std::fs::read_dir(home.join(".runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(runs.len(), 1, "one run dir: {runs:?}");
    assert!(runs[0].join("final/README.md").is_file());
    assert!(!runs[0].join("drafts").exists());
    assert!(!home.join("cache/facets").exists());
}
