//! Subprocess sandbox for executable validation.
//!
//! [`Sandbox`] wraps `tokio::process::Command` with a fresh
//! `tempfile::TempDir` per `run`, a hard wall-clock timeout, an
//! allowlist + denylist policy, and a capped stdout/stderr buffer
//! (64 KiB each by default) so a runaway process cannot blow up the
//! run memory.
//!
//! The sandbox inherits the process environment but strips anything
//! that smells like a secret (see [`SandboxConfig::strip_secrets_env`])
//! and forces `PATH` to the standard system paths and `HOME` to the
//! scratch directory.
//!
//! Compliance: `proposal-02-rust.md` §7 plus the implemented portions
//! of catalog 10-integrada-v0 §D.11. The remaining hardened variants
//! (`cgroup`, `unshare`, `seccomp`) are still opt-in catalog overlays.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use thiserror::Error as ThisError;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::cancel::{Cancel, HARD_KILL_GRACE};
use crate::error::{Error, Result};
use crate::redact::{RedactPolicy, Surface, apply};

use super::allowlist::{Allowlist, Denylist, contains_deny_token, is_allowed};
use super::policy::NetworkPolicy;
use super::seccomp::{self, SeccompPolicyKind};

/// Cap stdout/stderr to `max_bytes` per stream. When the cap is hit,
/// the sandbox returns [`SandboxError::OutputTruncated`] and the
/// process is killed (D.11.4). The default is 64 KiB.
pub const DEFAULT_OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// Maximum bytes captured per stdout stream by default.
pub const MAX_STDOUT_BYTES: usize = DEFAULT_OUTPUT_CAP_BYTES;
/// Maximum bytes captured per stderr stream by default.
pub const MAX_STDERR_BYTES: usize = DEFAULT_OUTPUT_CAP_BYTES;

/// Errors raised by the hardened subprocess controls.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum SandboxError {
    /// The child attempted to write beyond the configured per-stream cap.
    #[error("sandbox output truncated")]
    OutputTruncated,
    /// The requested binary could not be resolved in `PATH`.
    #[error("sandbox binary not found: {0}")]
    BinaryNotFound(String),
    /// The command or one of its arguments was rejected by the
    /// allowlist / denylist. Carries the human-readable reason.
    /// Added in D.11.15 so the new [`Sandbox::run_cmd`] API can
    /// report policy rejections as `Err` instead of encoding them
    /// into a [`SandboxResult::status`].
    #[error("sandbox policy rejection: {0}")]
    NotAllowed(String),
    /// The configured [`NetworkPolicy`] denied the host a subprocess
    /// would have reached. Catalog §D.11.13. Currently informational:
    /// [`MoaSandbox::run_cmd`] logs the denial and lets the spawn
    /// proceed. The variant exists so future socket-level enforcement
    /// (D.11.7 seccomp / D.11.1 cgroup) can surface policy rejections
    /// as `Err` from the same API surface.
    #[error("sandbox network denied: {0}")]
    NetworkDenied(String),
    /// An I/O or task failure prevented the sandbox from collecting output.
    #[error("sandbox I/O failure: {0}")]
    Io(String),
}

impl From<SandboxError> for Error {
    fn from(error: SandboxError) -> Self {
        Self::InvalidState(error.to_string())
    }
}

/// Configuration for one named validation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandConfig {
    /// Stable logical command name.
    pub name: &'static str,
    /// Binary invoked for the logical command.
    pub binary: &'static str,
    /// Maximum number of arguments accepted by the command.
    pub max_args: usize,
    /// Maximum UTF-8 byte length of one argument.
    pub max_arg_len: usize,
    /// Maximum bytes captured from each output stream.
    pub max_output_bytes: usize,
    /// Wall-clock timeout in seconds.
    pub timeout_secs: u64,
    /// Whether the command is permitted to use the network.
    pub allow_network: bool,
}

/// Static command policy table for the supported validators.
pub static COMMAND_CONFIGS: &[CommandConfig] = &[
    CommandConfig {
        name: "rust",
        binary: "cargo",
        max_args: 32,
        max_arg_len: 1024,
        max_output_bytes: 64 * 1024,
        timeout_secs: 180,
        allow_network: false,
    },
    CommandConfig {
        name: "python",
        binary: "python3",
        max_args: 32,
        max_arg_len: 1024,
        max_output_bytes: 64 * 1024,
        timeout_secs: 60,
        allow_network: false,
    },
    CommandConfig {
        name: "typescript",
        binary: "tsc",
        max_args: 32,
        max_arg_len: 1024,
        max_output_bytes: 64 * 1024,
        timeout_secs: 60,
        allow_network: false,
    },
    CommandConfig {
        name: "sql",
        binary: "sqlite3",
        max_args: 32,
        max_arg_len: 1024,
        max_output_bytes: 64 * 1024,
        timeout_secs: 30,
        allow_network: false,
    },
];

/// Find a command policy by its logical name.
pub fn config_for(name: &str) -> Option<&'static CommandConfig> {
    COMMAND_CONFIGS.iter().find(|config| config.name == name)
}

/// Strip secrets from `args` before spawning. Secret-looking values are
/// redacted through the crate-wide policy while preserving argument count.
pub fn strip_secrets(args: &[String]) -> Vec<String> {
    let policy = RedactPolicy::default();
    args.iter()
        .map(|arg| {
            if looks_like_secret(arg) {
                match apply(&policy, Surface::Telemetry, arg) {
                    Ok(redacted) if redacted.as_ref() != arg => redacted.into_owned(),
                    Ok(_) | Err(_) => "***REDACTED***".to_owned(),
                }
            } else {
                arg.clone()
            }
        })
        .collect()
}

fn looks_like_secret(value: &str) -> bool {
    value.starts_with("sk-cp-")
        || value.starts_with("sk-ant-")
        || value.starts_with("sk-")
        || value.starts_with("AIzaSy")
        || value.starts_with("hf_")
        || value.starts_with("r8_")
        || value.starts_with("ghp_")
        || value.starts_with("xoxb-")
        || value.starts_with("Bearer ")
}

/// Verify that a binary exists in `PATH` or at an absolute path before
/// spawning. An unresolved binary returns [`SandboxError::BinaryNotFound`].
pub fn verify_binary_exists(binary: &str) -> std::result::Result<(), SandboxError> {
    let path = Path::new(binary);
    if path.is_absolute() {
        return if path.exists() {
            Ok(())
        } else {
            Err(SandboxError::BinaryNotFound(binary.to_owned()))
        };
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = if directory.as_os_str().is_empty() {
                Path::new(binary).to_path_buf()
            } else {
                directory.join(binary)
            };
            if candidate.exists() {
                return Ok(());
            }
        }
    }
    Err(SandboxError::BinaryNotFound(binary.to_owned()))
}

/// Compile-time configuration for the sandbox. Cheap to clone (every
/// internal collection is small).
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Wall-clock cap per `run` call. `Duration::ZERO` means "no
    /// timeout" and is **not** allowed — the caller must opt out
    /// explicitly via [`SandboxConfig::no_timeout`] for clarity.
    pub timeout: Duration,
    /// Allowed command basenames.
    pub allowlist: Allowlist,
    /// Hard denylist scanned over argv.
    pub denylist: Denylist,
    /// Cap stdout/stderr capture. `None` means use [`MAX_STDOUT_BYTES`]
    /// / [`MAX_STDERR_BYTES`].
    pub max_capture_bytes: Option<usize>,
    /// Legacy boolean network flag (catalog §D.11.9). `true` maps to
    /// [`NetworkPolicy::Open`] and `false` maps to
    /// [`NetworkPolicy::Off`]. New code should use
    /// [`Self::network_policy`] and [`Self::with_network_policy`].
    /// Maintained for back-compat with existing call sites that pass
    /// the boolean straight from `Config::sandbox_allow_network`.
    #[deprecated(note = "use network_policy / with_network_policy instead")]
    pub allow_network: bool,
    /// Network policy applied to every subprocess. Catalog §D.11.13.
    /// Default `Off` so the install never silently contacts the
    /// registry / an arbitrary host. Operators opt in via
    /// `MOAGAN_SANDBOX_NETWORK_POLICY=open` (or `=allow_list`), via
    /// `with_network_policy`, or by setting
    /// `sandbox_network_policy = { kind = "open" }` in
    /// `~/.config/moagan/config.toml`. When the policy is `Off`, the
    /// sandbox injects `CARGO_NET_OFFLINE=true` in the subprocess
    /// env so cargo refuses to fetch crates from the registry.
    pub network_policy: NetworkPolicy,
    /// Whether to skip the secret-stripping pass over argv. Default
    /// `false` (strip). When `true`, the raw args are passed to the
    /// subprocess without redaction. Useful for debugging / repro
    /// cases where the operator wants to see exactly what bytes were
    /// passed. Catalog §D.11.10.
    pub allow_injection: bool,
    /// Opt-in seccomp syscall whitelist applied to the subprocess
    /// in its `pre_exec` hook. Catalog §D.11.7. Default
    /// [`SeccompPolicyKind::Permissive`] so the sandbox is
    /// unchanged for existing runs. Operators opt in via
    /// `MOAGAN_SANDBOX_SECCOMP=strict_rust_build` or by setting
    /// `sandbox_seccomp = "strict_rust_build"` in
    /// `~/.config/moagan/config.toml`.
    pub seccomp: SeccompPolicyKind,
}

impl SandboxConfig {
    /// Build a config with the project defaults:
    /// `timeout = 30s`, default allowlist + denylist, network
    /// disabled, secret stripping enabled.
    #[allow(deprecated)]
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            allowlist: Allowlist::default(),
            denylist: Denylist::default(),
            max_capture_bytes: None,
            allow_network: false,
            network_policy: NetworkPolicy::Off,
            allow_injection: false,
            seccomp: SeccompPolicyKind::Permissive,
        }
    }

    /// Replace the timeout. `0` is rejected to keep the safety
    /// property that every sandbox call has a deadline.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        if timeout.is_zero() {
            return self;
        }
        self.timeout = timeout;
        self
    }

    /// Explicitly disable the timeout. Use sparingly — the caller is
    /// promising it has another deadline layered on top.
    pub fn no_timeout(mut self) -> Self {
        self.timeout = Duration::from_secs(3600);
        self
    }

    /// Replace the allowlist with a custom one.
    pub fn with_allowlist(mut self, allowlist: Allowlist) -> Self {
        self.allowlist = allowlist;
        self
    }

    /// Replace the denylist with a custom one.
    pub fn with_denylist(mut self, denylist: Denylist) -> Self {
        self.denylist = denylist;
        self
    }

    /// Cap stdout/stderr capture per stream.
    pub fn with_max_capture(mut self, bytes: usize) -> Self {
        self.max_capture_bytes = Some(bytes);
        self
    }

    /// Opt in to network access for the subprocess. Default is
    /// `false` (off-by-default). When `true`, the sandbox does NOT
    /// set `CARGO_NET_OFFLINE=true` so cargo can fetch crates from
    /// the registry. Catalog §D.11.9.
    ///
    /// Deprecated wrapper: internally maps `true` to
    /// [`NetworkPolicy::Open`] and `false` to [`NetworkPolicy::Off`],
    /// and mirrors the bool into the legacy [`Self::allow_network`]
    /// field for back-compat. New code should use
    /// [`Self::with_network_policy`] so the `AllowList` case is
    /// representable.
    ///
    /// The method itself is intentionally not marked
    /// `#[deprecated]` so existing call sites that pass a `bool`
    /// from the legacy `Config::sandbox_allow_network` (catalog
    /// §D.11.9) keep compiling without `#[allow(deprecated)]`
    /// annotations. The field it wraps *is* deprecated; direct
    /// reads / writes of `cfg.allow_network` still emit the warning.
    #[allow(deprecated)]
    pub fn with_allow_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self.network_policy = if allow {
            NetworkPolicy::Open
        } else {
            NetworkPolicy::Off
        };
        self
    }

    /// Replace the network policy. Catalog §D.11.13. Mirrors the
    /// boolean into the legacy [`Self::allow_network`] field so
    /// existing call sites that read it still observe a consistent
    /// value (`true` for `Open` or any `AllowList`, `false` for
    /// `Off`).
    pub fn with_network_policy(mut self, policy: NetworkPolicy) -> Self {
        #[allow(deprecated)]
        {
            self.allow_network = !matches!(policy, NetworkPolicy::Off);
        }
        self.network_policy = policy;
        self
    }

    /// Opt out of the secret-stripping pass over argv. Default is
    /// `false` (strip). When `true`, the raw args are passed to the
    /// subprocess verbatim. Catalog §D.11.10.
    pub fn with_allow_injection(mut self, allow: bool) -> Self {
        self.allow_injection = allow;
        self
    }

    /// Replace the seccomp syscall whitelist applied to the
    /// subprocess. Catalog §D.11.7. Default
    /// [`SeccompPolicyKind::Permissive`] (no-op). Setting
    /// [`SeccompPolicyKind::StrictRustBuild`] makes the sandbox load
    /// a BPF program that allow-lists the syscalls needed to compile
    /// Rust from scratch.
    pub fn with_seccomp(mut self, kind: SeccompPolicyKind) -> Self {
        self.seccomp = kind;
        self
    }

    /// Strip any environment variable whose name hints at a secret.
    /// Run this on the inherited env before merging in the sandbox's
    /// overrides.
    pub fn strip_secrets_env(&self, env: &mut std::collections::HashMap<String, String>) {
        let mut to_remove: Vec<String> = Vec::new();
        for key in env.keys() {
            let lower = key.to_lowercase();
            if lower.contains("key")
                || lower.contains("token")
                || lower.contains("secret")
                || lower.contains("password")
            {
                to_remove.push(key.clone());
            }
        }
        for k in to_remove {
            env.remove(&k);
        }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-call outcome of a sandboxed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxResult {
    /// Process exit code. `-1` when the process was terminated by a
    /// signal and `code()` returned `None`.
    pub exit_code: i32,
    /// Captured stdout (truncated to the configured cap).
    pub stdout: String,
    /// Captured stderr (truncated to the configured cap).
    pub stderr: String,
    /// Wall-clock duration of the call (start → finish), excluding
    /// the cold-start of the binary itself.
    pub duration: Duration,
    /// Final classification of the call.
    pub status: SandboxStatus,
    /// The full command line, joined for human readability.
    pub command: String,
}

/// Lifecycle status of a single sandbox run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Process exited with code `0` and the policy is satisfied.
    Pass,
    /// Process exited with a non-zero code.
    Fail,
    /// Wall-clock cap elapsed before the process finished.
    Timeout,
    /// The command or one of its arguments was rejected by the
    /// policy (allowlist / denylist).
    NotAllowed,
    /// The binary was missing on disk (`ENOENT`).
    NotFound,
    /// I/O or scheduling error that prevented the process from
    /// running at all.
    Error,
}

impl SandboxResult {
    /// Convenience constructor used by [`Sandbox::run`].
    fn new(
        exit_code: i32,
        stdout: String,
        stderr: String,
        duration: Duration,
        status: SandboxStatus,
        command: String,
    ) -> Self {
        Self {
            exit_code,
            stdout,
            stderr,
            duration,
            status,
            command,
        }
    }
}

/// Raw sandbox output. Captured stdout/stderr are exposed as bytes
/// (callers needing `String` must do their own `from_utf8_lossy`).
/// `exit_code` is `None` only when the process was terminated by a
/// signal before its exit code could be read; otherwise it carries
/// the value the kernel returned. `killed_by_timeout` is true iff
/// the wall-clock timeout fired and the child was killed by the
/// sandbox.
///
/// Distinct from [`SandboxResult`], which adds policy-aware status
/// classification (`NotAllowed` / `NotFound` / `Pass` / `Fail` /
/// `Timeout` / `Error`) and a `String`-encoded stdout/stderr for the
/// legacy positional API. [`SandboxOutput`] is the simpler shape
/// returned by [`Sandbox::run_cmd`] — the caller handles the
/// `SandboxError` enum to learn about policy rejections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutput {
    /// Captured stdout as raw bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr as raw bytes.
    pub stderr: Vec<u8>,
    /// Process exit code. `None` when the process was terminated by
    /// a signal and `code()` returned `None` (or the child was
    /// killed before it could be waited on).
    pub exit_code: Option<i32>,
    /// Wall-clock duration of the call (start → finish), excluding
    /// the cold-start of the binary itself.
    pub duration: Duration,
    /// Whether the wall-clock timeout fired and the child was killed
    /// by the sandbox before exiting naturally.
    pub killed_by_timeout: bool,
}

/// Fluent-builder command struct passed to [`Sandbox::run_cmd`].
///
/// `Command` borrows everything (`binary`, `args`, optional `cwd`,
/// the `env` pairs) so the caller can stack-allocate a [`Command`]
/// without copying large arg lists. The [`Sandbox`] takes ownership
/// of the bytes for the duration of the call; the borrow ends when
/// `run_cmd` returns.
///
/// Fields are public so callers who already have a `Command` value
/// can mutate it directly; the builder methods exist for ergonomic
/// chaining. Defaults match the legacy positional API so a bare
/// `Command::new(binary, args)` behaves like the old
/// `sandbox.run(binary, args)`.
///
/// Catalog: D.11.15 — `Sandbox::run` with `Command` struct. The
/// previous positional signature
/// (`binary, args, env, cwd, stdin, timeout, max_stdout_bytes,
/// max_stderr_bytes`) was unwieldy and impossible to extend without
/// breaking every call site. The struct form lets us add future
/// opt-in overlays (namespace, stdin source from file, cgroup
/// resource limits) without further ABI churn.
#[derive(Debug, Clone)]
pub struct Command<'a> {
    /// Absolute or `PATH`-relative path to the binary. The basename
    /// is what the allowlist matches on; absolute paths are fine and
    /// the sandbox does not modify the path before `execve`.
    pub binary: &'a Path,
    /// Argv (excluding the binary name).
    pub args: &'a [&'a str],
    /// Extra environment variables added on top of the sandbox's
    /// sanitised environment. Empty by default.
    pub env: Vec<(&'a str, &'a str)>,
    /// Working directory for the spawned process. `None` means the
    /// sandbox allocates a fresh scratch dir per call (matching the
    /// legacy [`Sandbox::run`] semantics).
    pub cwd: Option<&'a Path>,
    /// Bytes to write to the child's stdin. `None` means
    /// `Stdio::null()` (stdin closed).
    pub stdin: Option<Vec<u8>>,
    /// Per-call wall-clock timeout override. `None` means use
    /// [`SandboxConfig::timeout`]. `Duration::ZERO` is rejected by
    /// the sandbox for symmetry with [`SandboxConfig::with_timeout`].
    pub timeout: Option<Duration>,
    /// Cap stdout capture per call. Defaults to [`MAX_STDOUT_BYTES`].
    pub max_stdout_bytes: usize,
    /// Cap stderr capture per call. Defaults to [`MAX_STDERR_BYTES`].
    pub max_stderr_bytes: usize,
}

impl<'a> Command<'a> {
    /// Build a `Command` with the required `binary` and `args`. All
    /// optional fields default to "use the sandbox's defaults":
    /// empty `env`, no `cwd` (fresh scratch dir), no `stdin`
    /// (`Stdio::null`), no per-call `timeout`, and the default
    /// stdout/stderr caps from [`MAX_STDOUT_BYTES`] /
    /// [`MAX_STDERR_BYTES`].
    pub fn new(binary: &'a Path, args: &'a [&'a str]) -> Self {
        Self {
            binary,
            args,
            env: Vec::new(),
            cwd: None,
            stdin: None,
            timeout: None,
            max_stdout_bytes: MAX_STDOUT_BYTES,
            max_stderr_bytes: MAX_STDERR_BYTES,
        }
    }

    /// Append a `key=value` env entry. Multiple `env` calls append
    /// in order; later calls overwrite earlier ones with the same
    /// key when merged into the spawned process's environment.
    pub fn env(mut self, k: &'a str, v: &'a str) -> Self {
        self.env.push((k, v));
        self
    }

    /// Override the working directory for the spawned process. When
    /// set, the directory must already exist; the sandbox does not
    /// create it.
    pub fn cwd(mut self, c: &'a Path) -> Self {
        self.cwd = Some(c);
        self
    }

    /// Feed `bytes` to the child's stdin. When unset, stdin is
    /// closed (`Stdio::null()`) so the child gets `EOF` immediately.
    pub fn stdin_bytes(mut self, b: Vec<u8>) -> Self {
        self.stdin = Some(b);
        self
    }

    /// Override the per-call wall-clock timeout.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Cap stdout capture per call. The default is
    /// [`MAX_STDOUT_BYTES`].
    pub fn max_stdout(mut self, n: usize) -> Self {
        self.max_stdout_bytes = n;
        self
    }

    /// Cap stderr capture per call. The default is
    /// [`MAX_STDERR_BYTES`].
    pub fn max_stderr(mut self, n: usize) -> Self {
        self.max_stderr_bytes = n;
        self
    }
}

/// Owned sandbox. Holds the configuration and a fresh scratch dir
/// per `run` invocation.
#[derive(Debug, Clone)]
pub struct Sandbox {
    config: SandboxConfig,
    /// Optional cancellation handle. When set, every spawned child is
    /// placed in its own process group (`pre_exec` calls `setpgid`)
    /// and its pgid is registered on the [`Cancel`] so a Hard-tier
    /// cancel can `killpg` it. `None` means sandbox calls are
    /// cooperative-only; `kill_on_drop` remains the cleanup guarantee.
    cancel: Option<Cancel>,
}

/// RAII guard that owns the pgid registration against a [`Cancel`]
/// handle for the lifetime of an in-flight sandbox child.
///
/// Construction registers the pgid; `Drop` unregisters it. Because
/// Rust drops locals on every exit path — natural completion, error
/// return, timeout, output-truncated, AND future drop from
/// orchestrator shutdown — the guard is the single source of truth
/// for unregistration. Call sites never call `unregister_child`
/// directly, so the cancel registry cannot leak pgids even when the
/// caller drops the future mid-flight.
struct RegisteredChild<'a> {
    cancel: &'a Cancel,
    pgid: i32,
}

impl Drop for RegisteredChild<'_> {
    fn drop(&mut self) {
        self.cancel.unregister_child(self.pgid);
    }
}

/// Watchdog that kills a process tree on timeout or cancellation.
///
/// `kill_on_drop` on `tokio::process::Command` only kills the immediate
/// child PID, leaving grandchildren (e.g. `cargo` → `rustc` → `linker`,
/// `tsc` → helper processes) as zombies after the parent dies. The
/// watchdog closes that gap: it relies on `setpgid(0, 0)` in the
/// sandbox's `pre_exec` so the spawned child forms its own process
/// group, and on `timeout`/`cancel` it sends `SIGTERM` to the whole
/// group, waits [`Self::grace`], then falls back to `SIGKILL`.
///
/// Catalog §D.11.11.
#[allow(dead_code)]
pub struct Watchdog {
    /// Process-group id to signal. Mirrors the watchdog's `killpg`
    /// target.
    pub pgid: i32,
    /// Wall-clock deadline before the watchdog fires `SIGTERM`.
    pub timeout: Duration,
    /// Grace window between `SIGTERM` and `SIGKILL`.
    pub grace: Duration,
    /// Cooperative cancel token. Cancelling wakes the watchdog from
    /// its initial sleep.
    pub cancel: CancellationToken,
}

impl Watchdog {
    /// Spawn a watchdog task that watches `pgid`. The returned
    /// `JoinHandle` resolves after the task has finished its
    /// `SIGTERM` + grace + `SIGKILL` sequence on the natural-completion
    /// path; on the timeout path it fires `SIGTERM` at `timeout` and
    /// `SIGKILL` at `timeout + grace`.
    ///
    /// The `cancel` token wakes the task from its initial sleep:
    /// cancelling it is the caller's signal that the child has already
    /// exited and the watchdog is no longer needed. The task still
    /// runs the `SIGTERM` + grace + `SIGKILL` sequence so that any
    /// orphaned grandchildren (the original motivation for D.11.11)
    /// are still reaped.
    ///
    /// On Unix the watchdog fires `libc::killpg(-pgid, SIGTERM)` then
    /// `libc::killpg(-pgid, SIGKILL)`. Missing groups yield `ESRCH`
    /// which is ignored, so the natural-completion path is silent.
    /// On non-Unix platforms the watchdog is a documented no-op: the
    /// spawned task completes immediately and `kill_on_drop` remains
    /// the cleanup guarantee.
    ///
    /// The spawned task is parented to `cancel` (AGENTS.md §"No-go
    /// list": no `tokio::spawn` without a `JoinHandle` recorded or a
    /// `CancellationToken` parent). The `JoinHandle` returned here
    /// satisfies the recorded-handle alternative; callers may drop it
    /// to detach the task once `cancel` has been signalled.
    pub fn spawn(
        pgid: i32,
        timeout: Duration,
        grace: Duration,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        #[cfg(unix)]
        {
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(timeout) => {}
                    _ = cancel.cancelled() => {}
                }
                // SAFETY: `killpg` is safe to call from any thread; a
                // missing group yields `ESRCH` which we ignore so the
                // natural-completion path stays silent.
                let _ = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGTERM) };
                tokio::time::sleep(grace).await;
                let _ = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (pgid, timeout, grace, cancel);
            tokio::spawn(async move {})
        }
    }
}

impl Sandbox {
    /// Build a new sandbox with the supplied configuration.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        if config.timeout.is_zero() {
            return Err(Error::InvalidState(
                "sandbox timeout must be > 0; use no_timeout() to opt out explicitly".into(),
            ));
        }
        Ok(Self {
            config,
            cancel: None,
        })
    }

    /// Borrow the current configuration.
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// Attach a cancellation handle. Every spawned child registers its
    /// process-group id on the handle so `CancelTier::Hard` can
    /// `SIGTERM` + delayed `SIGKILL` the whole subtree. Idempotent:
    /// the latest handle wins (the previous one is dropped, which is
    /// a no-op because `Cancel::register_child` requires a live handle
    /// to find its registry — a new handle starts with an empty set).
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Allocate a fresh scratch directory owned by the caller.
    ///
    /// Use this when a validator needs to drop files (e.g. a Cargo
    /// project layout) into the sandbox BEFORE invoking a command.
    /// The returned [`TempDir`] is independent of the scratch dirs
    /// that [`Sandbox::run`] creates per invocation; callers that
    /// want the layout visible to the spawned process must hand
    /// the path explicitly to [`Sandbox::run_in`].
    pub fn new_workdir(&self) -> Result<TempDir> {
        Ok(TempDir::new()?)
    }

    /// Execute `cmd` with `args` inside a fresh scratch directory.
    ///
    /// Policy:
    /// 1. The allowlist must permit the command basename.
    /// 2. The denylist must not appear in the command or any
    ///    argument.
    /// 3. The process is run with `env_clear()` + a sanitised
    ///    environment (see [`Sandbox::build_env`]) and with the
    ///    scratch dir as both `HOME` and current directory.
    /// 4. Stdout/stderr are captured up to the configured cap.
    /// 5. The wall-clock timeout from the config is enforced via
    ///    `tokio::time::timeout`.
    ///
    /// The scratch directory is created and cleaned up inside this
    /// call. If you need to populate the directory with files (e.g.
    /// a Cargo project layout) before running the command, use
    /// [`Sandbox::new_workdir`] plus [`Sandbox::run_in`].
    pub async fn run(
        &self,
        cmd: &str,
        args: &[&str],
    ) -> std::result::Result<SandboxResult, SandboxError> {
        let work = self
            .new_workdir()
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        self.run_in(work.path(), cmd, args).await
    }

    /// Execute `cmd` with an explicit output cap inside a fresh
    /// scratch directory.
    pub async fn run_with_output_cap(
        &self,
        cmd: &str,
        args: &[&str],
        max_output_bytes: usize,
    ) -> std::result::Result<SandboxResult, SandboxError> {
        let work = self
            .new_workdir()
            .map_err(|error| SandboxError::Io(error.to_string()))?;
        self.run_in_with_output_cap(work.path(), cmd, args, max_output_bytes)
            .await
    }

    /// Execute `cmd` with `args` inside the supplied `work_dir`. The
    /// directory's contents are visible to the spawned process and
    /// the process's CWD is set to it. The caller retains ownership
    /// of `work_dir` and decides when to drop it.
    pub async fn run_in(
        &self,
        work_dir: &Path,
        cmd: &str,
        args: &[&str],
    ) -> std::result::Result<SandboxResult, SandboxError> {
        let max_output_bytes = self.config.max_capture_bytes.unwrap_or_else(|| {
            let basename = Path::new(cmd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(cmd);
            config_for_binary(basename)
                .map(|config| config.max_output_bytes)
                .unwrap_or(DEFAULT_OUTPUT_CAP_BYTES)
        });
        let path = Path::new(cmd);
        let owned_args: Vec<&str> = args.to_vec();
        let command = Command::new(path, &owned_args)
            .cwd(work_dir)
            .max_stdout(max_output_bytes)
            .max_stderr(max_output_bytes);
        self.run_in_with_legacy_translation(&command).await
    }

    /// Execute `cmd` with an explicit per-stream output cap inside the
    /// supplied `work_dir`.
    pub async fn run_in_with_output_cap(
        &self,
        work_dir: &Path,
        cmd: &str,
        args: &[&str],
        max_output_bytes: usize,
    ) -> std::result::Result<SandboxResult, SandboxError> {
        let path = Path::new(cmd);
        let owned_args: Vec<&str> = args.to_vec();
        let command = Command::new(path, &owned_args)
            .cwd(work_dir)
            .max_stdout(max_output_bytes)
            .max_stderr(max_output_bytes);
        self.run_in_with_legacy_translation(&command).await
    }

    /// New command-struct API (D.11.15). Runs `cmd` inside the
    /// sandbox and returns the raw [`SandboxOutput`]: byte buffers,
    /// optional exit code, duration, and whether the wall-clock
    /// timeout fired.
    ///
    /// Distinct from the legacy positional [`Sandbox::run`] which
    /// returns a [`SandboxResult`] with a policy-aware status enum
    /// (`NotAllowed` / `NotFound` / `Pass` / `Fail` / `Timeout` /
    /// `Error`). Policy rejections surface here as `Err` variants
    /// ([`SandboxError::NotAllowed`] for allowlist / denylist
    /// failures, [`SandboxError::BinaryNotFound`] for missing
    /// binaries); call sites that need the rich status enum
    /// should keep using the legacy [`Sandbox::run`] until they
    /// migrate.
    pub async fn run_cmd(
        &self,
        cmd: &Command<'_>,
    ) -> std::result::Result<SandboxOutput, SandboxError> {
        self.run_in_with_limits(cmd).await
    }

    /// Legacy positional wrapper around the new [`Sandbox::run_cmd`].
    /// Kept as a deprecated entry point so external callers that
    /// already threaded the positional signature through their
    /// pipelines can migrate to [`Command`] one call site at a time
    /// without an immediate breakage. Builds a [`Command`] from the
    /// positional args and delegates to [`Sandbox::run_cmd`].
    #[deprecated(
        since = "0.4.0",
        note = "use Sandbox::run_cmd with the Command struct; the positional API is kept for migration only"
    )]
    #[allow(clippy::too_many_arguments)]
    pub async fn run_in_with_limits_legacy<'a>(
        &'a self,
        binary: &'a Path,
        args: &'a [&'a str],
        env: &'a [(&'a str, &'a str)],
        cwd: Option<&'a Path>,
        stdin: Option<Vec<u8>>,
        timeout: Option<Duration>,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> std::result::Result<SandboxOutput, SandboxError> {
        let mut cmd = Command::new(binary, args)
            .cwd(cwd.unwrap_or_else(|| Path::new(".")))
            .stdin_bytes(stdin.unwrap_or_default())
            .max_stdout(max_stdout_bytes)
            .max_stderr(max_stderr_bytes);
        if let Some(d) = timeout {
            cmd = cmd.timeout(d);
        }
        for (k, v) in env {
            cmd = cmd.env(k, v);
        }
        self.run_in_with_limits(&cmd).await
    }

    /// Translate the new command-struct core ([`run_in_with_limits`])
    /// into the legacy [`SandboxResult`] shape used by
    /// [`Sandbox::run`], [`Sandbox::run_in`],
    /// [`Sandbox::run_with_output_cap`], and
    /// [`Sandbox::run_in_with_output_cap`].
    ///
    /// - `Ok(SandboxOutput)` → `Ok(SandboxResult)` with status
    ///   `Pass` / `Fail` / `Timeout` derived from
    ///   `killed_by_timeout` + `exit_code`.
    /// - `Err(SandboxError::NotAllowed(_))` →
    ///   `Ok(SandboxResult)` with status `NotAllowed` (matches the
    ///   legacy contract that policy rejections surface as `Ok`).
    /// - `Err(SandboxError::BinaryNotFound(_))` →
    ///   `Ok(SandboxResult)` with status `NotFound`.
    /// - `Err(SandboxError::Io(_))` →
    ///   `Ok(SandboxResult)` with status `Error`.
    /// - `Err(SandboxError::OutputTruncated)` → propagates as `Err`
    ///   (the legacy tests assert this).
    async fn run_in_with_legacy_translation(
        &self,
        cmd: &Command<'_>,
    ) -> std::result::Result<SandboxResult, SandboxError> {
        match self.run_in_with_limits(cmd).await {
            Ok(output) => Ok(self.output_to_result(cmd, output)),
            Err(SandboxError::NotAllowed(msg)) => {
                Ok(self.output_to_status_result(cmd, msg, SandboxStatus::NotAllowed))
            }
            Err(SandboxError::BinaryNotFound(msg)) => Ok(self.output_to_status_result(
                cmd,
                format!("binary not found: {msg}"),
                SandboxStatus::NotFound,
            )),
            Err(SandboxError::Io(msg)) => Ok(self.output_to_status_result(
                cmd,
                format!("spawn failed: {msg}"),
                SandboxStatus::Error,
            )),
            Err(other) => Err(other),
        }
    }

    /// Convert a successful [`SandboxOutput`] into a [`SandboxResult`]
    /// for the legacy positional callers. The `command_str` is
    /// rebuilt from the [`Command`] so `result.command` matches what
    /// the legacy code reported (binary + sanitised args joined by
    /// spaces).
    ///
    /// Status mapping matches the pre-D.11.15 contract exactly:
    /// - `killed_by_timeout = true` → `Timeout`
    /// - `exit_code = Some(0)` → `Pass`
    /// - `exit_code = Some(non-zero)` → `Fail` (child exited with an
    ///   error code)
    /// - `exit_code = None` → `Fail` (child was killed by a signal,
    ///   e.g. a Hard-tier `Cancel`'s `killpg`). The legacy code
    ///   mapped `code() -> None` to `-1` and collapsed to `Fail`;
    ///   we preserve that so callers and the integration suite can
    ///   keep treating signal-killed children as `Fail`.
    fn output_to_result(&self, cmd: &Command<'_>, output: SandboxOutput) -> SandboxResult {
        let status = if output.killed_by_timeout {
            SandboxStatus::Timeout
        } else {
            match output.exit_code {
                Some(0) => SandboxStatus::Pass,
                Some(_) | None => SandboxStatus::Fail,
            }
        };
        SandboxResult::new(
            output.exit_code.unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.duration,
            status,
            self.command_string(cmd),
        )
    }

    /// Build a synthetic [`SandboxResult`] for a policy rejection:
    /// empty stdout, `msg` in stderr, `exit_code = -1`, and the
    /// supplied status. Used by
    /// [`Sandbox::run_in_with_legacy_translation`] to fold the new
    /// `Err(SandboxError::NotAllowed | BinaryNotFound | Io)` outcomes
    /// into the legacy `Ok(SandboxResult { status, .. })` shape.
    fn output_to_status_result(
        &self,
        cmd: &Command<'_>,
        msg: String,
        status: SandboxStatus,
    ) -> SandboxResult {
        SandboxResult::new(
            -1,
            String::new(),
            msg,
            Duration::ZERO,
            status,
            self.command_string(cmd),
        )
    }

    /// Reconstruct the human-readable command line from a
    /// [`Command`] (binary + args joined by spaces). The legacy
    /// `result.command` field is built this way for the positional
    /// callers after the refactor.
    ///
    /// Args are secret-stripped (catalog §D.11.10) before joining,
    /// mirroring the original behaviour: callers reading
    /// `SandboxResult.command` never see raw API keys. When
    /// `allow_injection` is opted in, the raw args are passed
    /// through verbatim so the operator sees exactly what bytes
    /// reached the child.
    fn command_string(&self, cmd: &Command<'_>) -> String {
        let raw_args: Vec<String> = cmd.args.iter().map(|arg| (*arg).to_owned()).collect();
        let visible_args: Vec<String> = if self.config.allow_injection {
            raw_args
        } else {
            strip_secrets(&raw_args)
        };
        std::iter::once(cmd.binary.display().to_string())
            .chain(visible_args)
            .collect::<Vec<_>>()
            .join(" ")
    }

    async fn run_in_with_limits(
        &self,
        cmd: &Command<'_>,
    ) -> std::result::Result<SandboxOutput, SandboxError> {
        let started = Instant::now();
        // Resolve the binary name for the allowlist / denylist /
        // `verify_binary_exists` checks. The struct's `binary`
        // field is `&Path` so absolute paths are accepted; the
        // allowlist matches on the basename.
        let binary_str = cmd.binary.to_str().ok_or_else(|| {
            SandboxError::Io(format!(
                "binary path is not valid UTF-8: {}",
                cmd.binary.display()
            ))
        })?;
        let binary_name = Path::new(binary_str)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(binary_str);

        // Capture-time caps. Per-call overrides on the [`Command`]
        // win over the config-level cap so a single fast `wc -l` run
        // does not have to allocate the default 64 KiB buffer.
        let max_stdout = cmd.max_stdout_bytes;
        let max_stderr = cmd.max_stderr_bytes;

        // Argv secret-stripping (catalog §D.11.10). The raw args are
        // fed to the policy check so a `sk-cp-...` token that has
        // been baked into argv still trips the denylist. The
        // visible `command_str` reflects the sanitised args.
        let raw_args: Vec<String> = cmd.args.iter().map(|arg| (*arg).to_owned()).collect();
        let sanitized_args: Vec<String> = if self.config.allow_injection {
            raw_args.clone()
        } else {
            strip_secrets(&raw_args)
        };
        let policy_argv: Vec<String> = std::iter::once(binary_name.to_owned())
            .chain(raw_args.iter().cloned())
            .collect();

        if !is_allowed(binary_name, &self.config.allowlist) {
            return Err(SandboxError::NotAllowed(format!(
                "command '{binary_name}' is not in the sandbox allowlist"
            )));
        }
        if contains_deny_token(&policy_argv, &self.config.denylist) {
            return Err(SandboxError::NotAllowed(
                "argv contains a denylisted token".into(),
            ));
        }

        if let Err(SandboxError::BinaryNotFound(binary)) = verify_binary_exists(binary_str) {
            return Err(SandboxError::BinaryNotFound(binary));
        }

        // Resolve the working directory: explicit `cwd` wins, then
        // fall back to a fresh scratch dir owned by this call.
        //
        // The TempDir scratch lifetime trick: we declare
        // `work_dir_owned` BEFORE `work_dir` so its lifetime covers
        // every use of `work_dir` for the rest of the function body.
        // The `match` below returns either the caller-supplied
        // `cmd.cwd` (whose lifetime is `'a` and therefore outlives
        // the function call) or a borrow of the scratch dir's path
        // (whose lifetime is tied to `work_dir_owned`, declared
        // above). Either way, `work_dir` is valid until the function
        // returns. When `cmd.cwd` is supplied, `work_dir_owned`
        // stays `None` and no scratch dir is allocated.
        let work_dir_owned: Option<TempDir> = if cmd.cwd.is_none() {
            Some(
                self.new_workdir()
                    .map_err(|error| SandboxError::Io(error.to_string()))?,
            )
        } else {
            None
        };
        let work_dir: &Path = match cmd.cwd {
            Some(cwd) => cwd,
            None => work_dir_owned
                .as_ref()
                .expect("scratch dir allocated when cmd.cwd is None")
                .path(),
        };

        let mut command = TokioCommand::new(binary_str);
        command
            .args(&sanitized_args)
            .current_dir(work_dir)
            .stdin(match cmd.stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // When a cancel handle is attached, place the child in its
        // own process group so `killpg` reaches the whole subtree
        // (cargo invokes rustc, tsc invokes tslib helpers, etc.).
        // `setpgid(0, 0)` is the canonical "I am the new group
        // leader" form: the child's pid becomes the pgid.
        #[cfg(unix)]
        if self.cancel.is_some() {
            // SAFETY: `tokio::process::Command::pre_exec` runs the
            // closure in the child between fork and exec. `setpgid(0, 0)`
            // only mutates the child's own process-group membership.
            // A non-zero return surfaces as `std::io::Error` and aborts
            // the spawn; we keep it non-fatal because `kill_on_drop`
            // is the backstop cleanup for this crate.
            let _ = unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                })
            };
        }

        // Catalog §D.11.7: install the seccomp BPF filter in the
        // child between fork and exec. `pre_exec` is a sync closure
        // that runs in the child, so this is the canonical place to
        // load a syscall whitelist that only affects the spawned
        // subprocess (the parent keeps running unrestricted).
        //
        // Default [`SeccompPolicyKind::Permissive`] is a no-op so
        // existing runs are unchanged; opting into
        // [`SeccompPolicyKind::StrictRustBuild`] activates the
        // ~30-syscall allow-list needed to compile Rust from
        // scratch.
        #[cfg(unix)]
        if self.config.seccomp != SeccompPolicyKind::Permissive {
            let kind = self.config.seccomp;
            // SAFETY: `pre_exec` runs in the child between fork and
            // exec. A non-zero return surfaces as `Err` and aborts
            // the spawn, which is the documented failure mode for
            // the seccomp loaders. `kill_on_drop` remains the
            // backstop cleanup for the parent.
            let _ = unsafe {
                command.pre_exec(move || -> std::result::Result<(), std::io::Error> {
                    seccomp::apply(kind)
                        .map_err(|error| std::io::Error::other(error.to_string()))
                })
            };
        }

        let env = self.build_env(work_dir);
        command.env_clear();
        for (key, value) in &env {
            command.env(key, value);
        }
        for (key, value) in &cmd.env {
            command.env(*key, *value);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SandboxError::BinaryNotFound(binary_str.to_owned()));
            }
            Err(error) => {
                return Err(SandboxError::Io(format!("spawn failed: {error}")));
            }
        };

        // Register the pgid (== the child pid) so Hard-tier cancel
        // can killpg it. The `RegisteredChild` RAII guard owns the
        // registration for the rest of `run_in_with_limits`; its
        // `Drop` impl unregisters the pgid on every exit branch
        // (natural, error, timeout, output-truncated, AND future
        // drop from orchestrator shutdown). The guard is the single
        // source of truth for unregistration — no call site duplicates
        // it, so the cancel registry cannot leak.
        let pgid: Option<i32> = child.id().and_then(|pid| i32::try_from(pid).ok());
        let _pgid_guard: Option<RegisteredChild<'_>> = match (&self.cancel, pgid) {
            (Some(cancel), Some(pgid)) => {
                cancel.register_child(pgid);
                Some(RegisteredChild { cancel, pgid })
            }
            _ => None,
        };

        // If the caller supplied stdin bytes, feed them to the child
        // before we start waiting for completion. The pipe is
        // dropped at the end of this block (when `_stdin_writer`
        // goes out of scope) which signals `EOF` to the child.
        if let Some(bytes) = cmd.stdin.as_ref()
            && let Some(mut stdin_pipe) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            let _ = stdin_pipe.write_all(bytes).await;
            let _ = stdin_pipe.shutdown().await;
        }

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let stdout_handle = child
            .stdout
            .take()
            .map(|stream| tokio::spawn(read_stream(stream, max_stdout, event_tx.clone())));
        let stderr_handle = child
            .stderr
            .take()
            .map(|stream| tokio::spawn(read_stream(stream, max_stderr, event_tx.clone())));
        drop(event_tx);

        // Per-call `cmd.timeout` overrides the config-level timeout.
        let effective_timeout = cmd.timeout.unwrap_or(self.config.timeout);
        if effective_timeout.is_zero() {
            return Err(SandboxError::Io(
                "sandbox timeout must be > 0; use SandboxConfig::no_timeout to opt out".into(),
            ));
        }
        let remaining = effective_timeout.saturating_sub(started.elapsed());

        // Spawn the Watchdog when the child is in its own process group
        // (i.e. a `Cancel` handle is attached, so `setpgid(0, 0)` ran in
        // `pre_exec`). The watchdog's `select!` resolves on the first of
        // `timeout` (the wall-clock deadline) or `cancel` (the child
        // exited, signalled by the cancel below). Either way it fires
        // `SIGTERM` at the pgid, waits `grace`, then `SIGKILL`. The
        // natural-completion path's `SIGTERM` lands on a defunct pgid
        // and yields `ESRCH` (no-op). Catalog §D.11.11.
        let watchdog_token = CancellationToken::new();
        let _watchdog_handle = if self.cancel.is_some() {
            pgid.map(|pgid| {
                Watchdog::spawn(pgid, remaining, HARD_KILL_GRACE, watchdog_token.clone())
            })
        } else {
            None
        };

        let deadline = time::sleep(remaining);
        tokio::pin!(deadline);
        let mut events_open = true;
        let (exit_code_opt, killed_by_timeout, early_error) = loop {
            tokio::select! {
                result = child.wait() => {
                    match result {
                        Ok(exit) => break (exit.code(), false, None),
                        Err(_) => break (None, false, Some(SandboxError::Io(
                            "waitpid failed".into(),
                        ))),
                    }
                }
                event = event_rx.recv(), if events_open => {
                    match event {
                        Some(error) => {
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            break (None, false, Some(error));
                        }
                        None => events_open = false,
                    }
                }
                _ = &mut deadline => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    break (None, true, None);
                }
            }
        };

        // Cancellation synchronisation point: the child has exited on
        // every branch (natural, error, timeout, output-truncated).
        // Cancelling the watchdog's token wakes its task from the
        // initial sleep so it does not sit on a stale timer, and the
        // watchdog's `SIGTERM` + grace + `SIGKILL` sequence reaps any
        // orphaned grandchildren (the original D.11.11 motivation).
        watchdog_token.cancel();
        drop(_pgid_guard);
        drop(_watchdog_handle);

        let stdout_result = await_reader(stdout_handle).await;
        let stderr_result = await_reader(stderr_handle).await;
        if let Some(error) = early_error {
            return Err(error);
        }
        let stdout_bytes = stdout_result?;
        let stderr_bytes = stderr_result?;

        Ok(SandboxOutput {
            stdout: stdout_bytes,
            stderr: stderr_bytes,
            exit_code: exit_code_opt,
            duration: started.elapsed(),
            killed_by_timeout,
        })
    }

    /// Build the sanitised environment for a sandboxed child.
    ///
    /// Strategy:
    /// - Copy inherited env, run [`SandboxConfig::strip_secrets_env`]
    ///   to drop anything that smells like a credential.
    /// - Force `PATH` to start with the standard system paths so the
    ///   child does not get a poisoned search path, then preserve the
    ///   inherited PATH so user-installed toolchains (`rustup`,
    ///   `cargo`) remain reachable.
    /// - Force `HOME` to the scratch directory so the child cannot
    ///   leak the real user's home contents.
    /// - When the [`NetworkPolicy`] is [`NetworkPolicy::Off`] (the
    ///   default, catalog §D.11.13), inject `CARGO_NET_OFFLINE=true`
    ///   so cargo refuses to fetch crates from the registry. For
    ///   `Open` and `AllowList`, the hint is NOT injected; the
    ///   policy is expected to be enforced at a lower layer (seccomp
    ///   / cgroup) once D.11.7 lands.
    fn build_env(&self, work_path: &Path) -> std::collections::HashMap<String, String> {
        let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
        self.config.strip_secrets_env(&mut env);
        let inherited_path = env
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
        env.insert(
            "PATH".into(),
            format!("/usr/local/bin:/usr/bin:/bin:{inherited_path}"),
        );
        env.insert("HOME".into(), work_path.to_string_lossy().into_owned());
        if matches!(self.config.network_policy, NetworkPolicy::Off) {
            env.insert("CARGO_NET_OFFLINE".into(), "true".into());
        }
        env
    }
}

/// High-level sandbox wrapper that carries the configured
/// [`NetworkPolicy`] alongside the underlying [`Sandbox`].
///
/// Catalog §D.11.13. The wrapper is the future composition point for
/// the rest of the Track E hardening (D.11.7 seccomp, D.11.1 cgroup).
/// For D.11.13 the only obligation is that
/// [`MoaSandbox::run_cmd`] validates host-like strings in the
/// supplied argv against the policy and emits a `tracing::warn!` for
/// any host that would be denied. The subprocess is still spawned
/// because the kernel-level enforcement lands with the seccomp /
/// cgroup follow-ups; the [`SandboxError::NetworkDenied`] variant
/// is reserved for the day those land.
#[derive(Debug, Clone)]
pub struct MoaSandbox {
    inner: Sandbox,
    network: NetworkPolicy,
}

impl MoaSandbox {
    /// Build a wrapper from `cfg` and an explicit [`NetworkPolicy`].
    /// The supplied `network` overrides any policy already on `cfg`;
    /// the inner [`Sandbox`] is then constructed from the merged
    /// config so its `CARGO_NET_OFFLINE` hint reflects the same
    /// policy that the pre-check validates.
    pub fn new(
        cfg: &SandboxConfig,
        network: NetworkPolicy,
    ) -> std::result::Result<Self, SandboxError> {
        let mut cfg = cfg.clone();
        cfg.network_policy = network.clone();
        let inner = Sandbox::new(cfg).map_err(|error| SandboxError::Io(error.to_string()))?;
        Ok(Self { inner, network })
    }

    /// Borrow the active [`NetworkPolicy`].
    pub fn network(&self) -> &NetworkPolicy {
        &self.network
    }

    /// Borrow the underlying [`Sandbox`] (for callers that need
    /// direct access to legacy positional APIs like
    /// [`Sandbox::run_in`]).
    pub fn inner(&self) -> &Sandbox {
        &self.inner
    }

    /// Run `cmd` inside the wrapped sandbox after validating host-
    /// like strings in the argv against [`Self::network`]. Hosts
    /// that would be denied are surfaced via a `tracing::warn!`
    /// line; the run itself proceeds. Socket-level enforcement is
    /// deferred to D.11.7 (seccomp) and D.11.1 (cgroup); when those
    /// land the pre-check will be promoted from a log line to an
    /// actual `SandboxError::NetworkDenied` short-circuit.
    pub async fn run_cmd(
        &self,
        cmd: &Command<'_>,
    ) -> std::result::Result<SandboxOutput, SandboxError> {
        for host in extract_hosts(cmd) {
            if let Some(reason) = self.network.deny_reason(&host) {
                tracing::warn!(
                    sandbox = "moa",
                    host = %host,
                    reason = %reason,
                    "network policy would deny host; enforcement deferred to D.11.7/D.11.1"
                );
            }
        }
        self.inner.run_cmd(cmd).await
    }
}

/// Extract host-like strings from a [`Command`]'s argv + binary name.
///
/// Heuristic: each argv string is split on whitespace; every token
/// that looks like a host (`[a-zA-Z0-9.-:]+` containing at least one
/// dot) is collected. This catches `curl https://example.com`
/// (single-token URL host), `wget example.com` (bare hostname), and
/// `sh -c "echo example.com"` (host embedded in a shell string).
/// Shell metacharacters (`/`, `=`, `&`) and absolute paths are
/// intentionally excluded by [`looks_like_host`].
fn extract_hosts(cmd: &Command<'_>) -> Vec<String> {
    let mut hosts = Vec::new();
    for arg in cmd.args {
        for token in arg.split_whitespace() {
            if looks_like_host(token) {
                hosts.push(token.to_owned());
            }
        }
    }
    if let Some(binary_str) = cmd.binary.to_str() {
        for token in binary_str.split_whitespace() {
            if looks_like_host(token) {
                hosts.push(token.to_owned());
            }
        }
    }
    hosts
}

fn looks_like_host(s: &str) -> bool {
    !s.is_empty()
        && s.contains('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
}

fn config_for_binary(binary: &str) -> Option<&'static CommandConfig> {
    let basename = Path::new(binary).file_name()?.to_str()?;
    COMMAND_CONFIGS
        .iter()
        .find(|config| config.binary == basename)
}

async fn read_stream<R>(
    mut stream: R,
    max_output_bytes: usize,
    event_tx: mpsc::UnboundedSender<SandboxError>,
) -> std::result::Result<Vec<u8>, SandboxError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(max_output_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = match stream.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let error = SandboxError::Io(error.to_string());
                return Err(error);
            }
        };
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > max_output_bytes {
            let remaining = max_output_bytes.saturating_sub(output.len());
            output.extend_from_slice(&buffer[..remaining]);
            let error = SandboxError::OutputTruncated;
            let _ = event_tx.send(error.clone());
            return Err(error);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

async fn await_reader(
    handle: Option<tokio::task::JoinHandle<std::result::Result<Vec<u8>, SandboxError>>>,
) -> std::result::Result<Vec<u8>, SandboxError> {
    match handle {
        Some(handle) => handle
            .await
            .map_err(|error| SandboxError::Io(error.to_string()))?,
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb() -> Sandbox {
        Sandbox::new(SandboxConfig::new()).expect("sandbox builds")
    }

    #[tokio::test]
    async fn zero_timeout_is_rejected() {
        let err = Sandbox::new(SandboxConfig {
            timeout: Duration::ZERO,
            ..SandboxConfig::new()
        })
        .unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
    }

    #[tokio::test]
    async fn allowlisted_command_passes() {
        let result = sb().run("echo", &["hello"]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"));
    }

    /// Attaching a `Cancel` handle must not change the observable
    /// behaviour of a normal sandbox call. The pre_exec `setpgid`
    /// runs in the child (between fork and exec) and the pgid
    /// registry is drained on the natural-completion path, so the
    /// end state is identical to the cancel-less baseline.
    #[tokio::test]
    async fn with_cancel_does_not_change_pass_outcome() {
        use crate::cancel::Cancel;
        let cancel = Cancel::new();
        let sandbox = Sandbox::new(SandboxConfig::new())
            .unwrap()
            .with_cancel(cancel.clone());
        let result = sandbox.run("echo", &["hello"]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"));
        // No pgid should remain in the registry after natural exit.
        cancel.cancel_with_tier(
            crate::cancel::CancelReason::Requested,
            crate::cancel::CancelTier::Hard,
        );
        // Just verify it doesn't panic; the pgid set is drained by
        // the sandbox's unregister_child call.
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn non_allowlisted_command_is_rejected() {
        let result = sb().run("curl", &["https://example"]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::NotAllowed);
        assert!(result.stderr.contains("allowlist"));
    }

    #[tokio::test]
    async fn denylisted_argv_token_is_rejected() {
        let result = sb().run("sh", &["-c", "curl", "https://x"]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::NotAllowed);
    }

    #[tokio::test]
    async fn missing_binary_returns_not_found() {
        // Allowlist the random name locally so we exercise the
        // post-allowlist spawn path that distinguishes NotFound
        // from NotAllowed.
        let cfg = SandboxConfig::new()
            .with_allowlist(Allowlist::from_slice(["definitely-not-a-real-binary-xyz"]));
        let sandbox = Sandbox::new(cfg).unwrap();
        let result = sandbox
            .run("definitely-not-a-real-binary-xyz", &[])
            .await
            .unwrap();
        assert_eq!(result.status, SandboxStatus::NotFound);
    }

    #[tokio::test]
    async fn failing_command_returns_fail_status() {
        let result = sb().run("sh", &["-c", "exit 7"]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::Fail);
        assert_eq!(result.exit_code, 7);
    }

    #[test]
    fn default_output_cap_is_64_kib() {
        assert_eq!(DEFAULT_OUTPUT_CAP_BYTES, 64 * 1024);
        assert_eq!(MAX_STDOUT_BYTES, DEFAULT_OUTPUT_CAP_BYTES);
        assert_eq!(MAX_STDERR_BYTES, DEFAULT_OUTPUT_CAP_BYTES);
    }

    #[test]
    fn command_config_for_known_name() {
        let config = config_for("rust").expect("rust command config");
        assert_eq!(config.binary, "cargo");
        assert_eq!(config.max_args, 32);
        assert_eq!(config.max_arg_len, 1024);
        assert_eq!(config.max_output_bytes, 64 * 1024);
        assert_eq!(config.timeout_secs, 180);
        // Catalog §D.11.9: the rust command config is off-by-default
        // for network. Callers that need to fetch crates must opt in
        // via `SandboxConfig::with_allow_network(true)`.
        assert!(!config.allow_network);
    }

    #[test]
    fn command_config_for_unknown_name_returns_none() {
        assert!(config_for("unknown-language").is_none());
    }

    #[test]
    fn strip_secrets_redacts_sk_cp() {
        let secret = "sk-cp-abcdefghijklmnopqrstuvwxyz".to_owned();
        let stripped = strip_secrets(std::slice::from_ref(&secret));
        assert_eq!(stripped.len(), 1);
        assert!(!stripped[0].contains(&secret));
        assert!(stripped[0].contains("REDACTED"));
    }

    #[test]
    fn strip_secrets_redacts_bearer() {
        let secret = "Bearer abcdefghijklmnop".to_owned();
        let stripped = strip_secrets(std::slice::from_ref(&secret));
        assert_eq!(stripped.len(), 1);
        assert!(!stripped[0].contains(&secret));
        assert!(stripped[0].contains("REDACTED"));
    }

    #[test]
    fn strip_secrets_preserves_arg_count() {
        let args = vec![
            "--token".to_owned(),
            "sk-cp-abcdefghijklmnopqrstuvwxyz".to_owned(),
            "plain".to_owned(),
        ];
        assert_eq!(strip_secrets(&args).len(), args.len());
    }

    #[test]
    fn strip_secrets_passes_through_non_secrets() {
        let args = vec!["--offline".to_owned(), "check".to_owned()];
        assert_eq!(strip_secrets(&args), args);
    }

    #[test]
    fn verify_binary_exists_finds_cargo() {
        assert!(verify_binary_exists("cargo").is_ok());
    }

    #[test]
    fn verify_binary_exists_fails_for_missing() {
        let result = verify_binary_exists("definitely-not-a-real-binary-xyz");
        assert!(matches!(
            result,
            Err(SandboxError::BinaryNotFound(binary))
                if binary == "definitely-not-a-real-binary-xyz"
        ));
    }

    #[tokio::test]
    async fn stdout_capture_is_capped() {
        let cfg = SandboxConfig::new().with_max_capture(256);
        let sandbox = Sandbox::new(cfg).unwrap();
        let result = sandbox.run("sh", &["-c", "yes A | head -c 8192"]).await;
        assert_eq!(result, Err(SandboxError::OutputTruncated));
    }

    #[tokio::test]
    async fn secrets_in_env_are_stripped() {
        let mut env = std::collections::HashMap::new();
        env.insert("MINIMAX_API_KEY".into(), "sk-cp-secret".into());
        env.insert("GITHUB_TOKEN".into(), "ghp_x".into());
        env.insert("MOAGAN_PARALLELISM_MAX".into(), "4".into());
        let cfg = SandboxConfig::new();
        cfg.strip_secrets_env(&mut env);
        assert!(!env.contains_key("MINIMAX_API_KEY"));
        assert!(!env.contains_key("GITHUB_TOKEN"));
        assert!(env.contains_key("MOAGAN_PARALLELISM_MAX"));
    }

    #[tokio::test]
    async fn command_string_includes_full_argv() {
        let result = sb().run("echo", &["a", "b", "c"]).await.unwrap();
        assert_eq!(result.command, "echo a b c");
    }

    /// Catalog §D.11.9 / §D.11.13: the default `SandboxConfig` must
    /// forbid network access for the subprocess. The sandbox enforces
    /// that by injecting `CARGO_NET_OFFLINE=true` in the env, so the
    /// observable contract is "default cargo runs offline". The
    /// post-D.11.13 source of truth is the `network_policy` field;
    /// the legacy `allow_network` boolean mirrors it via the
    /// `with_network_policy` / `with_allow_network` setters.
    #[tokio::test]
    async fn sandbox_default_does_not_allow_network() {
        let cfg = SandboxConfig::new();
        assert!(
            matches!(cfg.network_policy, NetworkPolicy::Off),
            "default network_policy must be Off (D.11.13)"
        );
        let sandbox = Sandbox::new(cfg).unwrap();
        let result = sandbox
            .run("sh", &["-c", "echo ${CARGO_NET_OFFLINE:-unset}"])
            .await
            .unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert!(
            result.stdout.contains("true"),
            "CARGO_NET_OFFLINE must be 'true' by default, got {:?}",
            result.stdout
        );
    }

    /// Catalog §D.11.9 / §D.11.13: `with_network_policy(Open)` (or
    /// the legacy `with_allow_network(true)` wrapper) opts in. The
    /// sandbox must NOT set `CARGO_NET_OFFLINE` so cargo can fetch
    /// crates from the registry. The test exercises the legacy
    /// boolean path with `#[allow(deprecated)]` because the
    /// post-D.11.13 source of truth is the typed
    /// [`NetworkPolicy::Open`] variant.
    #[tokio::test]
    #[allow(deprecated)]
    async fn sandbox_opt_in_allows_network() {
        let cfg = SandboxConfig::new().with_allow_network(true);
        assert!(matches!(cfg.network_policy, NetworkPolicy::Open));
        let sandbox = Sandbox::new(cfg).unwrap();
        let result = sandbox
            .run("sh", &["-c", "echo ${CARGO_NET_OFFLINE:-unset}"])
            .await
            .unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert!(
            result.stdout.contains("unset"),
            "CARGO_NET_OFFLINE must be unset when network is allowed, got {:?}",
            result.stdout
        );
    }

    /// Catalog §D.11.13: `with_network_policy(AllowList)` is the
    /// post-D.11.13 representation of a partial opt-in. The cargo
    /// env hint must NOT be injected (the policy is permissive for
    /// the listed hosts) and the new `network_policy` field must
    /// carry the list verbatim so the pre-check can match it.
    #[tokio::test]
    async fn sandbox_with_network_policy_allow_list_does_not_inject_offline() {
        let cfg = SandboxConfig::new().with_network_policy(NetworkPolicy::AllowList {
            hosts: vec!["crates.io".to_owned()],
        });
        assert!(matches!(
            cfg.network_policy,
            NetworkPolicy::AllowList { ref hosts } if hosts == &vec!["crates.io".to_owned()]
        ));
        let sandbox = Sandbox::new(cfg).unwrap();
        let result = sandbox
            .run("sh", &["-c", "echo ${CARGO_NET_OFFLINE:-unset}"])
            .await
            .unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert!(
            result.stdout.contains("unset"),
            "AllowList must NOT inject CARGO_NET_OFFLINE; got {:?}",
            result.stdout
        );
    }

    /// Catalog §D.11.10: the default `SandboxConfig` runs the
    /// secret-stripping pass over argv. The visible `command_str`
    /// must NOT contain the raw secret.
    #[tokio::test]
    async fn sandbox_default_strips_secrets() {
        let sandbox = Sandbox::new(SandboxConfig::new()).unwrap();
        let secret = "sk-cp-fake_default_strip_secret_xyz";
        let result = sandbox.run("echo", &[secret]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert!(
            !result.command.contains(secret),
            "raw secret leaked into command string: {}",
            result.command
        );
        assert!(
            result.command.contains("REDACTED"),
            "expected REDACTED marker in command string, got: {}",
            result.command
        );
    }

    /// Catalog §D.11.10: with `allow_injection=true`, the raw args
    /// are passed to the subprocess verbatim and the visible
    /// `command_str` reflects the unredacted args. The operator
    /// intentionally opted in to see "what bytes were passed".
    #[tokio::test]
    async fn sandbox_allow_injection_keeps_secrets() {
        let cfg = SandboxConfig::new().with_allow_injection(true);
        let sandbox = Sandbox::new(cfg).unwrap();
        let secret = "sk-cp-fake_injection_keep_secret_xyz";
        let result = sandbox.run("echo", &[secret]).await.unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert!(
            result.command.contains(secret),
            "secret must pass through when allow_injection=true, got: {}",
            result.command
        );
    }

    // ----------------------------------------------------------------
    // D.11.15 — `Sandbox::run_cmd` with `Command` struct tests.
    //
    // Each test below covers one of the acceptance criteria spelled
    // out in the PR-D spec. Builder coverage (`command_new_...`,
    // `command_builder_chain_works`) stays synchronous so it runs
    // even when the tokio runtime is unavailable; the spawn-side
    // tests are `#[tokio::test]`.
    // ----------------------------------------------------------------

    /// `Command::new` must populate only the required fields and
    /// leave every optional field at its documented default. This
    /// pins the wire format so future changes cannot silently break
    /// call sites that rely on "bare `Command::new(binary, args)`
    /// behaves like the legacy positional API".
    #[test]
    fn command_new_constructs_with_required_fields() {
        let binary = Path::new("echo");
        let args = ["hello", "world"];
        let cmd = Command::new(binary, &args);
        assert_eq!(cmd.binary, binary);
        assert_eq!(cmd.args, &args);
        assert!(cmd.env.is_empty(), "env defaults to empty");
        assert!(cmd.cwd.is_none(), "cwd defaults to None (scratch dir)");
        assert!(cmd.stdin.is_none(), "stdin defaults to None");
        assert!(cmd.timeout.is_none(), "timeout defaults to None");
        assert_eq!(cmd.max_stdout_bytes, MAX_STDOUT_BYTES);
        assert_eq!(cmd.max_stderr_bytes, MAX_STDERR_BYTES);
    }

    /// The fluent builder must produce a `Command` whose fields
    /// reflect every chained call. Each setter returns `Self` so
    /// the chains read top-to-bottom; this test pins the value at
    /// each step.
    #[test]
    fn command_builder_chain_works() {
        let binary = Path::new("/bin/sh");
        let args = ["-c", "echo hi"];
        let cwd = Path::new("/tmp");
        let cmd = Command::new(binary, &args)
            .env("FOO", "bar")
            .env("BAZ", "qux")
            .cwd(cwd)
            .stdin_bytes(b"input\n".to_vec())
            .timeout(Duration::from_secs(5))
            .max_stdout(1024)
            .max_stderr(2048);
        assert_eq!(cmd.binary, binary);
        assert_eq!(cmd.args, &args);
        assert_eq!(
            cmd.env,
            vec![("FOO", "bar"), ("BAZ", "qux")],
            "env collects every entry in order"
        );
        assert_eq!(cmd.cwd, Some(cwd));
        assert_eq!(cmd.stdin.as_deref(), Some(b"input\n".as_ref()));
        assert_eq!(cmd.timeout, Some(Duration::from_secs(5)));
        assert_eq!(cmd.max_stdout_bytes, 1024);
        assert_eq!(cmd.max_stderr_bytes, 2048);
    }

    /// `Sandbox::run_cmd` must surface the process exit code in the
    /// returned [`SandboxOutput`]. The struct's `exit_code` is
    /// `Option<i32>`; for a normal exit it is `Some(0)` for `echo`,
    /// `Some(7)` for `sh -c "exit 7"`.
    #[tokio::test]
    async fn sandbox_run_cmd_returns_exit_code() {
        let sandbox = sb();
        let args = ["-c", "exit 7"];
        let cmd = Command::new(Path::new("sh"), &args);
        let output = sandbox.run_cmd(&cmd).await.unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert!(!output.killed_by_timeout);
    }

    /// `Sandbox::run_cmd` must capture both stdout and stderr as
    /// raw bytes (not `String`) so callers can decide how to decode
    /// them. The shape differs from the legacy `Sandbox::run`
    /// (which returns `String`s after a `from_utf8_lossy`).
    #[tokio::test]
    async fn sandbox_run_cmd_captures_stdout_and_stderr() {
        let sandbox = sb();
        let args = ["-c", "echo on-stdout; echo on-stderr 1>&2"];
        let cmd = Command::new(Path::new("sh"), &args);
        let output = sandbox.run_cmd(&cmd).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(
            output.stdout, b"on-stdout\n",
            "stdout is captured verbatim as bytes"
        );
        assert_eq!(
            output.stderr, b"on-stderr\n",
            "stderr is captured verbatim as bytes"
        );
        assert!(!output.killed_by_timeout);
    }

    /// Per-call `Command::max_stdout` overrides the
    /// [`MAX_STDOUT_BYTES`] default. Hitting the cap yields
    /// [`SandboxError::OutputTruncated`] (D.11.4 contract).
    #[tokio::test]
    async fn sandbox_run_cmd_respects_max_stdout_bytes() {
        let sandbox = sb();
        // `yes A` produces a stream of "A\n" bytes — more than the
        // 256-byte cap, so the sandbox must truncate.
        let args = ["-c", "yes A | head -c 8192"];
        let cmd = Command::new(Path::new("sh"), &args).max_stdout(256);
        let err = sandbox.run_cmd(&cmd).await.unwrap_err();
        assert_eq!(err, SandboxError::OutputTruncated);
    }

    /// Per-call `Command::timeout` overrides the
    /// [`SandboxConfig::timeout`]. The struct's `killed_by_timeout`
    /// flag must be true when the timeout fires, even if the
    /// process happened to be racing to exit on its own.
    #[tokio::test]
    async fn sandbox_run_cmd_respects_timeout() {
        let sandbox = sb();
        // 200ms timeout, but the child sleeps 5s. The sandbox must
        // kill the child and return `killed_by_timeout = true`.
        let args = ["-c", "sleep 5"];
        let cmd = Command::new(Path::new("sh"), &args).timeout(Duration::from_millis(200));
        let output = sandbox.run_cmd(&cmd).await.unwrap();
        assert!(
            output.killed_by_timeout,
            "wall-clock timeout must set killed_by_timeout"
        );
        // The kernel hasn't reported an exit code after a SIGKILL;
        // the sandbox surfaces `None` rather than fabricating a
        // value.
        assert!(
            output.exit_code.is_none(),
            "killed children must report exit_code = None, got {:?}",
            output.exit_code
        );
    }

    /// `run_in_with_limits_legacy` (D.11.15 compat shim) must
    /// accept the legacy positional signature, build a [`Command`]
    /// from it, and behave identically to the new
    /// [`Sandbox::run_cmd`] call. The `#[deprecated]` attribute on
    /// the wrapper is the only outward difference; this test
    /// opts out of the deprecation lint with
    /// `#[allow(deprecated)]` so it stays runnable across the
    /// migration window.
    #[tokio::test]
    #[allow(deprecated)]
    async fn sandbox_run_legacy_wrapper_still_works() {
        let sandbox = sb();
        let binary = Path::new("echo");
        let args = ["hello-legacy"];
        let env: Vec<(&str, &str)> = Vec::new();
        let cwd: Option<&Path> = None;
        let stdin: Option<Vec<u8>> = None;
        let timeout: Option<Duration> = None;
        let output = sandbox
            .run_in_with_limits_legacy(
                binary,
                &args,
                &env,
                cwd,
                stdin,
                timeout,
                MAX_STDOUT_BYTES,
                MAX_STDERR_BYTES,
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, b"hello-legacy\n");
        assert!(!output.killed_by_timeout);
    }

    // ----------------------------------------------------------------
    // D.11.13 — `MoaSandbox` + `NetworkPolicy` tests.
    //
    // Each test covers one of the acceptance criteria spelled out in
    // the PR-E spec. The constructor / getter / legacy-mapping tests
    // are synchronous so they run even when the tokio runtime is
    // unavailable; the spawn-side test is `#[tokio::test]`.
    // ----------------------------------------------------------------

    /// D.11.13: `MoaSandbox::new` stores the supplied policy verbatim
    /// on the wrapper. The getter returns a borrow so callers can
    /// check the policy without consuming the wrapper.
    #[test]
    fn moa_sandbox_new_stores_policy() {
        let cfg = SandboxConfig::new();
        let policy = NetworkPolicy::AllowList {
            hosts: vec!["crates.io".to_owned(), "github.com".to_owned()],
        };
        let moa = MoaSandbox::new(&cfg, policy.clone()).expect("moa sandbox builds");
        assert_eq!(moa.network(), &policy);
    }

    /// D.11.13: when the supplied policy is `Off`, `MoaSandbox::run_cmd`
    /// extracts host-like strings from argv, emits a `tracing::warn!`
    /// line for each one, and proceeds to spawn the subprocess.
    /// Socket-level enforcement is left to D.11.7 / D.11.1, so the
    /// observable contract at D.11.13 is "logs a denial, lets the
    /// run complete". The `tracing_subscriber` initialisation is a
    /// best-effort `try_init`: a parallel test that already installed
    /// a subscriber is fine; the only failure mode is "no subscriber
    /// installed", which does not affect the assertions below.
    #[tokio::test]
    async fn moa_sandbox_run_cmd_with_off_logs_denial() {
        let _ = tracing_subscriber::fmt::try_init();
        let cfg = SandboxConfig::new();
        let moa = MoaSandbox::new(&cfg, NetworkPolicy::Off).expect("moa sandbox builds");
        let args = ["-c", "echo example.com"];
        let cmd = Command::new(Path::new("sh"), &args);
        let output = moa.run_cmd(&cmd).await.expect("run completes");
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, b"example.com\n");
        assert!(!output.killed_by_timeout);
    }

    /// D.11.13: the legacy `SandboxConfig::allow_network = true`
    /// path (catalog §D.11.9) maps to [`NetworkPolicy::Open`] on the
    /// wrapper. Pinning the mapping here means a refactor that drops
    /// the wrapper / setter symmetry surfaces as a test failure.
    #[test]
    #[allow(deprecated)]
    fn moa_sandbox_legacy_allow_network_true_maps_to_open() {
        let cfg = SandboxConfig::new().with_allow_network(true);
        assert!(matches!(cfg.network_policy, NetworkPolicy::Open));
        let moa = MoaSandbox::new(&cfg, cfg.network_policy.clone()).expect("moa sandbox builds");
        assert_eq!(moa.network(), &NetworkPolicy::Open);
    }

    /// D.11.13: `with_network_policy(AllowList)` round-trips the list
    /// so `MoaSandbox::run_cmd` can validate hosts against it. This
    /// complements the policy-level tests in `policy.rs` by pinning
    /// the integration with [`SandboxConfig`].
    #[test]
    fn moa_sandbox_with_network_policy_allow_list_round_trip() {
        let list = vec!["crates.io".to_owned(), "github.com".to_owned()];
        let cfg = SandboxConfig::new().with_network_policy(NetworkPolicy::AllowList {
            hosts: list.clone(),
        });
        let moa = MoaSandbox::new(
            &cfg,
            NetworkPolicy::AllowList {
                hosts: list.clone(),
            },
        )
        .expect("moa sandbox");
        assert_eq!(moa.network(), &NetworkPolicy::AllowList { hosts: list });
    }

    /// D.11.13: `MoaSandbox::run_cmd` returns the same
    /// [`SandboxOutput`] shape as the underlying [`Sandbox::run_cmd`]
    /// so call sites that already consume [`SandboxOutput`] do not
    /// need to special-case the wrapper.
    #[tokio::test]
    async fn moa_sandbox_run_cmd_returns_sandbox_output() {
        let cfg = SandboxConfig::new();
        let moa = MoaSandbox::new(&cfg, NetworkPolicy::Open).expect("moa sandbox builds");
        let args = ["hello"];
        let cmd = Command::new(Path::new("echo"), &args);
        let output = moa.run_cmd(&cmd).await.expect("run completes");
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, b"hello\n");
        assert!(!output.killed_by_timeout);
    }

    /// D.11.13: the helper [`extract_hosts`] catches common host
    /// patterns and rejects noise like relative paths, shell
    /// metacharacters, and empty strings. Pinning the heuristic here
    /// means a refactor that tightens or loosens the pattern is
    /// visible in the test diff.
    #[test]
    fn extract_hosts_catches_dotted_argv_tokens() {
        let binary = Path::new("/bin/sh");
        let args = ["-c", "echo example.com"];
        let cmd = Command::new(binary, &args);
        let hosts = extract_hosts(&cmd);
        assert!(
            hosts.contains(&"example.com".to_owned()),
            "expected example.com in hosts, got {hosts:?}"
        );
        assert!(
            !hosts.contains(&"-c".to_owned()),
            "flag-like args must not be treated as hosts, got {hosts:?}"
        );
    }

    /// D.11.13: the binary path itself is scanned, so a literal
    /// `curl` argument or a hostname-shaped binary name still trips
    /// the pre-check.
    #[test]
    fn extract_hosts_includes_binary_when_dotted() {
        let binary = Path::new("host.example.com");
        let cmd = Command::new(binary, &[]);
        let hosts = extract_hosts(&cmd);
        assert_eq!(hosts, vec!["host.example.com".to_owned()]);
    }

    // ----------------------------------------------------------------
    // D.11.11 — Watchdog kills process tree on timeout / cancel.
    //
    // Each test below covers one of the acceptance criteria spelled
    // out in the PR-F spec. The Unix-only tests use `spawn_in_new_pgid`
    // to put the child in its own process group (mirroring the
    // sandbox's `pre_exec` `setpgid(0, 0)`) so the watchdog's `killpg`
    // targets the right group. The non-unix test is gated
    // `#[cfg(not(unix))]` and asserts the documented no-op behavior.
    // ----------------------------------------------------------------

    /// Spawn `binary args` in a fresh process group by setting
    /// `setpgid(0, 0)` in `pre_exec`. Mirrors the sandbox's `pre_exec`
    /// hook so the watchdog's `killpg(-pgid, ...)` reaches the whole
    /// subtree. Unix-only.
    #[cfg(unix)]
    fn spawn_in_new_pgid(binary: &str, args: &[&str]) -> tokio::process::Child {
        let mut cmd = TokioCommand::new(binary);
        cmd.args(args);
        // SAFETY: `pre_exec` runs in the child between fork and exec.
        // `setpgid(0, 0)` only mutates the child's own process-group
        // membership; a non-zero return surfaces as `Err` and aborts
        // the spawn.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        cmd.spawn().expect("spawn in new pgid")
    }

    /// D.11.11: the watchdog must fire `SIGTERM` at the supplied pgid
    /// when the timeout elapses. The test spawns a `sleep` in a fresh
    /// pgid, hands the watchdog that pgid, and asserts the child is
    /// terminated by a signal (exit code `None` under tokio's wait).
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_spawns_with_correct_pgid() {
        let mut child = spawn_in_new_pgid("sh", &["-c", "sleep 60"]);
        let pid = child.id().expect("child pid") as i32;
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let _handle = Watchdog::spawn(
            pid,
            Duration::from_millis(200),
            Duration::from_millis(100),
            cancel.clone(),
        );
        let status = child.wait().await.expect("child wait");
        let elapsed = started.elapsed();
        // The watchdog killed the child via SIGTERM. `tokio::process::Child::wait`
        // surfaces `code() == None` when the kernel reports a
        // signal-terminated process.
        assert!(
            status.code().is_none(),
            "child must be signal-terminated, got code {:?}",
            status.code()
        );
        // The kill must not happen before the timeout (the watchdog's
        // `select!` resolves on `timeout`, not immediately).
        assert!(
            elapsed >= Duration::from_millis(200),
            "must wait at least the timeout, got {elapsed:?}"
        );
        // And it should not take much longer than timeout + grace.
        assert!(
            elapsed < Duration::from_secs(5),
            "must kill within timeout + grace, got {elapsed:?}"
        );
    }

    /// D.11.11: the watchdog must kill the whole process tree, not
    /// just the immediate child. The test spawns a shell that spawns a
    /// `sleep` grandchild and waits; the watchdog's `killpg` must
    /// terminate the shell. The grandchild is reaped as a side effect
    /// of the parent shell's death (and by the SIGKILL fallback).
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_kills_pgid_on_timeout() {
        let mut child = spawn_in_new_pgid("sh", &["-c", "sleep 60 & wait"]);
        let pid = child.id().expect("child pid") as i32;
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let _handle = Watchdog::spawn(
            pid,
            Duration::from_millis(500),
            Duration::from_millis(200),
            cancel.clone(),
        );
        let status = child.wait().await.expect("child wait");
        let elapsed = started.elapsed();
        assert!(
            status.code().is_none(),
            "whole tree must be signal-terminated, got code {:?}",
            status.code()
        );
        // Total elapsed must be at least the timeout (the watchdog did
        // not fire prematurely) and bounded by timeout + grace + slack.
        assert!(
            elapsed >= Duration::from_millis(500),
            "must wait at least timeout, got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must kill within timeout + grace, got {elapsed:?}"
        );
    }

    /// D.11.11: when the child exits naturally before the watchdog's
    /// timeout fires, the watchdog must not leave any unrelated process
    /// behind. The test cancels the watchdog's token (the sandbox does
    /// this on every exit branch) and verifies that the watchdog's
    /// `SIGTERM` + grace + `SIGKILL` sequence is harmless because the
    /// pgid is already gone. Concretely: the join handle completes
    /// without panic, and a witness process in a separate pgid is
    /// untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_does_not_kill_when_child_exits_normally() {
        let mut child = spawn_in_new_pgid("true", &[]);
        let pid = child.id().expect("child pid") as i32;
        let status = child.wait().await.expect("child wait");
        assert!(status.success(), "`true` exits 0, got {status:?}");

        // Spawn a witness in its own pgid. The watchdog's SIGTERM must
        // not reach it because the watchdog only knows the dead
        // child's pgid.
        let mut witness = spawn_in_new_pgid("sleep", &["3"]);
        let witness_pid = witness.id().expect("witness pid") as i32;

        let cancel = CancellationToken::new();
        let handle = Watchdog::spawn(
            pid,
            Duration::from_secs(5),
            Duration::from_millis(100),
            cancel.clone(),
        );
        cancel.cancel();
        handle.await.expect("watchdog completes");

        // The witness must still be alive. `kill(pid, 0)` returns 0 if
        // the process exists. SAFETY: `kill` with signal 0 is a
        // pure-existence probe.
        let alive = unsafe { libc::kill(witness_pid as libc::pid_t, 0) } == 0;
        assert!(
            alive,
            "witness must be alive after the watchdog's cancel path"
        );

        // Clean up the witness so the test does not leak a sleep.
        let _ = witness.kill().await;
        let _ = witness.wait().await;
    }

    /// D.11.11: the watchdog must wait the configured grace period
    /// between SIGTERM and SIGKILL. The test spawns a shell that
    /// ignores SIGTERM (via `trap '' TERM`) and loops forever, so the
    /// only thing that can kill it is the watchdog's SIGKILL. The
    /// elapsed time from watchdog spawn to child death must therefore
    /// be approximately `timeout + grace`.
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_respects_grace_period_before_sigkill() {
        let mut child = spawn_in_new_pgid("sh", &["-c", "trap '' TERM; while :; do sleep 1; done"]);
        let pid = child.id().expect("child pid") as i32;
        let grace = Duration::from_millis(500);
        let timeout = Duration::from_millis(200);
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let _handle = Watchdog::spawn(pid, timeout, grace, cancel.clone());
        let status = child.wait().await.expect("child wait");
        let elapsed = started.elapsed();
        // The shell ignores SIGTERM (`trap '' TERM`), so the SIGTERM
        // is a no-op and the watchdog's SIGKILL is what kills it.
        // Expected elapsed: timeout + grace (with scheduler jitter).
        let lower = timeout
            .checked_add(grace)
            .unwrap()
            .saturating_sub(Duration::from_millis(100));
        let upper = timeout
            .checked_add(grace)
            .unwrap()
            .saturating_add(Duration::from_secs(3));
        assert!(
            elapsed >= lower,
            "elapsed must be >= timeout + grace - 100ms, got {elapsed:?}"
        );
        assert!(
            elapsed < upper,
            "elapsed must be < timeout + grace + 3s, got {elapsed:?}"
        );
        assert!(
            status.code().is_none(),
            "child must be signal-killed, got code {:?}",
            status.code()
        );
    }

    /// D.11.11: the watchdog is a documented no-op on non-Unix
    /// platforms. The spawned task completes immediately without
    /// touching any process tree; `kill_on_drop` remains the cleanup
    /// guarantee on Windows.
    #[cfg(not(unix))]
    #[tokio::test]
    async fn watchdog_is_noop_on_non_unix() {
        let cancel = CancellationToken::new();
        let handle = Watchdog::spawn(
            12345,
            Duration::from_millis(100),
            Duration::from_millis(50),
            cancel.clone(),
        );
        // The handle must complete without panic and within a small
        // budget so the test does not hang.
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("watchdog handle completes within 1s")
            .expect("watchdog handle does not panic");
    }
}
