//! Integration test: D.17.8 — dashboard HTML per-run.
//!
//! Verifies the PR-02 wiring: `write_dashboard(run_dir)` is invoked
//! from the deliver phase at the end of every pipeline run so each
//! run produces `run_dir/dashboard.html`. The HTML is the static
//! `DASHBOARD_HTML` constant shipped from `src/telemetry/dashboard_static.rs`
//! (it mounts an empty `#runs` div that the JS handler fills by
//! fetching `/api/runs`).
//!
//! The smoke probe is a single `moagan run --mode fast --provider mock`
//! invocation rooted at a tmpdir so the cross-run LLM cache is
//! guaranteed cold. The test asserts:
//!
//! 1. The CLI exits successfully (`<runs-dir>/.runs/<id>/` exists).
//! 2. `<runs-dir>/.runs/<id>/dashboard.html` exists.
//! 3. The on-disk file matches the `DASHBOARD_HTML` constant byte-for-byte.
//! 4. The HTML contains the `moagan dashboard` sentinel so a regression
//!    that drops the file in favour of an empty placeholder still trips
//!    the assertion.
//!
//! Note: under a non-TTY stdout the `moagan run <id> …` banner and
//! the `run id: <uuid>` footer are deliberately suppressed so stdout
//! stays pure NDJSON for `moagan … | jq` consumers. The run id is
//! therefore resolved by enumerating `<runs-dir>/.runs/` instead of
//! parsing stdout.

use std::process::Command;

#[test]
fn mock_run_writes_dashboard_html() {
    let bin = std::env::var("CARGO_BIN_EXE_moagan")
        .expect("CARGO_BIN_EXE_moagan — run via `cargo test`, not directly");

    let tmp = tempfile::TempDir::new().expect("tmpdir");

    // v0.10 dispatcher requires the `--provider SECTION` shorthand
    // to resolve to a configured model id; `Config::default()` ships
    // `[[providers.mock]]` with an empty `models[]` list. Drop a
    // one-line config onto disk that registers `mock-model` under
    // the `mock` section so the dispatcher finds it. The
    // `MOAGAN_CONFIG` env var overrides the user-level config
    // lookup (`src/config/mod.rs:2361`).
    let mock_cfg_dir = tempfile::TempDir::new().expect("mock cfg tmpdir");
    let mock_cfg_path = mock_cfg_dir.path().join("moagan-test-mock.toml");
    std::fs::write(
        &mock_cfg_path,
        "[[providers.mock]]\n\
         endpoint = \"mock://local\"\n\
         models = [\"mock-model\"]\n",
    )
    .expect("write mock config");

    let out = Command::new(&bin)
        .arg("run")
        .arg("--mode")
        .arg("fast")
        .arg("--provider")
        .arg("mock:mock-model")
        .arg("--mock-dir")
        .arg("tests/fixtures/mock_provider")
        .arg("--prompt")
        .arg("D.17.8 dashboard HTML per-run probe")
        .arg("--non-interactive")
        .arg("--runs-dir")
        .arg(tmp.path())
        .env_remove("MINIMAX_API_KEY")
        .env("MOAGAN_CONFIG", &mock_cfg_path)
        .output()
        .expect("moagan run");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "mock run failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status
    );

    // Resolve the run id from disk instead of from stdout: under
    // a non-TTY stdout (which `Command::output()` captures), the
    // dispatcher suppresses both the `moagan run <id> …` banner
    // and the `run id: <uuid>` footer so stdout stays pure NDJSON
    // for `moagan … | jq`. The only stateful on-disk artefact of
    // the run is `<runs-dir>/.runs/<uuid>/manifest.json`, so we
    // enumerate the `.runs/` subdir and pick the one entry.
    let runs_root = tmp.path().join(".runs");
    let mut entries: Vec<_> = std::fs::read_dir(&runs_root)
        .unwrap_or_else(|e| panic!("read_dir({}): {e}", runs_root.display()))
        .filter_map(|res| res.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one run under {runs_root:?}, found {} (stdout={stdout}\nstderr={stderr})",
        entries.len()
    );
    let run_id = entries
        .pop()
        .expect("checked above")
        .file_name()
        .into_string()
        .expect("run id is utf-8");
    assert_eq!(run_id.len(), 36, "expected hyphenated uuid, got {run_id}");

    let dashboard_path = tmp
        .path()
        .join(".runs")
        .join(&run_id)
        .join("dashboard.html");
    assert!(
        dashboard_path.exists(),
        "dashboard.html was not written at {}",
        dashboard_path.display()
    );

    let written = std::fs::read_to_string(&dashboard_path).expect("read dashboard.html");

    let expected = moagan::telemetry::dashboard_static::DASHBOARD_HTML;
    assert_eq!(
        written, expected,
        "dashboard.html on disk must match the bundled DASHBOARD_HTML constant"
    );

    let sentinel = "moagan dashboard";
    assert!(
        written.contains(sentinel),
        "dashboard.html must contain the `{sentinel}` sentinel; got:\n{written}"
    );
}
