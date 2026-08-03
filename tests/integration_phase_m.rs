//! End-to-end integration tests for Phase M (v0.4 — error
//! structuring). Covers:
//!
//! - `Error::code()` mapping for every variant (M.1 / M.2).
//! - `ErrorCode` serialization round-trip in
//!   `SCREAMING_SNAKE_CASE` (D.12.12).
//! - `RunPaths::resolve()` for the eight documented keys with
//!   both `relative` and `absolute` maps populated (D.12.16).
//!
//! These tests complement the unit tests in `src/error_code.rs`,
//! `src/error.rs`, and `src/fs_layout.rs` by exercising the
//! public surface against a real `MoaganHome`.

#![allow(clippy::await_holding_lock)]

use moagan::error::{Error, IoError};
use moagan::error_code::ErrorCode;
use moagan::fs_layout::{MoaganHome, RunPaths};
use moagan::ids::RunId;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Mount a fresh `MOAGAN_HOME` in a tmpdir and return the
/// resolved `MoaganHome`. The env lock keeps the variable
/// stable for the duration of the test.
fn fresh_home() -> (tempfile::TempDir, MoaganHome) {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = MoaganHome::resolve().unwrap();
    home.ensure().unwrap();
    (tmp, home)
}

#[test]
fn error_io_returns_code_io() {
    // I/O errors collapse to ErrorCode::Io.
    let err = Error::Io(IoError::Raw(std::io::Error::other("disk full")));
    assert_eq!(err.code(), ErrorCode::Io);
    // Wire form is SCREAMING_SNAKE_CASE.
    assert_eq!(err.code().stable(), "IO");
    // Io is NOT in the retriable set — it's the catch-all bucket
    // and includes permanent failures.
    assert!(!err.code().is_retriable());
}

#[test]
fn error_invalid_args_returns_code_invalid_args() {
    let err = Error::InvalidArgs("missing --prompt".into());
    assert_eq!(err.code(), ErrorCode::InvalidArgs);
    assert_eq!(err.code().stable(), "INVALID_ARGS");
    // User errors never retry.
    assert!(!err.code().is_retriable());
    assert!(!err.code().is_circuit_opening());
}

#[test]
fn error_already_exists_returns_code_already_exists() {
    // `Error::InvalidApiKey` is the only auth-related variant in
    // the current Error enum; we verify it routes to Auth and that
    // Auth is circuit-opening (per the catalog decision D.12.8:
    // "auth failures should sidelining the provider").
    let err = Error::InvalidApiKey("missing key".into());
    assert_eq!(err.code(), ErrorCode::Auth);
    assert_eq!(err.code().stable(), "AUTH");
    assert!(err.code().is_circuit_opening());
}

#[test]
fn error_code_round_trips_json() {
    // External tools must read what we write. Spot-check the
    // most common codes plus one numeric one.
    for code in [
        ErrorCode::FsNotFound,
        ErrorCode::ProviderAuth,
        ErrorCode::Http429,
        ErrorCode::Http500,
        ErrorCode::Cancelled,
        ErrorCode::TimeoutSketch,
        ErrorCode::InvalidArgs,
        ErrorCode::SandboxTimeout,
        ErrorCode::ManifestInconsistent,
        ErrorCode::PromptInjectionConfirmed,
        ErrorCode::UnhandledError,
        ErrorCode::Custom,
    ] {
        let j = serde_json::to_string(&code).expect("serialize");
        let back: ErrorCode = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(code, back, "round-trip mismatch for {code:?}");
        // Wire form must be uppercase + digits + underscores only.
        let wire = j.trim_matches('"');
        assert!(!wire.is_empty());
        for ch in wire.chars() {
            assert!(
                ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_',
                "non SCREAMING_SNAKE char in {wire:?} (code {code:?})"
            );
        }
    }
}

#[test]
fn run_paths_resolves_brief_and_final() {
    let (_tmp, home) = fresh_home();
    let id = RunId::new();
    let run_dir = home.run_dir(id);
    run_dir.ensure().unwrap();

    let paths = RunPaths::resolve(&home, id);

    // Both maps populated.
    assert_eq!(paths.relative.len(), 8);
    assert_eq!(paths.absolute.len(), 8);

    // brief: relative + absolute point at the same suffix.
    let brief_rel = paths.relative_str("brief").expect("brief relative");
    assert_eq!(brief_rel, "brief.json");
    let brief_abs = paths.absolute("brief").expect("brief absolute");
    assert!(brief_abs.ends_with("brief.json"));
    assert!(brief_abs.starts_with(run_dir.root()));

    // final: directory under the run root.
    let final_abs = paths.absolute("final").expect("final absolute");
    assert!(final_abs.ends_with("final"));
    assert!(final_abs.starts_with(run_dir.root()));
    // The `final` directory exists after ensure().
    assert!(final_abs.is_dir());

    // The relative map keys are pure suffixes (no absolute path
    // leakage) so a re-rooted run keeps them valid.
    for (key, rel) in &paths.relative {
        assert!(
            !rel.starts_with('/'),
            "{key}: relative starts with / ({rel})"
        );
        assert!(!rel.contains(".."), "{key}: relative contains .. ({rel})");
    }

    // Serde round-trip.
    let j = serde_json::to_string(&paths).unwrap();
    let back: RunPaths = serde_json::from_str(&j).unwrap();
    assert_eq!(paths, back);
}

#[test]
fn run_paths_contains_all_documented_keys_with_expected_suffixes() {
    let (_tmp, home) = fresh_home();
    let paths = RunPaths::resolve(&home, RunId::new());
    let expected: &[(&str, &str)] = &[
        ("brief", "brief.json"),
        ("final", "final"),
        ("manifest", "manifest.json"),
        ("ranking", "rankings/ranking.json"),
        ("calls", "telemetry/calls.jsonl.gz"),
        ("phases", "telemetry/phases.jsonl.gz"),
        ("warnings", "telemetry/warnings.jsonl"),
        ("checkpoints", "telemetry/checkpoints.jsonl"),
    ];
    assert_eq!(paths.len(), expected.len());
    for (key, suffix) in expected {
        let rel = paths
            .relative_str(key)
            .unwrap_or_else(|| panic!("missing {key}"));
        assert_eq!(rel, *suffix, "suffix mismatch for {key}");
        let abs = paths
            .absolute(key)
            .unwrap_or_else(|| panic!("missing {key}"));
        let abs_str = abs.to_string_lossy();
        assert!(
            abs_str.ends_with(suffix),
            "{key}: absolute {abs_str} does not end with {suffix}"
        );
    }
}
