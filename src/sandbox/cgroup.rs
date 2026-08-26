//! Opt-in cgroup v2 resource isolation for the sandbox.
//!
//! Unix-only. When `/sys/fs/cgroup/cgroup.controllers` exists, the
//! sandbox creates a child cgroup scoped to the run, applies the
//! requested limits, and moves the child PID into it via the
//! canonical `cgroup.procs` write. Otherwise it falls back to
//! `libc::prlimit` per-process limits (a strictly weaker guarantee:
//! cgroup v2 enforces after fork, prlimit only for the immediate
//! child).
//!
//! The cgroup path requires the kernel to expose `cpu`, `memory`,
//! and `pids` controllers (`/sys/fs/cgroup/cgroup.controllers` lists
//! them). On a standard systemd host or a Docker container with
//! `cgroupns` delegation, all three are present. On a stripped-down
//! container only some may be available; the function silently
//! skips the missing controllers so a partial mount still produces a
//! functional cgroup.
//!
//! ## Compliance
//!
//! Catalog `10-integrada-v0` §D.11.1 — "cgroup v2 + prlimit
//! fallback". The defaults (1 CPU, 2 GiB, 512 PIDs) reflect a
//! conservative compile-friendly profile that the validator runs
//! can fit inside without hitting OOM.
//!
//! ## Usage
//!
//! Operators opt in via `MOAGAN_SANDBOX_CGROUP=enabled` (or any
//! truthy value) or by setting `sandbox_cgroup = { cpu_max = "...",
//! memory_max_bytes = ..., pids_max = ... }` in
//! `~/.config/moagan/config.toml`. The sandbox's `pre_exec` hook
//! calls [`apply`] between fork and exec; the chosen backend is
//! logged via `tracing::info!` for observability.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Resource limits applied to a sandboxed subprocess.
///
/// Every field is optional so operators can scope the cgroup to a
/// subset of controllers (e.g. only `memory.max` if `cpu` and
/// `pids` are not delegated). The kernel silently rejects the
/// controller files that aren't available; see [`apply`] for the
/// fall-through behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct CgroupLimits {
    /// `cpu.max` value (e.g. `"50000 100000"` for 50% of one CPU).
    /// The format is `<quota> <period>` in microseconds; `period`
    /// defaults to `100000` (100 ms) on most distros.
    pub cpu_max: Option<String>,
    /// `memory.max` value in bytes.
    pub memory_max_bytes: Option<u64>,
    /// `pids.max` value (max processes/threads in the cgroup).
    pub pids_max: Option<u64>,
}

impl Default for CgroupLimits {
    fn default() -> Self {
        Self {
            cpu_max: Some("100000 100000".into()),
            memory_max_bytes: Some(2 * 1024 * 1024 * 1024),
            pids_max: Some(512),
        }
    }
}

/// Which backend successfully applied the resource limits. Returned
/// by [`apply`] so the caller can log the chosen path for
/// observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CgroupBackend {
    /// cgroup v2 was available and the child cgroup was created +
    /// the PID moved into it.
    CgroupV2,
    /// cgroup v2 was unavailable (or the cgroup write failed) and
    /// the per-process `prlimit` fallback ran instead.
    Prlimit,
    /// Neither mechanism could enforce limits (e.g. non-Unix host
    /// or `prlimit` itself failed). The caller should log a warning.
    None,
}

impl std::fmt::Display for CgroupBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CgroupV2 => "cgroup_v2",
            Self::Prlimit => "prlimit",
            Self::None => "none",
        })
    }
}

/// Detect cgroup v2 availability by reading
/// `/sys/fs/cgroup/cgroup.controllers`. Returns `false` on every
/// non-Linux platform.
pub fn cgroup_v2_available() -> bool {
    let available = cgroup_v2_path().is_some();
    tracing::trace!(sandbox = "cgroup", available, "cgroup_v2_available probe");
    available
}

/// Canonical cgroup v2 mount path, when present.
fn cgroup_v2_path() -> Option<&'static Path> {
    static PATH: &str = "/sys/fs/cgroup";
    let path = Path::new(PATH);
    if path.join("cgroup.controllers").exists() {
        Some(path)
    } else {
        None
    }
}

/// Apply resource limits using whichever mechanism is available.
///
/// Resolution order:
/// 1. cgroup v2 (`/sys/fs/cgroup/cgroup.controllers` exists) →
///    create `moagan-<pid>-<nanos>` under `/sys/fs/cgroup`, write
///    each non-`None` field, move the current PID into the cgroup.
/// 2. cgroup v2 unavailable or cgroup write failed → `libc::prlimit`
///    on `RLIMIT_NPROC` / `RLIMIT_AS`.
/// 3. Non-Unix host → [`CgroupBackend::None`].
///
/// The cgroup path is best-effort: missing controllers (e.g. no
/// `pids` delegation on a container) are silently skipped so a
/// partial mount still produces a usable cgroup. A structural
/// failure (mount gone, EACCES on `cgroup.procs`) downgrades to the
/// `prlimit` fallback rather than aborting the run.
///
/// Returns the backend that ultimately applied the limits so the
/// caller can log it.
pub fn apply(limits: &CgroupLimits) -> Result<CgroupBackend> {
    apply_for_target(limits)
}

#[cfg(unix)]
fn apply_for_target(limits: &CgroupLimits) -> Result<CgroupBackend> {
    tracing::debug!(
        sandbox = "cgroup",
        cpu_max = ?limits.cpu_max,
        memory_max_bytes = ?limits.memory_max_bytes,
        pids_max = ?limits.pids_max,
        "apply_for_target: resolving cgroup backend"
    );
    if let Some(root) = cgroup_v2_path() {
        match create_and_configure_cgroup(root, limits) {
            Ok(()) => {
                tracing::info!(
                    sandbox = "cgroup",
                    backend = %CgroupBackend::CgroupV2,
                    "cgroup v2 path succeeded"
                );
                return Ok(CgroupBackend::CgroupV2);
            }
            Err(error) => {
                tracing::warn!(
                    sandbox = "cgroup",
                    error = %error,
                    "cgroup v2 setup failed; falling back to prlimit",
                );
            }
        }
    } else {
        tracing::debug!(
            sandbox = "cgroup",
            "cgroup v2 mount unavailable; going to prlimit path"
        );
    }
    apply_prlimit(limits).map_err(|error| {
        tracing::warn!(
            sandbox = "cgroup",
            error = %error,
            "prlimit fallback also failed; sandbox runs without resource limits",
        );
        error
    })?;
    Ok(CgroupBackend::Prlimit)
}

#[cfg(not(unix))]
fn apply_for_target(limits: &CgroupLimits) -> Result<CgroupBackend> {
    tracing::trace!(sandbox = "cgroup", "apply_for_target: no-op on non-Unix");
    let _ = limits;
    Ok(CgroupBackend::None)
}

/// Create `moagan-<pid>-<nanos>` under `root`, write every
/// non-`None` limit, and move the current PID into it.
///
/// Missing controller files (e.g. `pids.max` not delegated) are
/// silently skipped: a stripped-down mount that still has `cpu` +
/// `memory` is treated as a partial success. Hard failures (missing
/// root, EACCES on `cgroup.procs`) propagate as `Err`.
#[cfg(unix)]
fn create_and_configure_cgroup(root: &Path, limits: &CgroupLimits) -> Result<()> {
    use std::fs;
    use std::io;

    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let cgroup_path: PathBuf = root.join(format!("moagan-{pid}-{nanos}"));
    tracing::debug!(
        sandbox = "cgroup",
        path = %cgroup_path.display(),
        pid,
        "create_and_configure_cgroup: creating child cgroup"
    );

    fs::create_dir_all(&cgroup_path)?;
    if let Some(cpu) = &limits.cpu_max {
        write_cgroup_file(&cgroup_path, "cpu.max", cpu)?;
    }
    if let Some(mem) = limits.memory_max_bytes {
        write_cgroup_file(&cgroup_path, "memory.max", &mem.to_string())?;
    }
    if let Some(pids) = limits.pids_max {
        write_cgroup_file(&cgroup_path, "pids.max", &pids.to_string())?;
    }
    // Move current PID into the cgroup. `cgroup.procs` accepts a
    // single PID per write; writing the empty string is a no-op.
    let pid_str = pid.to_string();
    fs::write(cgroup_path.join("cgroup.procs"), &pid_str).map_err(|error| {
        io::Error::new(error.kind(), format!("cgroup.procs write failed: {error}"))
    })?;
    tracing::info!(
        sandbox = "cgroup",
        path = %cgroup_path.display(),
        pid,
        "create_and_configure_cgroup: child PID moved into cgroup"
    );
    Ok(())
}

/// Write `value` to `<cgroup>/<name>`, ignoring ENOENT (the
/// controller is not delegated). Any other I/O error propagates.
#[cfg(unix)]
fn write_cgroup_file(cgroup: &Path, name: &str, value: &str) -> Result<()> {
    use std::io;
    match std::fs::write(cgroup.join(name), value) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(
                sandbox = "cgroup",
                controller = name,
                "controller not delegated; skipping",
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Apply per-process resource limits via `libc::prlimit`. This is
/// the strictly weaker fallback: the limit only applies to the
/// calling process (the child, when invoked from `pre_exec`),
/// whereas a cgroup v2 write would catch descendants.
///
/// `RLIMIT_NPROC` requires `CAP_SYS_RESOURCE` on Linux. When the
/// caller lacks the capability the syscall returns `EPERM`; we
/// surface that so the upstream log line is accurate but the sandbox
/// does not abort.
#[cfg(unix)]
fn apply_prlimit(limits: &CgroupLimits) -> Result<()> {
    use std::io;

    tracing::debug!(
        sandbox = "cgroup",
        pids_max = ?limits.pids_max,
        memory_max_bytes = ?limits.memory_max_bytes,
        "apply_prlimit: applying per-process rlimits"
    );

    // SAFETY: `prlimit(0, ...)` mutates the calling process; both
    // arguments are well-formed references to stack-allocated
    // `rlimit` values. `prlimit` is async-signal-safe.
    unsafe {
        if let Some(pids) = limits.pids_max {
            let rlim = libc::rlimit {
                rlim_cur: pids,
                rlim_max: pids,
            };
            if libc::prlimit(0, libc::RLIMIT_NPROC, &rlim, std::ptr::null_mut()) != 0 {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::PermissionDenied {
                    tracing::error!(
                        sandbox = "cgroup",
                        rlimit = "RLIMIT_NPROC",
                        error = %err,
                        "apply_prlimit: RLIMIT_NPROC failed (non-EPERM)"
                    );
                    return Err(err.into());
                }
                tracing::debug!(
                    sandbox = "cgroup",
                    rlimit = "RLIMIT_NPROC",
                    "EPERM on RLIMIT_NPROC; CAP_SYS_RESOURCE missing",
                );
            } else {
                tracing::trace!(
                    sandbox = "cgroup",
                    rlimit = "RLIMIT_NPROC",
                    pids,
                    "RLIMIT_NPROC applied"
                );
            }
        }
        if let Some(mem) = limits.memory_max_bytes {
            let rlim = libc::rlimit {
                rlim_cur: mem,
                rlim_max: mem,
            };
            if libc::prlimit(0, libc::RLIMIT_AS, &rlim, std::ptr::null_mut()) != 0 {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::PermissionDenied {
                    tracing::error!(
                        sandbox = "cgroup",
                        rlimit = "RLIMIT_AS",
                        error = %err,
                        "apply_prlimit: RLIMIT_AS failed (non-EPERM)"
                    );
                    return Err(err.into());
                }
                tracing::debug!(
                    sandbox = "cgroup",
                    rlimit = "RLIMIT_AS",
                    "EPERM on RLIMIT_AS; CAP_SYS_RESOURCE missing",
                );
            } else {
                tracing::trace!(
                    sandbox = "cgroup",
                    rlimit = "RLIMIT_AS",
                    bytes = mem,
                    "RLIMIT_AS applied"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catalog §D.11.1: `cgroup_v2_available` returns `true` when
    /// the kernel mounts cgroup v2 at `/sys/fs/cgroup` (the
    /// canonical location on every modern systemd / Docker host).
    /// On a host without the mount — e.g. macOS, Windows, or a CI
    /// container without `cgroupns` — the test prints a skip notice
    /// and exits early so it does not trip in those environments.
    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_v2_available_returns_true_on_linux_with_cgroup2() {
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            eprintln!("skipping: /sys/fs/cgroup/cgroup.controllers not present");
            return;
        }
        assert!(
            cgroup_v2_available(),
            "cgroup v2 mount detected but cgroup_v2_available returned false"
        );
    }

    /// Catalog §D.11.1: on a host without cgroup v2 the helper must
    /// return `false`. The only way to assert that is to run on a
    /// host that genuinely lacks cgroup v2; everywhere else the
    /// test is a no-op so it does not false-fail in CI.
    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_v2_available_returns_false_without_cgroup2() {
        if cgroup_v2_available() {
            eprintln!("skipping: cgroup v2 is available on this host");
            return;
        }
        assert!(!cgroup_v2_available());
    }

    /// Non-Linux hosts never have cgroup v2 (the kernel feature is
    /// Linux-only). The helper must reflect that.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn cgroup_v2_available_returns_false_on_non_linux() {
        assert!(!cgroup_v2_available());
    }

    /// Pin the default profile so a refactor that bumps the
    /// per-process caps surfaces as a test failure. The values are
    /// documented in the module doc-comment as the
    /// "compile-friendly profile".
    #[test]
    fn cgroup_limits_default_sensible_values() {
        let limits = CgroupLimits::default();
        assert_eq!(limits.cpu_max.as_deref(), Some("100000 100000"));
        assert_eq!(limits.memory_max_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(limits.pids_max, Some(512));
    }

    /// TOML round-trip: every field is preserved so operators can
    /// pin their override in `~/.config/moagan/config.toml`.
    #[test]
    fn cgroup_limits_toml_round_trip() {
        let limits = CgroupLimits {
            cpu_max: Some("50000 100000".into()),
            memory_max_bytes: Some(512 * 1024 * 1024),
            pids_max: Some(64),
        };
        let raw = toml::to_string(&limits).expect("serialise");
        let back: CgroupLimits = toml::from_str(&raw).expect("deserialise");
        assert_eq!(back, limits);
    }

    /// Catalog §D.11.1: when cgroup v2 is unavailable, `apply` must
    /// land on the `prlimit` fallback rather than silently dropping
    /// the limits. On a host with cgroup v2 the test prints a skip
    /// notice so it does not false-fail.
    #[cfg(unix)]
    #[test]
    fn cgroup_apply_returns_prlimit_backend_when_cgroup2_unavailable() {
        if cgroup_v2_available() {
            eprintln!("skipping: cgroup v2 is available on this host");
            return;
        }
        let limits = CgroupLimits::default();
        let backend = apply(&limits).expect("apply succeeds");
        assert_eq!(backend, CgroupBackend::Prlimit);
    }

    /// On non-Unix hosts (Windows, no `cfg(unix)`) `apply` is a
    /// no-op: cgroup v2 does not exist and there is no `libc` to
    /// call. The function must still return `Ok(CgroupBackend::None)`
    /// so the wire-up does not panic.
    #[cfg(not(unix))]
    #[test]
    fn cgroup_apply_is_noop_on_non_unix() {
        let limits = CgroupLimits::default();
        let backend = apply(&limits).expect("apply succeeds on non-unix");
        assert_eq!(backend, CgroupBackend::None);
    }

    /// Apply RLIMIT_NPROC via `prlimit` and read it back. Setting
    /// `RLIMIT_NPROC` requires `CAP_SYS_RESOURCE`; on a stripped
    /// host the write silently fails (`EPERM` is logged at debug)
    /// and the read returns whatever the kernel currently has. The
    /// test therefore tolerates a no-op outcome: it must simply not
    /// panic and must return `Ok(())` from `apply`.
    ///
    /// Marked `#[ignore]` because `prlimit` mutates a *process-wide*
    /// resource and `cargo test` runs every test in the same
    /// process. A 987-PID cap would race with every other test that
    /// spawns a subprocess (e.g. sqlite migrations that fork a
    /// child) and surface as `fork: Resource temporarily unavailable`
    /// flakes. Operators can opt in explicitly via
    /// `cargo test -- --ignored prlimit_apply` when reviewing
    /// changes to this module; the CI gauntlet does not.
    #[cfg(unix)]
    #[test]
    #[ignore = "mutates process-wide RLIMIT_NPROC; run via cargo test -- --ignored"]
    fn prlimit_apply_sets_nproc_rlimit() {
        let limits = CgroupLimits {
            cpu_max: None,
            memory_max_bytes: None,
            pids_max: Some(987),
        };
        // Force the prlimit path even on hosts that have cgroup v2
        // by calling the private helper directly.
        let result = apply_prlimit(&limits);
        assert!(
            result.is_ok(),
            "apply_prlimit must return Ok, got {result:?}"
        );

        // Read RLIMIT_NPROC back. If the read itself fails (no caps)
        // we still consider the test passed — the contract is "best
        // effort, never panic".
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let read = unsafe { libc::prlimit(0, libc::RLIMIT_NPROC, std::ptr::null(), &mut current) };
        if read == 0 {
            assert_eq!(
                current.rlim_cur, 987,
                "RLIMIT_NPROC soft limit must equal the requested value"
            );
            assert_eq!(
                current.rlim_max, 987,
                "RLIMIT_NPROC hard limit must equal the requested value"
            );
        }
    }

    /// Apply RLIMIT_AS via `prlimit` and read it back. RLIMIT_AS is
    /// the address-space limit; the soft/hard pair share the same
    /// value. `EPERM` is tolerated (no `CAP_SYS_RESOURCE`).
    ///
    /// Marked `#[ignore]` for the same reason as
    /// [`prlimit_apply_sets_nproc_rlimit`]: a process-wide
    /// address-space cap can race with concurrent tests that
    /// allocate memory and surface as spurious flakes.
    #[cfg(unix)]
    #[test]
    #[ignore = "mutates process-wide RLIMIT_AS; run via cargo test -- --ignored"]
    fn prlimit_apply_sets_as_rlimit() {
        let limits = CgroupLimits {
            cpu_max: None,
            memory_max_bytes: Some(987_654_321),
            pids_max: None,
        };
        let result = apply_prlimit(&limits);
        assert!(
            result.is_ok(),
            "apply_prlimit must return Ok, got {result:?}"
        );

        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let read = unsafe { libc::prlimit(0, libc::RLIMIT_AS, std::ptr::null(), &mut current) };
        if read == 0 {
            assert_eq!(
                current.rlim_cur, 987_654_321,
                "RLIMIT_AS soft limit must equal the requested value"
            );
            assert_eq!(
                current.rlim_max, 987_654_321,
                "RLIMIT_AS hard limit must equal the requested value"
            );
        }
    }

    /// Catalog §D.11.1: serde `snake_case` wire format so the enum
    /// survives a TOML round-trip on
    /// `sandbox_cgroup = "..."`-style configs. Pin the values so a
    /// refactor that flips the convention trips the test.
    #[test]
    fn cgroup_backend_serializes_to_snake_case() {
        let json = serde_json::to_string(&CgroupBackend::CgroupV2).expect("serialise");
        assert_eq!(
            json, "\"cgroup_v2\"",
            "CgroupV2 must serialise as snake_case"
        );
        let json = serde_json::to_string(&CgroupBackend::Prlimit).expect("serialise");
        assert_eq!(json, "\"prlimit\"", "Prlimit must serialise as snake_case");
        let json = serde_json::to_string(&CgroupBackend::None).expect("serialise");
        assert_eq!(json, "\"none\"", "None must serialise as snake_case");
    }

    /// Round-trip the enum through JSON so `Display`-style logs and
    /// structured telemetry agree on the spelling.
    #[test]
    fn cgroup_backend_json_round_trip() {
        for backend in [
            CgroupBackend::CgroupV2,
            CgroupBackend::Prlimit,
            CgroupBackend::None,
        ] {
            let json = serde_json::to_string(&backend).expect("serialise");
            let back: CgroupBackend = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, backend);
        }
    }

    /// `Display` must agree with serde so the `tracing::info!` log
    /// lines (which use `%backend` and therefore `Display`) match
    /// the JSON telemetry on the same backend label.
    #[test]
    fn cgroup_backend_display_matches_serde() {
        assert_eq!(CgroupBackend::CgroupV2.to_string(), "cgroup_v2");
        assert_eq!(CgroupBackend::Prlimit.to_string(), "prlimit");
        assert_eq!(CgroupBackend::None.to_string(), "none");
    }
}
