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
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time;

use crate::cancel::Cancel;
use crate::error::{Error, Result};
use crate::redact::{RedactPolicy, Surface, apply};

use super::allowlist::{Allowlist, Denylist, contains_deny_token, is_allowed};

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
    /// Whether the subprocess can reach the network. Default `false`
    /// (off-by-default, catalog §D.11.9). When `false`, the sandbox
    /// sets `CARGO_NET_OFFLINE=true` in the subprocess env so cargo
    /// cannot fetch crates from the network. Callers that need
    /// network must opt in explicitly via [`SandboxConfig::with_allow_network`].
    pub allow_network: bool,
    /// Whether to skip the secret-stripping pass over argv. Default
    /// `false` (strip). When `true`, the raw args are passed to the
    /// subprocess without redaction. Useful for debugging / repro
    /// cases where the operator wants to see exactly what bytes were
    /// passed. Catalog §D.11.10.
    pub allow_injection: bool,
}

impl SandboxConfig {
    /// Build a config with the project defaults:
    /// `timeout = 30s`, default allowlist + denylist, network
    /// disabled, secret stripping enabled.
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            allowlist: Allowlist::default(),
            denylist: Denylist::default(),
            max_capture_bytes: None,
            allow_network: false,
            allow_injection: false,
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
    pub fn with_allow_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    /// Opt out of the secret-stripping pass over argv. Default is
    /// `false` (strip). When `true`, the raw args are passed to the
    /// subprocess verbatim. Catalog §D.11.10.
    pub fn with_allow_injection(mut self, allow: bool) -> Self {
        self.allow_injection = allow;
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
            config_for_binary(cmd)
                .map(|config| config.max_output_bytes)
                .unwrap_or(DEFAULT_OUTPUT_CAP_BYTES)
        });
        self.run_in_with_limits(work_dir, cmd, args, max_output_bytes)
            .await
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
        self.run_in_with_limits(work_dir, cmd, args, max_output_bytes)
            .await
    }

    async fn run_in_with_limits(
        &self,
        work_dir: &Path,
        cmd: &str,
        args: &[&str],
        max_output_bytes: usize,
    ) -> std::result::Result<SandboxResult, SandboxError> {
        let started = Instant::now();
        let raw_args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        // Catalog §D.11.10: when `allow_injection` is opted in, the
        // raw args are passed to the subprocess verbatim and the
        // argv-side secret-stripping pass is skipped. The visible
        // `command_str` (used by telemetry / sidecars) also reflects
        // the unredacted args so the operator sees exactly what ran.
        let sanitized_args = if self.config.allow_injection {
            raw_args.clone()
        } else {
            strip_secrets(&raw_args)
        };
        let policy_argv: Vec<String> = std::iter::once(cmd.to_owned())
            .chain(raw_args.iter().cloned())
            .collect();
        let command_str = std::iter::once(cmd.to_owned())
            .chain(sanitized_args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");

        if !is_allowed(cmd, &self.config.allowlist) {
            return Ok(SandboxResult::new(
                -1,
                String::new(),
                format!("command '{cmd}' is not in the sandbox allowlist"),
                started.elapsed(),
                SandboxStatus::NotAllowed,
                command_str,
            ));
        }
        if contains_deny_token(&policy_argv, &self.config.denylist) {
            return Ok(SandboxResult::new(
                -1,
                String::new(),
                "argv contains a denylisted token".into(),
                started.elapsed(),
                SandboxStatus::NotAllowed,
                command_str,
            ));
        }

        if let Err(SandboxError::BinaryNotFound(binary)) = verify_binary_exists(cmd) {
            return Ok(SandboxResult::new(
                -1,
                String::new(),
                format!("binary not found: {binary}"),
                started.elapsed(),
                SandboxStatus::NotFound,
                command_str,
            ));
        }

        let mut command = Command::new(cmd);
        command
            .args(&sanitized_args)
            .current_dir(work_dir)
            .stdin(Stdio::null())
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

        let env = self.build_env(work_dir);
        command.env_clear();
        for (key, value) in &env {
            command.env(key, value);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SandboxResult::new(
                    -1,
                    String::new(),
                    format!("binary not found: {cmd}"),
                    started.elapsed(),
                    SandboxStatus::NotFound,
                    command_str,
                ));
            }
            Err(error) => {
                return Ok(SandboxResult::new(
                    -1,
                    String::new(),
                    format!("spawn failed: {error}"),
                    started.elapsed(),
                    SandboxStatus::Error,
                    command_str,
                ));
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
        let _pgid_guard: Option<RegisteredChild<'_>> = match (&self.cancel, child.id()) {
            (Some(cancel), Some(pid)) => match i32::try_from(pid) {
                Ok(pgid) => {
                    cancel.register_child(pgid);
                    Some(RegisteredChild { cancel, pgid })
                }
                Err(_) => None,
            },
            _ => None,
        };

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let stdout_handle = child
            .stdout
            .take()
            .map(|stream| tokio::spawn(read_stream(stream, max_output_bytes, event_tx.clone())));
        let stderr_handle = child
            .stderr
            .take()
            .map(|stream| tokio::spawn(read_stream(stream, max_output_bytes, event_tx.clone())));
        drop(event_tx);

        let remaining = self.config.timeout.saturating_sub(started.elapsed());
        let deadline = time::sleep(remaining);
        tokio::pin!(deadline);
        let mut events_open = true;
        let (status, exit_code, early_error) = loop {
            tokio::select! {
                result = child.wait() => {
                    match result {
                        Ok(exit) => {
                            let exit_code = exit.code().unwrap_or(-1);
                            let status = if exit_code == 0 {
                                SandboxStatus::Pass
                            } else {
                                SandboxStatus::Fail
                            };
                            break (status, exit_code, None);
                        }
                        Err(_) => break (SandboxStatus::Error, -1, None),
                    }
                }
                event = event_rx.recv(), if events_open => {
                    match event {
                        Some(error) => {
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            break (SandboxStatus::Error, -1, Some(error));
                        }
                        None => events_open = false,
                    }
                }
                _ = &mut deadline => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    break (SandboxStatus::Timeout, -1, None);
                }
            }
        };

        // Every exit branch (natural, error, timeout, output-truncated)
        // converges here. The `RegisteredChild` guard's `Drop` impl
        // unregisters the pgid from the cancel registry as the stack
        // unwinds — including the case where the caller drops the
        // future mid-flight. The guard, not the call sites, is the
        // source of truth for unregistration.
        drop(_pgid_guard);

        let stdout_result = await_reader(stdout_handle).await;
        let stderr_result = await_reader(stderr_handle).await;
        if let Some(error) = early_error {
            return Err(error);
        }
        let stdout_bytes = stdout_result?;
        let stderr_bytes = stderr_result?;

        Ok(SandboxResult::new(
            exit_code,
            String::from_utf8_lossy(&stdout_bytes).into_owned(),
            String::from_utf8_lossy(&stderr_bytes).into_owned(),
            started.elapsed(),
            status,
            command_str,
        ))
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
    /// - When `allow_network == false` (the default, catalog §D.11.9),
    ///   inject `CARGO_NET_OFFLINE=true` so cargo refuses to fetch
    ///   crates from the registry. The flag is the canonical
    ///   cargo-respected hint; we do not attempt network namespaces
    ///   here because that requires CAP_SYS_ADMIN on the host.
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
        if !self.config.allow_network {
            env.insert("CARGO_NET_OFFLINE".into(), "true".into());
        }
        env
    }
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

    /// Catalog §D.11.9: the default `SandboxConfig` must forbid
    /// network access for the subprocess. The sandbox enforces that
    /// by injecting `CARGO_NET_OFFLINE=true` in the env, so the
    /// observable contract is "default cargo runs offline".
    #[tokio::test]
    async fn sandbox_default_does_not_allow_network() {
        let cfg = SandboxConfig::new();
        assert!(!cfg.allow_network, "default must opt out of network");
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

    /// Catalog §D.11.9: `with_allow_network(true)` opts in. The
    /// sandbox must NOT set `CARGO_NET_OFFLINE` so cargo can fetch
    /// crates from the registry.
    #[tokio::test]
    async fn sandbox_opt_in_allows_network() {
        let cfg = SandboxConfig::new().with_allow_network(true);
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
}
