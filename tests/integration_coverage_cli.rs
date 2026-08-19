//! Integration test for the `moagan coverage` subcommand. ADR-0002.
//!
//! Runs the real CLI binary through a fresh `MoaganHome` and
//! asserts the text view prints the expected "not instrumented"
//! hint when no `profraw` files are on disk. The HTML view is not
//! tested here because it requires `grcov` on PATH; the unit
//! tests in `src/coverage/inspect.rs` cover the `grcov` fallback
//! path.

use std::process::Command;

use moagan::ids::RunId;
use moagan::test_support::with_moagan_home;

fn moagan_bin() -> std::path::PathBuf {
    // `cargo test` runs from the crate root and Cargo puts the
    // freshly built `moagan` binary next to the integration-test
    // executables. Falling back to `target/debug/moagan` keeps the
    // test runnable in plain `cargo test --test
    // integration_coverage_cli` invocations.
    std::env::var("CARGO_BIN_EXE_moagan")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("moagan")
        })
}

#[test]
fn coverage_show_text_prints_not_instrumented_hint() {
    with_moagan_home("coverage_cli_text_hint", |_home_path| {
        // Use a fresh run id; the run dir does not exist yet
        // (the operator can ask about a run that never executed).
        let run_id = RunId::new();
        let output = Command::new(moagan_bin())
            .env("MOAGAN_HOME", _home_path)
            .arg("coverage")
            .arg("show")
            .arg(run_id.to_string())
            .arg("--format")
            .arg("text")
            .output()
            .expect("spawn moagan coverage");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "moagan coverage show --format text must exit 0; \
             stdout=\n{stdout}\nstderr=\n{stderr}"
        );
        assert!(
            stdout.contains("not instrumented"),
            "stdout must explain the no-coverage state; got:\n{stdout}"
        );
        assert!(
            stdout.contains("RUSTFLAGS"),
            "stdout must point the operator at the build flags \
             to enable coverage; got:\n{stdout}"
        );
    });
}

#[test]
fn coverage_show_text_lists_profraw_files() {
    with_moagan_home("coverage_cli_text_files", |home_path| {
        let home = moagan::fs_layout::MoaganHome::at(home_path.to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        // Drop two fake `profraw` files in the coverage dir.
        std::fs::write(
            run_dir
                .coverage()
                .join(format!("{run_id}-phase-1-0.profraw")),
            b"a",
        )
        .unwrap();
        std::fs::write(
            run_dir
                .coverage()
                .join(format!("{run_id}-call-c1-1.profraw")),
            b"bb",
        )
        .unwrap();
        let output = Command::new(moagan_bin())
            .env("MOAGAN_HOME", home_path)
            .arg("coverage")
            .arg("show")
            .arg(run_id.to_string())
            .arg("--format")
            .arg("text")
            .output()
            .expect("spawn moagan coverage");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "moagan coverage show must exit 0; stdout=\n{stdout}"
        );
        assert!(
            stdout.contains("instrumented"),
            "stdout must report the run as instrumented; got:\n{stdout}"
        );
        assert!(
            stdout.contains("phase-1-0.profraw"),
            "stdout must list the phase snapshot; got:\n{stdout}"
        );
        assert!(
            stdout.contains("call-c1-1.profraw"),
            "stdout must list the call snapshot; got:\n{stdout}"
        );
    });
}

#[test]
fn coverage_show_html_without_grcov_errors_cleanly() {
    with_moagan_home("coverage_cli_html_no_grcov", |home_path| {
        // Strip PATH to make sure `grcov` is not found.
        let empty_path = std::path::PathBuf::from("/usr/bin");
        let home = moagan::fs_layout::MoaganHome::at(home_path.to_path_buf());
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        std::fs::write(run_dir.coverage().join(format!("{run_id}.profraw")), b"x").unwrap();
        let output = Command::new(moagan_bin())
            .env("MOAGAN_HOME", home_path)
            .env("PATH", &empty_path)
            .arg("coverage")
            .arg("show")
            .arg(run_id.to_string())
            .arg("--format")
            .arg("html")
            .output()
            .expect("spawn moagan coverage");
        assert!(
            !output.status.success(),
            "moagan coverage show --format html must fail when \
             grcov is not on PATH; stdout=\n{} stderr=\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("grcov"),
            "stderr must mention grcov to help the operator; got:\n{stderr}"
        );
    });
}
