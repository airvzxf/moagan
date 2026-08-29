use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

fn run_in(directory: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(binary());
    command.current_dir(directory).args(arguments);
    command
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn version_succeeds_with_dotenv_in_current_directory() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join(".env"), "MINIMAX_API_KEY=fake\n").unwrap();

    let output = run_in(tmp.path(), &["--version"])
        .env_remove("MINIMAX_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
    // c1 migration (Riesgos #6): clap processes `--version`
    // BEFORE `init_tracing()` runs and calls
    // `std::process::exit(0)` from the parser. As a result, the
    // new `moagan::boot` tracing event is NOT emitted on the
    // `--version` fast path — clap exits before any subscriber
    // is installed. The pre-c1 plain-text line was similarly
    // missing on `--version` because the `eprintln!` lived in
    // the same `main()` body that clap short-circuits. The
    // contract under c1 is: no plain-text line, no JSONL boot
    // event, just the version on stdout.
    let stderr_text = stderr(&output);
    assert!(
        !stderr_text.contains("[moagan] loaded .env from"),
        "legacy plain-text '[moagan] loaded .env from …' must NOT appear on stderr; stderr:\n{stderr_text}"
    );
}

#[test]
fn doctor_loads_api_key_from_dotenv() {
    let tmp = tempfile::tempdir().unwrap();
    // PR-B2: the doctor now checks every keyed provider kind
    // (minimax / deepseek / opencode). Pre-PR-B2 only
    // MINIMAX_API_KEY was checked; to keep this test passing we
    // supply the other two via direct env (the dotenv test only
    // exercises MINIMAX).
    fs::write(
        tmp.path().join(".env"),
        "MINIMAX_API_KEY=from-dotenv\n\
         DEEPSEEK_API_KEY=from-dotenv\n\
         OPENCODE_API_KEY=from-dotenv\n",
    )
    .unwrap();

    let output = run_in(tmp.path(), &["doctor", "--log-format", "json"])
        .env_remove("MINIMAX_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENCODE_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .env("MOAGAN_HOME", tmp.path().join("home"))
        .env("MOAGAN_CONFIG", tmp.path().join("missing.toml"))
        .output()
        .unwrap();
    let stdout = stdout(&output);

    assert!(output.status.success(), "{stdout}\n{}", stderr(&output));
    assert!(stdout.contains("[OK] api_key"), "{stdout}");
    assert!(stdout.contains("doctor: OK"), "{stdout}");
    // c1 migration (Riesgos #6): the operator-facing notice
    // is now emitted via `tracing::info!` with target
    // `moagan::boot` instead of an `eprintln!` that leaked
    // into NDJSON purity on stderr. PR-04a (E-1) then routed
    // the INFO-level event to **stdout** (the v0.12.0
    // stream-routing-flip default), so the JSONL boot event
    // pins to stdout rather than stderr — the contract still
    // holds (a tracing event, not a plain-text `eprintln!`).
    let stderr_text = stderr(&output);
    assert!(
        !stderr_text.contains("[moagan] loaded .env from"),
        "legacy plain-text '[moagan] loaded .env from …' must NOT appear; stderr:\n{stderr_text}"
    );
    assert!(
        stdout.contains("\"target\":\"moagan::boot\""),
        "expected JSONL boot event on stdout under PR-04a routing; stdout:\n{stdout}"
    );
    assert!(
        !stderr_text.contains("\"target\":\"moagan::boot\""),
        "boot event must NOT be on stderr under PR-04a routing (stderr is for ERROR-level only); stderr:\n{stderr_text}"
    );
}

#[test]
fn dotenv_does_not_override_existing_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let dotenv_home = tmp.path().join("from-dotenv");
    let shell_home = tmp.path().join("from-shell");
    // PR-B2: dotenv must set every keyed provider kind so the
    // doctor check has every key to resolve.
    fs::write(
        tmp.path().join(".env"),
        format!(
            "MINIMAX_API_KEY=from-dotenv-BBB\n\
             DEEPSEEK_API_KEY=from-dotenv-BBB\n\
             OPENCODE_API_KEY=from-dotenv-BBB\n\
             MOAGAN_HOME={}\n",
            dotenv_home.display()
        ),
    )
    .unwrap();

    let output = run_in(tmp.path(), &["doctor"])
        .env("MINIMAX_API_KEY", "from-shell-AAA")
        .env("DEEPSEEK_API_KEY", "from-shell-AAA")
        .env("OPENCODE_API_KEY", "from-shell-AAA")
        .env("MOAGAN_HOME", &shell_home)
        .env("MOAGAN_CONFIG", tmp.path().join("missing.toml"))
        .env("MOAGAN_QUIET", "1")
        .output()
        .unwrap();
    let stdout = stdout(&output);
    let stderr = stderr(&output);

    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("doctor: OK"), "{stdout}");
    assert!(
        stdout.contains(&shell_home.display().to_string()),
        "{stdout}"
    );
    assert!(
        !stdout.contains(&dotenv_home.display().to_string()),
        "{stdout}"
    );
    assert!(!stderr.contains("[moagan] loaded .env from"), "{stderr}");
}
