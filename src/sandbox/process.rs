//! Subprocess sandbox for executable validation.
//!
//! [`Sandbox`] wraps `tokio::process::Command` with a fresh
//! `tempfile::TempDir` per `run`, a hard wall-clock timeout, an
//! allowlist + denylist policy, and a capped stdout/stderr buffer
//! (4 KiB each) so a runaway process cannot blow up the run memory.
//!
//! The sandbox inherits the process environment but strips anything
//! that smells like a secret (see [`SandboxConfig::strip_secrets_env`])
//! and forces `PATH` to the standard system paths and `HOME` to the
//! scratch directory.
//!
//! Compliance: `proposal-02-rust.md` §7. The hardened variants
//! (`cgroup`, `unshare`, `seccomp`) live in catalog 10-integrada-v0
//! §D.11 and are not yet implemented here.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time;

use crate::error::{Error, Result};

use super::allowlist::{Allowlist, Denylist, contains_deny_token, is_allowed};

/// Maximum bytes captured per stream before truncation. Matches
/// `proposal-02-rust.md` §7 cap (4 KiB).
pub const MAX_STDOUT_BYTES: usize = 4 * 1024;
/// Same cap for stderr. Mirrors stdout for symmetry.
pub const MAX_STDERR_BYTES: usize = 4 * 1024;

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
    pub async fn run(&self, cmd: &str, args: &[&str]) -> Result<SandboxResult> {
        let work = self.new_workdir()?;
        self.run_in(work.path(), cmd, args).await
    }

    /// Execute `cmd` with `args` inside the supplied `work_dir`. The
    /// directory's contents are visible to the spawned process and
    /// the process's CWD is set to it. The caller retains ownership
    /// of `work_dir` and decides when to drop it.
    pub async fn run_in(&self, work_dir: &Path, cmd: &str, args: &[&str]) -> Result<SandboxResult> {
        let started = Instant::now();
        let argv: Vec<String> = std::iter::once(cmd.to_owned())
            .chain(args.iter().map(|s| (*s).to_owned()))
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

        let (stdout_cap, stderr_cap) = match self.config.max_capture_bytes {
            Some(n) => (n, n),
            None => (MAX_STDOUT_BYTES, MAX_STDERR_BYTES),
        };

        let mut command = Command::new(cmd);
        command
            .args(args)
            .current_dir(work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let env = self.build_env(work_dir);
        command.env_clear();
        for (k, v) in &env {
            command.env(k, v);
        }

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SandboxResult::new(
                    -1,
                    String::new(),
                    format!("binary not found: {e}"),
                    started.elapsed(),
                    SandboxStatus::NotFound,
                    command_str,
                ));
            }
            Err(e) => {
                return Ok(SandboxResult::new(
                    -1,
                    String::new(),
                    format!("spawn failed: {e}"),
                    started.elapsed(),
                    SandboxStatus::Error,
                    command_str,
                ));
            }
        };

        let stdout_handle = child.stdout.take().map(|mut s| {
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(stdout_cap);
                let mut tmp = [0u8; 1024];
                loop {
                    match s.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if buf.len() + n > stdout_cap {
                                let remaining = stdout_cap.saturating_sub(buf.len());
                                buf.extend_from_slice(&tmp[..remaining]);
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        Err(_) => break,
                    }
                }
                buf
            })
        });
        let stderr_handle = child.stderr.take().map(|mut s| {
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(stderr_cap);
                let mut tmp = [0u8; 1024];
                loop {
                    match s.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if buf.len() + n > stderr_cap {
                                let remaining = stderr_cap.saturating_sub(buf.len());
                                buf.extend_from_slice(&tmp[..remaining]);
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        Err(_) => break,
                    }
                }
                buf
            })
        });

        let timeout_at = started + self.config.timeout;
        let (status, exit_code) = {
            let now = Instant::now();
            if now >= timeout_at {
                let _ = child.start_kill();
                let _ = child.wait().await;
                (SandboxStatus::Timeout, -1)
            } else {
                let remaining = timeout_at - now;
                match time::timeout(remaining, child.wait()).await {
                    Ok(Ok(es)) => {
                        let exit_code = es.code().unwrap_or(-1);
                        let status = if exit_code == 0 {
                            SandboxStatus::Pass
                        } else {
                            SandboxStatus::Fail
                        };
                        (status, exit_code)
                    }
                    Ok(Err(_)) => (SandboxStatus::Error, -1),
                    Err(_) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        (SandboxStatus::Timeout, -1)
                    }
                }
            }
        };

        let stdout_bytes: Vec<u8> = match stdout_handle {
            Some(h) => h.await.unwrap_or_default(),
            None => Vec::new(),
        };
        let stderr_bytes: Vec<u8> = match stderr_handle {
            Some(h) => h.await.unwrap_or_default(),
            None => Vec::new(),
        };

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

    #[tokio::test]
    async fn stdout_capture_is_capped() {
        // Generate 8 KiB of output; cap is 4 KiB by default.
        let cfg = SandboxConfig::new().with_max_capture(256);
        let sandbox = Sandbox::new(cfg).unwrap();
        let result = sandbox
            .run("sh", &["-c", "yes A | head -c 8192"])
            .await
            .unwrap();
        assert_eq!(result.status, SandboxStatus::Pass);
        assert!(result.stdout.len() <= 256);
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
