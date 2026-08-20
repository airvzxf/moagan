//! Runtime code coverage recorder. ADR-0002.
//!
//! The recorder is the source-level answer to "which lines of
//! moagan code did this run actually execute?". It hooks into the
//! LLVM SanCov runtime that `rustc` links into the binary when the
//! build is configured with `RUSTFLAGS="-Cinstrument-coverage"` and
//! the `coverage` Cargo feature is enabled. The runtime writes
//! `*.profraw` files at the path pointed at by `LLVM_PROFILE_FILE`;
//! snapshots are captured by renaming the active `profraw` so the
//! runtime starts a new one on its next write.
//!
//! # Three graceful-degradation states
//!
//! The recorder is always safe to construct, clone, and call into.
//! It adapts to the three states the binary can be in:
//!
//! 1. **Instrumented** (the `coverage` feature is on AND the binary
//!    was built with `-Cinstrument-coverage`). `enable` sets
//!    `LLVM_PROFILE_FILE` to a run-scoped path; the runtime writes
//!    counters there; `snapshot` renames the active file to a
//!    tag-named sibling so the runtime starts a fresh one.
//! 2. **Feature on, RUSTFLAGS missing** (operator enabled
//!    `--features coverage` but forgot the RUSTFLAGS). The env var
//!    is set but the runtime symbols are not linked, so no
//!    `profraw` is ever written. `snapshot` is a no-op that returns
//!    the active path; the `moagan coverage` subcommand reports a
//!    clear "no coverage data" error.
//! 3. **Default build** (no `--features coverage`). The recorder
//!    is constructed via `noop()`; every operation is a no-op that
//!    returns the active path (which is `/dev/null`).
//!
//! The recorder is intentionally NOT behind `#[cfg(feature =
//! "coverage")]` at the source level because the test suite needs
//! to call into it under all three states to verify the
//! no-op behaviour. Cargo `cfg` flags would only gate the runtime
//! symbols, not the call sites, and the call sites are cheap
//! enough to keep always-on.
//!
//! # Snapshot semantics
//!
//! `snapshot(tag)` is the moment-of-error primitive. It renames the
//! current `profraw` to `<run_id>-<tag>-<seq>.profraw` (atomic on
//! POSIX filesystems). The runtime detects the file is gone on its
//! next counter flush and creates a new `profraw` at the original
//! path. Each snapshot is therefore a *delta* of counters since
//! the previous snapshot — to get the cumulative coverage the
//! operator merges the snapshots with `llvm-profdata merge` (the
//! same workflow `cargo-llvm-cov` uses for test runs).
//!
//! The rename is racy with the runtime when a counter flush
//! happens between the existence check and the rename. In that
//! case the rename fails with `NotFound` and we return the active
//! path unchanged, preserving the lossy-window property: a missing
//! rename still gives the operator a path to look at.
//!
//! # Threading
//!
//! The recorder is `Clone` and uses an `Arc<AtomicU32>` for the
//! snapshot sequence so `snapshot(&self, ...)` does not need `&mut
//! self` and so cloned recorders (e.g. the one threaded through
//! [`crate::telemetry::Telemetry`]) see a single, monotonically
//! increasing sequence across the whole run. This prevents two
//! snapshots from the same phase from colliding on the same
//! filename.
//!
//! # See also
//!
//! - [`docs/adr/0002-runtime-coverage.md`] — design rationale.
//! - [`crate::cli::coverage_cmd`] — the `moagan coverage
//!   <run_id>` subcommand that consumes the recorded `profraw`
//!   files.
//! - [`crate::coverage::inspect`] — the inspection helpers that
//!   build a coverage report from the on-disk `profraw` files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::Result;
use crate::fs_layout::RunDir;
use crate::ids::RunId;

pub mod inspect;

pub use inspect::{
    CoverageReport, ProfrawEntry, ensure_instrumented, filter_by_tag, find_profraw,
    grcov_available, render_text, scan_run,
};

// ADR-0002 §A.2: explicit FFI declaration for the LLVM SanCov
// runtime. The stable Rust toolchain does NOT auto-link the
// `compiler-rt` profile runtime when the binary is built with
// `RUSTFLAGS="-Cinstrument-coverage"`; `cargo-llvm-cov` does
// it via a small FFI trick. We mirror that here: the extern
// declaration forces the linker to pull in the object file
// that provides `__llvm_profile_write_file` and friends, and
// the call site triggers a runtime flush of the in-memory
// counters to the file pointed at by `LLVM_PROFILE_FILE`.
//
// The newer compiler-rt also exports `__llvm_profile_dump`,
// which is a thin wrapper around `__llvm_profile_write_file`.
// We declare the lower-level entry point directly because
// (a) `__llvm_profile_dump` is not always present (older
// compiler-rt only ships the `_write_file` symbol) and
// (b) the local symbol `dump_runtime_counters` already
// documents the intent, so we do not need the extra wrapper.
//
// The FFI is gated behind the `coverage` Cargo feature so the
// default build (`cargo build` with no features) does not pull
// the symbol and the linker does not need a stub. On the default
// build the recorder is a true no-op; the snapshot's flush
// branch reduces to "no file existed before, no file exists
// after, return the active path".
#[cfg(feature = "coverage")]
unsafe extern "C" {
    fn __llvm_profile_write_file();
}

/// Best-effort flush of the SanCov runtime counters. Calls
/// `__llvm_profile_write_file()` when the recorder is active AND
/// the binary was linked against the runtime (i.e. the
/// `coverage` feature is on AND `RUSTFLAGS="-Cinstrument-coverage"`
/// was passed at build time). On the default build, the call is
/// compiled out entirely.
///
/// `#[inline(never)]` plus the volatile `black_box` on the
/// function pointer together defeat LLVM's "side-effect-free
/// extern" elision. The compiler-rt profile runtime writes to a
/// file the optimizer does not model, so without these hints the
/// call gets dropped. `core::hint::black_box` is the official
/// escape hatch for "this call has effects I cannot model".
#[cfg(feature = "coverage")]
#[inline(never)]
fn dump_runtime_counters() {
    // SAFETY: `__llvm_profile_write_file` is async-signal-safe
    // per the LLVM compiler-rt documentation, takes no arguments,
    // and returns no value. The `black_box` around the function
    // pointer prevents the optimizer from eliding the call as
    // side-effect-free.
    let f: unsafe extern "C" fn() = __llvm_profile_write_file;
    let f = std::hint::black_box(f);
    unsafe { f() };
}

/// Default-build stub. The `cfg`-gated `dump_runtime_counters`
/// is the one that actually does work; this one exists so the
/// call site in [`CoverageRecorder::snapshot`] does not need its
/// own `#[cfg]`.
#[cfg(not(feature = "coverage"))]
fn dump_runtime_counters() {
    // Intentionally empty: the default build has no SanCov
    // runtime linked, so there is nothing to flush.
}

/// One captured coverage snapshot. The path is stable; the
/// underlying `profraw` is frozen at the moment of the rename
/// (further counter updates go to a new `profraw` that the
/// runtime creates on the next write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageSnapshot {
    /// Path to the renamed `profraw` (frozen at the moment of the
    /// snapshot). If the runtime was not writing yet (no-op
    /// recorder, or feature off) the path is the recorder's
    /// `active_path()` and the file may not exist on disk.
    pub path: PathBuf,
    /// Tag the caller passed to [`CoverageRecorder::snapshot`].
    pub tag: String,
    /// Monotonic sequence number assigned by the recorder.
    /// Disambiguates snapshots that share a tag.
    pub seq: u32,
}

/// Runtime coverage recorder. Cheap to clone.
///
/// The internal sequence counter is wrapped in an `Arc` so cloned
/// recorders (e.g. the one threaded through
/// [`crate::telemetry::Telemetry`]) see a single, monotonically
/// increasing sequence across the whole run. This prevents two
/// snapshots from the same phase from colliding on the same
/// filename.
#[derive(Debug, Clone)]
pub struct CoverageRecorder {
    /// Path the runtime is currently writing to. After a snapshot,
    /// the runtime re-creates this file on its next write — so
    /// this field always points at the "live" target.
    active: PathBuf,
    /// Directory where snapshots are written.
    snapshots_dir: PathBuf,
    /// Sequence counter for snapshot filenames. Shared by `Arc` so
    /// clones see a single counter.
    seq: Arc<AtomicU32>,
    /// True when the binary is expected to have the SanCov runtime
    /// linked. False for the no-op recorder. Used by
    /// [`Self::is_active`].
    active_flag: bool,
}

impl CoverageRecorder {
    /// Wire the recorder into the LLVM SanCov runtime for the
    /// current process. Sets `LLVM_PROFILE_FILE` to
    /// `<run_dir>/telemetry/coverage/<run_id>.profraw` and ensures
    /// the parent directory exists.
    ///
    /// # Safety (env var)
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 because the
    /// env table is process-global and concurrent readers
    /// (e.g. `std::env::var` from another thread) can observe a
    /// partially-updated state. The recorder sets the var exactly
    /// once at the start of a run, before any other thread is
    /// spawned, so the unsafety is contained.
    pub fn enable(run_dir: &RunDir<'_>, run_id: RunId) -> Result<Self> {
        let coverage_dir = run_dir.coverage();
        std::fs::create_dir_all(&coverage_dir)?;
        let active = coverage_dir.join(format!("{run_id}.profraw"));
        // SAFETY: see fn docs. The recorder is constructed at run
        // start, single-threaded.
        unsafe {
            std::env::set_var("LLVM_PROFILE_FILE", &active);
        }
        Ok(Self {
            active,
            snapshots_dir: coverage_dir,
            seq: Arc::new(AtomicU32::new(0)),
            active_flag: true,
        })
    }

    /// Build a no-op recorder. All operations return safe defaults
    /// without touching the filesystem or the env table. Used by
    /// the test suite and by the `coverage` feature being off.
    pub fn noop() -> Self {
        Self {
            active: PathBuf::from("/dev/null"),
            snapshots_dir: PathBuf::from("/dev/null"),
            seq: Arc::new(AtomicU32::new(0)),
            active_flag: false,
        }
    }

    /// Is the recorder wired into the runtime? `true` after
    /// [`Self::enable`], `false` after [`Self::noop`]. The recorder
    /// does not try to detect "feature on, RUSTFLAGS missing"
    /// because the runtime does not expose a public "is
    /// instrumented" flag from Rust; that case is handled by the
    /// `moagan inspect coverage` subcommand reporting "no
    /// coverage data on disk" when the expected file is absent.
    pub fn is_active(&self) -> bool {
        self.active_flag
    }

    /// Path the runtime is currently writing to. Recorded into
    /// the `phase` and `call` JSONL events as `coverage_snapshot`
    /// so post-mortem correlation can find the right file.
    pub fn active_path(&self) -> &Path {
        &self.active
    }

    /// Snapshot the in-flight counters. Renames the current
    /// `profraw` to `<active>-<tag>-<seq>.profraw`; the runtime
    /// detects the file is gone on its next write and creates a
    /// new one at the original path. Returns the path of the
    /// renamed (frozen) snapshot.
    ///
    /// The sequence number is **always** monotonic across the
    /// lifetime of the recorder (shared across clones), so
    /// downstream consumers can sort snapshots deterministically
    /// even when the rename did not happen (no-op or NotFound
    /// race).
    ///
    /// The operation is a no-op when the recorder is not active
    /// or the active file does not exist yet (the runtime has not
    /// flushed any counters). Both cases return the active path
    /// so callers can always record *something* into the JSONL.
    pub fn snapshot(&self, tag: &str) -> Result<CoverageSnapshot> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            tag = tag,
            seq = seq,
            active = %self.active.display(),
            snapshots_dir = %self.snapshots_dir.display(),
            active_exists = self.active.exists(),
            active_flag = self.active_flag,
            "coverage: snapshot start"
        );
        if !self.active_flag {
            return Ok(CoverageSnapshot {
                path: self.active.clone(),
                tag: tag.to_owned(),
                seq,
            });
        }
        // Force the SanCov runtime to flush the in-memory
        // counters to the active `profraw` file. Without this
        // call the runtime buffers the counters and only writes
        // them on process exit (via `atexit`), so a long-running
        // pipeline would never produce a snapshot until the
        // run terminates.
        dump_runtime_counters();
        let after_dump = self.active.exists();
        let after_size = if after_dump {
            std::fs::metadata(&self.active)
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };
        tracing::debug!(
            tag = tag,
            seq = seq,
            after_dump_exists = after_dump,
            after_dump_size = after_size,
            "coverage: dump flushed"
        );
        if !after_dump {
            return Ok(CoverageSnapshot {
                path: self.active.clone(),
                tag: tag.to_owned(),
                seq,
            });
        }
        let active_name = self
            .active
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "coverage.profraw".to_owned());
        let snapshot_path = self
            .snapshots_dir
            .join(format!("{active_name}-{tag}-{seq}.profraw"));
        match std::fs::rename(&self.active, &snapshot_path) {
            Ok(()) => Ok(CoverageSnapshot {
                path: snapshot_path,
                tag: tag.to_owned(),
                seq,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // The runtime flushed and recreated the file
                // between our existence check and the rename. The
                // counters since the previous snapshot are now
                // sitting in the new active file; surface the
                // active path so the caller can correlate
                // against the *next* snapshot.
                Ok(CoverageSnapshot {
                    path: self.active.clone(),
                    tag: tag.to_owned(),
                    seq,
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Spawn a background thread that periodically checks the
    /// active `profraw` file size and triggers a snapshot when it
    /// exceeds `MOAGAN_COVERAGE_PROFRAW_BYTES_MAX` (default 1 GiB).
    /// The snapshot renames the live file to a seq-tagged sibling
    /// and the SanCov runtime creates a fresh one on its next
    /// write, so disk usage stays bounded over arbitrary runtimes.
    ///
    /// Verified context: run8 on 2026-08-19 produced a 66 GB
    /// `active.profraw` over 5h 40m because the previous build had
    /// no rotation cap. Recovery was a manual `rm -rf` under
    /// pressure with the disk at 96% used.
    ///
    /// The thread is a no-op when the recorder is not active.
    /// Returns the `JoinHandle` so the caller can join on shutdown
    /// (the impl currently does not block on it; the thread exits
    /// after [`Self::is_active`] flips off, which only happens via
    /// process exit anyway).
    pub fn start_rotation(
        &self,
        max_bytes: u64,
        interval_secs: u64,
    ) -> Option<std::thread::JoinHandle<()>> {
        if !self.active_flag {
            return None;
        }
        let me = self.clone();
        Some(std::thread::spawn(move || {
            let interval = std::time::Duration::from_secs(interval_secs.max(1));
            loop {
                std::thread::sleep(interval);
                let size = std::fs::metadata(&me.active).map(|m| m.len()).unwrap_or(0);
                if size > max_bytes {
                    let tag = format!("rotate-{}-{}", size, me.seq.load(Ordering::Relaxed));
                    if let Err(e) = me.snapshot(&tag) {
                        tracing::warn!(
                            error = %e,
                            profraw = %me.active.display(),
                            "coverage: rotation snapshot failed",
                        );
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_layout::MoaganHome;
    use crate::test_support::with_moagan_home;

    #[test]
    fn noop_is_inactive_and_writes_nothing() {
        let r = CoverageRecorder::noop();
        assert!(!r.is_active());
        let snap = r.snapshot("phase-1").unwrap();
        assert_eq!(snap.path, PathBuf::from("/dev/null"));
        assert_eq!(snap.tag, "phase-1");
        assert_eq!(snap.seq, 0);
        // The seq is monotonic across snapshots, even noop ones,
        // so two consecutive snapshots return distinct seq
        // values. This keeps the contract uniform across the
        // three recorder states (active, no-file, noop).
        let snap2 = r.snapshot("phase-2").unwrap();
        assert_eq!(snap2.path, PathBuf::from("/dev/null"));
        assert_eq!(snap2.seq, 1);
    }

    #[test]
    fn enable_sets_env_var_and_creates_dir() {
        with_moagan_home("coverage_enable_sets_env", |home_path| {
            let home = MoaganHome::at(home_path.to_path_buf());
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let r = CoverageRecorder::enable(&run_dir, RunId::new()).unwrap();
            assert!(r.is_active());
            // The active path lives in <run_dir>/telemetry/coverage/
            // and ends in `<run_id>.profraw`. Check both halves
            // separately so a future refactor that splits or
            // rejoins the path still trips the test.
            assert_eq!(r.active_path().parent(), Some(run_dir.coverage().as_path()));
            assert!(r.active_path().to_string_lossy().ends_with(".profraw"));
            assert_eq!(
                std::env::var("LLVM_PROFILE_FILE").unwrap(),
                r.active_path().to_string_lossy().to_string()
            );
            // The coverage dir was created by enable.
            assert!(run_dir.coverage().is_dir());
        });
    }

    #[test]
    fn snapshot_without_active_file_returns_active_path() {
        // Recorder is active (env var set, dir created) but the
        // runtime has not flushed any counters yet, so the
        // `profraw` does not exist. `snapshot` is a no-op.
        with_moagan_home("coverage_snapshot_no_file", |home_path| {
            let home = MoaganHome::at(home_path.to_path_buf());
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let r = CoverageRecorder::enable(&run_dir, RunId::new()).unwrap();
            let snap = r.snapshot("phase-1").unwrap();
            assert_eq!(snap.path, r.active_path().to_path_buf());
            assert_eq!(snap.tag, "phase-1");
        });
    }

    #[test]
    fn snapshot_renames_existing_profraw() {
        with_moagan_home("coverage_snapshot_rename", |home_path| {
            let home = MoaganHome::at(home_path.to_path_buf());
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let r = CoverageRecorder::enable(&run_dir, RunId::new()).unwrap();
            // Pretend the runtime wrote a `profraw` already.
            std::fs::write(r.active_path(), b"fake-profraw-bytes").unwrap();
            assert!(r.active_path().exists());

            let snap = r.snapshot("phase-2").unwrap();
            // The snapshot path is a sibling under the same
            // coverage dir, suffixed with the tag and seq.
            assert!(snap.path.to_string_lossy().contains("-phase-2-0.profraw"));
            // The active file is gone (the runtime will recreate
            // it on its next write).
            assert!(!r.active_path().exists());
            // The snapshot is a frozen copy of what was there.
            assert!(snap.path.exists());
            let bytes = std::fs::read(&snap.path).unwrap();
            assert_eq!(bytes, b"fake-profraw-bytes");
        });
    }

    #[test]
    fn snapshot_seq_increments_on_repeated_calls() {
        with_moagan_home("coverage_snapshot_seq", |home_path| {
            let home = MoaganHome::at(home_path.to_path_buf());
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let r = CoverageRecorder::enable(&run_dir, RunId::new()).unwrap();
            // Two snapshots of the same active file simulate
            // the runtime recreating the file between captures
            // (the active file is always empty between
            // snapshots, so the second `snapshot` call returns
            // the active path without renaming anything).
            std::fs::write(r.active_path(), b"a").unwrap();
            let s1 = r.snapshot("phase").unwrap();
            assert_eq!(s1.seq, 0);
            // The active file was renamed; the runtime would
            // re-create it on the next write, but our test does
            // not simulate that, so a second `snapshot` finds
            // the file missing and returns the active path.
            let s2 = r.snapshot("phase").unwrap();
            assert_eq!(s2.seq, 1);
            assert_eq!(s2.path, r.active_path().to_path_buf());
        });
    }

    #[test]
    fn clone_shares_seq_counter() {
        with_moagan_home("coverage_clone_seq", |home_path| {
            let home = MoaganHome::at(home_path.to_path_buf());
            let run_dir = home.run_dir(RunId::new());
            run_dir.ensure().unwrap();
            let r = CoverageRecorder::enable(&run_dir, RunId::new()).unwrap();
            let r2 = r.clone();
            // Both clones share the same seq counter, so two
            // snapshots taken through different clones are
            // disambiguated.
            std::fs::write(r.active_path(), b"x").unwrap();
            let _ = r.snapshot("a").unwrap();
            std::fs::write(r2.active_path(), b"y").unwrap();
            let s2 = r2.snapshot("b").unwrap();
            assert_eq!(s2.seq, 1);
        });
    }
}
