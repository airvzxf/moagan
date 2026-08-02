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

use crate::error::{Error, Result};

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
        allow_network: true,
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
}

impl SandboxConfig {
    /// Build a config with the project defaults:
    /// `timeout = 30s`, default allowlist + denylist.
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            allowlist: Allowlist::default(),
            denylist: Denylist::default(),
            max_capture_bytes: None,
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
}

impl Sandbox {
    /// Build a new sandbox with the supplied configuration.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        if config.timeout.is_zero() {
            return Err(Error::InvalidState(
                "sandbox timeout must be > 0; use no_timeout() to opt out explicitly".into(),
            ));
        }
        Ok(Self { config })
    }

    /// Borrow the current configuration.
    pub fn config(&self) -> &SandboxConfig {
        &self.config
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
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let argv: Vec<String> = std::iter::once(cmd.to_owned())
            .chain(args.iter().cloned())
            .collect();
        let command_str = argv.join(" ");

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
        if contains_deny_token(&argv, &self.config.denylist) {
            return Ok(SandboxResult::new(
                -1,
                String::new(),
                "argv contains a denylisted token".into(),
                started.elapsed(),
                SandboxStatus::NotAllowed,
                command_str,
            ));
        }

        let mut command = Command::new(cmd);
        command
            .args(&args)
            .current_dir(work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

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
        assert!(config.allow_network);
    }

    #[test]
    fn command_config_for_unknown_name_returns_none() {
        assert!(config_for("unknown-language").is_none());
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
}
