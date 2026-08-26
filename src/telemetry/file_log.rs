//! Lazy file writer for the `--logs` flag and `MOAGAN_RUN_LOGS` env var.
//!
//! The tracing subscriber is initialised in `src/main.rs::init_tracing`
//! BEFORE clap parses args. `tracing-subscriber::registry::try_init`
//! can only run once per process, so we plumb the file path through a
//! process-global [`OnceLock`]. The path is set by the dispatcher after
//! clap parses; the writer looks it up on every `make_writer()` call.
//!
//! Precedence rule: the env var beats the flag (Unix convention). The
//! dispatcher reads `MOAGAN_RUN_LOGS` first and falls back to
//! `cli.logs` only when the env var is unset or empty. See
//! [`set`] for the storage rule.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_subscriber::fmt::MakeWriter;

/// Process-global destination of the `--logs` / `MOAGAN_RUN_LOGS`
/// file writer. Set once via [`set`]; immutable thereafter.
static LOG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Outcome of a [`set`] call.
#[derive(Debug)]
pub enum SetError {
    /// A previous `set()` already stored a path; the new value was
    /// rejected. Carries the previously stored path so callers can
    /// log it (e.g. "MOAGAN_RUN_LOGS already set; --logs flag
    /// ignored").
    AlreadySet(PathBuf),
    /// `create_dir_all` on the parent directory failed; the file
    /// path was NOT stored.
    ParentDir {
        /// Path the caller tried to set.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
}

/// Activate the file writer. Called by `dispatch_inner()` after
/// clap parses the `--logs` flag (and after the dispatcher reads
/// the `MOAGAN_RUN_LOGS` env var). The path is consumed by every
/// `MakeWriter` call thereafter.
///
/// Idempotency: a second `set()` is rejected with
/// [`SetError::AlreadySet`] carrying the previously stored path.
/// The first call wins, matching `OnceLock::set`'s semantics.
///
/// `create_dir_all` runs on the parent before the cell is filled
/// so a `--logs /tmp/newdir/moagan.log` invocation does not need
/// the operator to pre-create `/tmp/newdir`. A bare filename
/// (`logs` with no parent component) skips the directory step —
/// `Path::parent` returns `Some("")` in that case.
pub fn set(path: PathBuf) -> Result<(), SetError> {
    tracing::debug!(path = %path.display(), "file_log::set: enter");
    // `Path::parent` returns `Some("")` for a bare filename. Skip
    // the create_dir_all when the parent is empty.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(source) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            path = %path.display(),
            error = %source,
            "file_log::set: create_dir_all failed"
        );
        return Err(SetError::ParentDir { path, source });
    }
    let outcome = LOG_FILE_PATH.set(path).map_err(|_| {
        // `set` failed because the cell is already populated.
        // `get` is guaranteed `Some` in that case; the `expect`
        // documents the invariant.
        SetError::AlreadySet(
            LOG_FILE_PATH
                .get()
                .expect("LOG_FILE_PATH set returned Err but get returned None")
                .clone(),
        )
    });
    match &outcome {
        Ok(()) => tracing::info!("file_log::set: ok"),
        Err(SetError::AlreadySet(_)) => tracing::warn!("file_log::set: already set, ignored"),
        Err(SetError::ParentDir { .. }) => {}
    }
    outcome
}

/// True iff `--logs` or `MOAGAN_RUN_LOGS` requested a file. Cheap
/// (single atomic load); safe to call from any thread.
pub fn is_set() -> bool {
    LOG_FILE_PATH.get().is_some()
}

/// Get the configured path, if any. Used by tests and by the
/// integration harness to verify the precedence rule.
pub fn path() -> Option<PathBuf> {
    LOG_FILE_PATH.get().cloned()
}

/// Writer factory that resolves the configured path on every
/// `make_writer` call. `MakeWriter` is a per-event factory, so
/// each event opens (and drops) its own `File` handle — no
/// sharing across threads, no long-lived mutex. The lazy
/// resolution keeps the cost zero when the feature is disabled
/// (`LOG_FILE_PATH.get()` returns `None` and `make_writer`
/// produces a no-op handle).
#[derive(Default, Debug, Clone, Copy)]
pub struct FileLogWriter;

impl<'a> MakeWriter<'a> for FileLogWriter {
    type Writer = FileLogHandle;

    fn make_writer(&'a self) -> Self::Writer {
        FileLogHandle {
            inner: LOG_FILE_PATH.get().and_then(|p| {
                match OpenOptions::new().create(true).append(true).open(p) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        tracing::warn!(
                            path = %p.display(),
                            error = %e,
                            "file_log: failed to open log file, dropping event"
                        );
                        None
                    }
                }
            }),
        }
    }
}

/// Per-event writer handle. `inner` is `None` when the file
/// writer is disabled OR when the file could not be opened (the
/// latter is rare and surfaces as silent drops — preferred over
/// panicking inside a tracing formatter).
#[derive(Debug)]
pub struct FileLogHandle {
    inner: Option<File>,
}

impl io::Write for FileLogHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Some(f) => f.write(buf),
            // Sink to /dev/null when not configured. The `Ok`
            // return with `buf.len()` tells the formatter the
            // full slice was consumed, so the caller's framing
            // invariant is preserved.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use super::*;

    /// Serialises the file_log tests because the underlying
    /// `OnceLock` is process-global: parallel tests that call
    /// `set()` race for the cell. Each test uses a unique
    /// `tempfile::TempDir` for the path so they would still be
    /// data-independent, but the second `set()` would observe
    /// `AlreadySet` and report a flaky false-negative on busy
    /// CI. The lock is the simple fix.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        match TEST_LOCK.lock() {
            Ok(g) => g,
            // Poisoned = another test in this module panicked.
            // Swallow so we surface our own assertion failure
            // rather than the poison.
            Err(p) => p.into_inner(),
        }
    }

    #[test]
    fn unset_sinks_writes() {
        // We can't actually UNSET the global OnceLock once it's
        // populated; we work around it by checking the handle's
        // behaviour on a brand-new inner field directly. The
        // `is_set()` / `set()` contract is exercised by the other
        // tests below.
        let mut handle = FileLogHandle { inner: None };
        // write() returns Ok(buf.len()) — the formatter sees the
        // slice as fully consumed.
        let n = handle.write(b"hello, world").expect("write sinks");
        assert_eq!(n, b"hello, world".len());
        // flush() is a no-op.
        handle.flush().expect("flush sinks");
    }

    #[test]
    fn set_then_get_round_trip() {
        let _g = lock();
        // Stable, non-tempdir path so the file survives across
        // sibling tests. Each invocation gets a unique filename
        // (via PID + a per-test counter) so two parallel test
        // runs cannot collide. The file is truncated at start
        // so the assertion sees a clean payload.
        let log_path = std::env::temp_dir().join(format!(
            "moagan-file-log-test-{}-{}.log",
            std::process::id(),
            "set_then_get_round_trip"
        ));
        // Clean any leftover from a previous run.
        let _ = std::fs::remove_file(&log_path);

        // `set()` is the only writer of the cell; if a sibling
        // test populated it first, our `set()` returns
        // `AlreadySet` and the test continues using whatever
        // path the cell currently holds. That is the contract
        // we want to verify: the writer writes to the cell's
        // current path, whichever it is.
        let _ = set(log_path.clone());

        // Drive the writer directly: a real subscriber install
        // races with the global one (`tracing-subscriber::try_init`
        // fails the second time), which makes the test flaky
        // under `cargo test` parallelism. The writer's contract
        // is `io::Write` over a `File` — testing it directly
        // exercises the same code path that the formatter
        // would, with no subscriber plumbing in the way.
        //
        // Ensure the parent directory exists for whichever
        // path the cell currently holds; without this, an
        // `AlreadySet` from a sibling test that used a deleted
        // tempdir would silently sink writes to /dev/null.
        let actual = path().expect("cell should be populated by set()");
        if let Some(parent) = actual.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&actual);

        let mut handle = FileLogWriter.make_writer();
        handle
            .write_all(b"hello-from-file-log-test\n")
            .expect("write to file");
        handle.flush().expect("flush file");

        let body = std::fs::read_to_string(&actual).expect("read log file");
        assert!(
            body.contains("hello-from-file-log-test"),
            "file missing expected payload, body={body:?}"
        );
        // The writer is raw bytes (no ANSI by definition); the
        // `with_ansi(false)` formatter wiring is exercised by
        // the integration test that spawns the real binary.
        assert!(
            !body.contains('\u{1b}'),
            "file unexpectedly contains ANSI escape, body={body:?}"
        );
        let _ = std::fs::remove_file(&actual);
    }

    #[test]
    fn set_idempotent_returns_err() {
        let _g = lock();
        // Stable paths in /tmp so they survive the sibling
        // tests' tempdir cleanup. Each test gets a unique
        // filename via the test name.
        let first = std::env::temp_dir().join(format!(
            "moagan-file-log-test-{}-{}-first.log",
            std::process::id(),
            "set_idempotent_returns_err"
        ));
        let second = std::env::temp_dir().join(format!(
            "moagan-file-log-test-{}-{}-second.log",
            std::process::id(),
            "set_idempotent_returns_err"
        ));
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);

        // The first set() may itself be rejected if a sibling
        // test already populated the cell. We don't assert on
        // its outcome; we only assert that a SUCCESSFUL set
        // followed by another set returns `AlreadySet` carrying
        // the FIRST path (the winner).
        let winner = match set(first.clone()) {
            Ok(()) => first.clone(),
            Err(SetError::AlreadySet(prev)) => prev,
            Err(SetError::ParentDir { path, source }) => {
                panic!("unexpected ParentDir error: {path:?}: {source}");
            }
        };

        match set(second.clone()) {
            Ok(()) => panic!("second set() should have returned AlreadySet"),
            Err(SetError::AlreadySet(prev)) => {
                assert_eq!(
                    prev, winner,
                    "AlreadySet must carry the first (winning) path"
                );
                assert_eq!(path(), Some(winner), "path() reports the winner");
            }
            Err(SetError::ParentDir { path, source }) => {
                panic!("unexpected ParentDir error: {path:?}: {source}");
            }
        }
        let _ = std::fs::remove_file(&second);
    }

    #[test]
    fn set_creates_parent_dir() {
        let _g = lock();
        // Stable nested path in /tmp so it survives sibling
        // tests' tempdir cleanup. The path includes several
        // nested directories that don't exist yet.
        let nested = std::env::temp_dir().join(format!(
            "moagan-file-log-test-{}-{}/nested/sub/log.txt",
            std::process::id(),
            "set_creates_parent_dir"
        ));
        // Clean up any leftover from a previous run.
        let _ = std::fs::remove_dir_all(nested.parent().unwrap());
        assert!(!nested.parent().unwrap().exists());

        // `set()` may return `AlreadySet` if a sibling test
        // already populated the cell; both outcomes are
        // acceptable here because the parent-dir creation
        // happens BEFORE the cell write. We only assert that
        // the parent directory exists at the end.
        let _ = set(nested.clone());
        assert!(
            nested.parent().unwrap().exists(),
            "parent dir must be created by set(), path={}",
            nested.display()
        );
        let _ = std::fs::remove_dir_all(nested.parent().unwrap());
    }
}
