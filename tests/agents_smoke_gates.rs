//! The probe smoke gates of AGENTS.md run as written (#1025).
//!
//! Gates 3 and 4 are `moagan probe … --dry-run` commands. Each one is
//! read from AGENTS.md, run against the built binary with an empty
//! home, and must exit 0 and persist nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

/// The backticked `moagan probe …` command of each numbered smoke gate.
fn probe_gates() -> Vec<String> {
    let agents = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("AGENTS.md"))
        .expect("read AGENTS.md");
    agents
        .lines()
        .filter(|l| l.starts_with("3. `moagan probe ") || l.starts_with("4. `moagan probe "))
        .filter_map(|l| l.split('`').nth(1).map(str::to_owned))
        .collect()
}

#[test]
fn both_probe_smoke_gates_name_real_subcommands_and_pass_as_written() {
    let gates = probe_gates();
    assert_eq!(gates.len(), 2, "smoke gates 3 and 4: {gates:?}");
    for gate in gates {
        let home = tempfile::tempdir().expect("home");
        let words: Vec<&str> = gate.split_whitespace().collect();
        assert_eq!(words.first(), Some(&"moagan"), "{gate}");
        let out = Command::new(binary())
            .args(&words[1..])
            .env("MOAGAN_HOME", home.path())
            .env("MINIMAX_API_KEY", "test-key")
            .env("XDG_CONFIG_HOME", home.path().join("xdg"))
            .env_remove("MOAGAN_CONFIG")
            .output()
            .expect("spawn moagan");
        assert!(
            out.status.success(),
            "`{gate}` failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let persisted: Vec<PathBuf> = std::fs::read_dir(home.path())
            .expect("read home")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        assert!(persisted.is_empty(), "`{gate}` persisted {persisted:?}");
    }
}

#[test]
fn the_probe_help_names_the_subcommands_as_typed() {
    for sub in [
        &[][..],
        &["max-tokens"],
        &["top-p"],
        &["top-k"],
        &["temperature"],
    ] {
        let out = Command::new(binary())
            .arg("probe")
            .args(sub)
            .arg("--help")
            .output()
            .expect("spawn moagan");
        let help = String::from_utf8_lossy(&out.stdout);
        for wrong in [
            "probe max_tokens",
            "probe top_p",
            "probe top_k",
            "`max_tokens` and",
        ] {
            assert!(
                !help.contains(wrong),
                "`moagan probe {sub:?} --help` says {wrong:?}:\n{help}"
            );
        }
    }
}
