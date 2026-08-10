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
    assert!(stderr(&output).contains("[moagan] loaded .env from"));
}

#[test]
fn doctor_loads_api_key_from_dotenv() {
    let tmp = tempfile::tempdir().unwrap();
    // PR-B2: the doctor now checks every keyed provider kind
    // (minimax / deepseek / opencode_go). Pre-PR-B2 only
    // MINIMAX_API_KEY was checked; to keep this test passing we
    // supply the other two via direct env (the dotenv test only
    // exercises MINIMAX).
    fs::write(
        tmp.path().join(".env"),
        "MINIMAX_API_KEY=from-dotenv\n\
         DEEPSEEK_API_KEY=from-dotenv\n\
         OPENCODE_GO_API_KEY=from-dotenv\n",
    )
    .unwrap();

    let output = run_in(tmp.path(), &["doctor"])
        .env_remove("MINIMAX_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENCODE_GO_API_KEY")
        .env_remove("MOAGAN_QUIET")
        .env("MOAGAN_HOME", tmp.path().join("home"))
        .env("MOAGAN_CONFIG", tmp.path().join("missing.toml"))
        .output()
        .unwrap();
    let stdout = stdout(&output);

    assert!(output.status.success(), "{stdout}\n{}", stderr(&output));
    assert!(stdout.contains("[OK] api_key"), "{stdout}");
    assert!(stdout.contains("doctor: OK"), "{stdout}");
    assert!(stderr(&output).contains("[moagan] loaded .env from"));
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
             OPENCODE_GO_API_KEY=from-dotenv-BBB\n\
             MOAGAN_HOME={}\n",
            dotenv_home.display()
        ),
    )
    .unwrap();

    let output = run_in(tmp.path(), &["doctor"])
        .env("MINIMAX_API_KEY", "from-shell-AAA")
        .env("DEEPSEEK_API_KEY", "from-shell-AAA")
        .env("OPENCODE_GO_API_KEY", "from-shell-AAA")
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
