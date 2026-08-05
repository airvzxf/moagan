//! Test-only utilities for serialising tests that mutate `MOAGAN_HOME`.
//!
//! The CLI dispatcher reads `MOAGAN_HOME` from the environment to
//! locate the meta-database (`<MOAGAN_HOME>/meta.sqlite`) and the
//! run directory (`<MOAGAN_HOME>/.runs/`). Two integration tests
//! that both `std::env::set_var("MOAGAN_HOME", ...)` can interleave
//! under the OS scheduler, ending up reading each other's home
//! directory and tripping the non-idempotent v007 / v009
//! migrations with a "duplicate column name" panic.
//!
//! [`with_moagan_home`] is a process-wide Mutex-guarded helper
//! that:
//!
//! 1. Acquires [`ENV_LOCK`] so no other test in this process can
//!    read or mutate `MOAGAN_HOME` while the closure runs.
//! 2. Creates a fresh tempdir keyed on `label`, the current PID,
//!    and the current nanosecond timestamp, so two concurrent
//!    calls get distinct paths even under `cargo test` parallelism.
//! 3. Sets `MOAGAN_HOME` to that tempdir for the duration of the
//!    closure (saving the previous value).
//! 4. Restores the previous `MOAGAN_HOME` value (or removes the
//!    var if there was none) on the way out.
//! 5. Removes the tempdir best-effort so the OS does not slowly
//!    fill up with `/tmp/moagan-*` directories between runs.
//!
//! The closure receives the tempdir path as `&Path`. Tests that
//! need to hand the path to a child process should pass it via
//! `--runs-dir` or `.env("MOAGAN_HOME", ...)` on the
//! `Command` builder as they did before — this helper only
//! governs the parent-process env var.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Process-wide mutex serialising tests that mutate `MOAGAN_HOME`.
/// Held for the duration of [`with_moagan_home`].
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `MOAGAN_HOME` set to a unique tempdir.
///
/// Acquires [`ENV_LOCK`], creates a fresh tempdir named
/// `moagan-<pid>-<nanos>-<label>` under [`std::env::temp_dir`],
/// points `MOAGAN_HOME` at it, runs `f` with the path, then
/// restores the previous `MOAGAN_HOME` value and removes the
/// tempdir.
///
/// The closure is sync; async tests can drive it with
/// `tokio::runtime::Builder::new_current_thread()` (matching the
/// pattern already used in `tests/integration_validators.rs`).
pub fn with_moagan_home<F, R>(label: &str, f: F) -> R
where
    F: FnOnce(&Path) -> R,
{
    let _guard = ENV_LOCK.lock().expect("env lock poisoned");
    let dir = unique_tempdir(label);
    let prev = std::env::var_os("MOAGAN_HOME");
    // SAFETY: serialised by ENV_LOCK; no other thread reads or
    // mutates MOAGAN_HOME while the guard is held.
    unsafe {
        std::env::set_var("MOAGAN_HOME", &dir);
    }
    let result = f(&dir);
    // SAFETY: serialised by ENV_LOCK; restore the previous value
    // (or remove the var if there was none) so the next test does
    // not inherit this tempdir.
    match prev {
        Some(value) => unsafe {
            std::env::set_var("MOAGAN_HOME", value);
        },
        None => unsafe {
            std::env::remove_var("MOAGAN_HOME");
        },
    }
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Build a unique tempdir path under [`std::env::temp_dir`] and
/// create it. Used by [`with_moagan_home`] and exposed for tests
/// that want the unique-path generator without the env-var
/// ceremony.
pub fn unique_tempdir(label: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("moagan-{pid}-{nanos}-{label}"));
    std::fs::create_dir_all(&dir).expect("create unique tempdir");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_moagan_home_sets_and_restores_env() {
        // Sanity check: starting from "unset", the helper sets
        // MOAGAN_HOME during the closure and removes it on the
        // way out.
        unsafe {
            std::env::remove_var("MOAGAN_HOME");
        }
        let observed_during_call = std::sync::Mutex::new(None::<PathBuf>);
        with_moagan_home("sets_restores", |home| {
            let current = std::env::var_os("MOAGAN_HOME")
                .expect("MOAGAN_HOME must be set inside the closure");
            assert_eq!(current.to_str().unwrap(), home.to_str().unwrap());
            *observed_during_call.lock().unwrap() = Some(home.to_path_buf());
        });
        assert!(
            observed_during_call.lock().unwrap().is_some(),
            "closure must have run"
        );
        assert!(
            std::env::var_os("MOAGAN_HOME").is_none(),
            "MOAGAN_HOME must be removed on exit when unset on entry"
        );

        // And when MOAGAN_HOME was set going in, the helper
        // points the env var at the fresh tempdir for the
        // duration of the closure, then restores the previous
        // value.
        let previous = std::env::temp_dir().join("moagan-pre-existing-home");
        std::fs::create_dir_all(&previous).unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", &previous);
        }
        let new_dir = with_moagan_home("sets_restores_2", |home| {
            let current = std::env::var_os("MOAGAN_HOME").unwrap();
            assert_eq!(current.to_str().unwrap(), home.to_str().unwrap());
            home.to_path_buf()
        });
        assert_eq!(
            new_dir.to_str().unwrap(),
            new_dir.to_str().unwrap(),
            "inside the closure MOAGAN_HOME must point at the fresh tempdir"
        );
        assert_ne!(
            new_dir, previous,
            "fresh tempdir must differ from the pre-existing value"
        );
        let after = std::env::var_os("MOAGAN_HOME").unwrap();
        assert_eq!(
            after.to_str().unwrap(),
            previous.to_str().unwrap(),
            "MOAGAN_HOME must be restored to its previous value on exit"
        );
        std::fs::remove_dir_all(&previous).unwrap();
    }

    #[test]
    fn with_moagan_home_creates_unique_dir_per_call() {
        // Two consecutive calls (both holding the lock) must get
        // different tempdir paths even if their nanosecond
        // timestamp collides — the directory creation fails if
        // the path already exists, so a duplicate would manifest
        // as a panic here.
        let a = with_moagan_home("unique_a", |home| home.to_path_buf());
        let b = with_moagan_home("unique_b", |home| home.to_path_buf());
        assert_ne!(a, b, "two calls must yield distinct tempdir paths");
    }

    #[test]
    fn with_moagan_home_cleans_up_dir() {
        let path = with_moagan_home("clean_up", |home| home.to_path_buf());
        assert!(
            !path.exists(),
            "tempdir must be removed after the closure returns, found {}",
            path.display()
        );
    }
}
