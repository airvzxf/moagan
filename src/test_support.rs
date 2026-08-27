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
//! 2. Creates a fresh `tempfile::TempDir` whose OS-level cleanup
//!    runs in `Drop`. The panic path also cleans up — no leaked
//!    `/tmp/moagan-*` directories between runs.
//! 3. Sets `MOAGAN_HOME` to that tempdir for the duration of the
//!    closure (saving the previous value).
//! 4. Restores the previous `MOAGAN_HOME` value (or removes the
//!    var if there was none) on the way out via a `Drop` guard
//!    so the panic path also restores the env.
//!
//! Drop order on return (normal or panic) is the reverse of
//! declaration: env-var restore runs before tempdir removal,
//! before lock release. All three cleanup steps run regardless of
//! whether `f` returns or panics.
//!
//! The closure receives the tempdir path as `&Path`. Tests that
//! need to hand the path to a child process should pass it via
//! `--runs-dir` or `.env("MOAGAN_HOME", ...)` on the
//! `Command` builder as they did before — this helper only
//! governs the parent-process env var.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

/// Process-wide mutex serialising tests that mutate `MOAGAN_HOME`.
/// Held for the duration of [`with_moagan_home`].
///
/// Uses `parking_lot::ReentrantMutex` (not `std::sync::Mutex`)
/// because the self-test in this module
/// (`with_moagan_home_sets_and_restores_env`) holds the lock for
/// its own `unsafe { std::env::* }` mutations and then calls
/// `with_moagan_home` (which itself locks). `std::sync::Mutex`
/// would deadlock; `parking_lot::ReentrantMutex` increments the
/// lock count on the same thread and unlocks at the matching
/// drop. Sibling tests that do not hold the lock continue to be
/// serialised correctly because the lock is observed across
/// threads.
pub static ENV_LOCK: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());

/// Run `f` with `MOAGAN_HOME` set to a unique tempdir.
///
/// Acquires [`ENV_LOCK`], creates a fresh `tempfile::TempDir`
/// (auto-removed on `Drop`, including the panic path), points
/// `MOAGAN_HOME` at it, runs `f` with the path, then restores the
/// previous `MOAGAN_HOME` value via a `Drop` guard (also covers
/// the panic path).
///
/// The closure is sync; async tests can drive it with
/// `tokio::runtime::Builder::new_current_thread()` (matching the
/// pattern already used in `tests/integration_validators.rs`).
pub fn with_moagan_home<F, R>(label: &str, f: F) -> R
where
    F: FnOnce(&Path) -> R,
{
    let _guard = ENV_LOCK.lock();
    let tmp = tempfile::Builder::new()
        .prefix(&format!("moagan-{label}-"))
        .tempdir()
        .expect("create unique tempdir");
    let dir = tmp.path().to_path_buf();

    // Restore the previous MOAGAN_HOME value (or unset it) on
    // drop — both normal return and unwind paths run this.
    struct EnvRestore(Option<std::ffi::OsString>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            // SAFETY: serialised by ENV_LOCK, same as the matching
            // `set_var` above.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("MOAGAN_HOME", v),
                    None => std::env::remove_var("MOAGAN_HOME"),
                }
            }
        }
    }
    let prev = std::env::var_os("MOAGAN_HOME");
    // SAFETY: serialised by ENV_LOCK; no other thread reads or
    // mutates MOAGAN_HOME while the guard is held.
    unsafe {
        std::env::set_var("MOAGAN_HOME", &dir);
    }
    let _restore = EnvRestore(prev);

    f(&dir)
    // Drop order on return / unwind (reverse of declaration):
    //   _restore → restores MOAGAN_HOME
    //   tmp      → removes the tempdir from /tmp
    //   _guard   → releases ENV_LOCK
    // All three run on the panic path too.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_moagan_home_sets_and_restores_env() {
        // Hold the global ENV_LOCK for the entire test so the
        // direct `unsafe { std::env::* }` mutations below (and the
        // final assertions) cannot race with sibling tests that
        // mutate MOAGAN_HOME via `with_moagan_home` (which itself
        // serialises on the same lock).
        let _guard = ENV_LOCK.lock();

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
        let previous = tempfile::Builder::new()
            .prefix("moagan-pre-existing-home-")
            .tempdir()
            .unwrap();
        let previous_path = previous.path().to_path_buf();
        unsafe {
            std::env::set_var("MOAGAN_HOME", &previous_path);
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
            new_dir, previous_path,
            "fresh tempdir must differ from the pre-existing value"
        );
        let after = std::env::var_os("MOAGAN_HOME").unwrap();
        assert_eq!(
            after.to_str().unwrap(),
            previous_path.to_str().unwrap(),
            "MOAGAN_HOME must be restored to its previous value on exit"
        );
        // `previous` (TempDir) drops at end of test → automatic cleanup.
    }

    #[test]
    fn with_moagan_home_creates_unique_dir_per_call() {
        // Two consecutive calls (both holding the lock) must get
        // different tempdir paths. `tempfile::Builder::tempdir`
        // appends a random suffix on top of the prefix, so a
        // duplicate would require the random source to repeat —
        // vanishingly unlikely. If a future change ever lost the
        // uniqueness, `tempdir()` itself would error on the
        // second `create_dir_all` and panic here.
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
