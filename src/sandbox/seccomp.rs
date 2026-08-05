//! Opt-in seccomp syscall whitelist for the sandbox (catalog §D.11.7).
//!
//! [`SeccompPolicyKind`] is the closed enum the operator picks from.
//! The default is [`SeccompPolicyKind::Permissive`], which is a
//! no-op; turning the knob to [`SeccompPolicyKind::StrictRustBuild`]
//! is supposed to load a BPF program via
//! `prctl(PR_SET_NO_NEW_PRIVS, 1)` +
//! `seccomp(SECCOMP_MODE_FILTER, ...)` so the sandboxed subprocess
//! cannot escape the allow-list of syscalls needed to compile Rust.
//!
//! Linux-only: on every other Unix / Unix-like (macOS) and on
//! Windows the module is a documented no-op so cross-platform builds
//! keep linking. The crate-level no-go list forbids
//! `seccompiler` / `libseccomp` / `capsicum`, so the BPF program is
//! built manually with `libc::sock_filter` + the raw `prctl` /
//! `seccomp` syscalls (no SDK dependency).
//!
//! ## Status
//!
//! The structure, the default, and the no-op paths are implemented
//! and tested. The BPF program for [`SeccompPolicyKind::StrictRustBuild`]
//! is left as a `todo!()` because:
//!
//! - A correct allow-list needs ~30 syscalls (read, write, openat,
//!   close, mmap, mprotect, munmap, brk, rt_sigaction,
//!   rt_sigprocmask, exit_group, exit, futex, getrandom,
//!   arch_prctl, set_tid_address, set_robust_list, madvise, ...)
//!   and each one needs the right BPF instruction encoding.
//! - Exercising `seccomp(SECCOMP_MODE_FILTER, ...)` requires the
//!   test harness to install a BPF program in CI, which is not
//!   feasible without root privileges and a guaranteed kernel with
//!   `CONFIG_SECCOMP`.
//!
//! The wire-up is in place: [`crate::sandbox::process::SandboxConfig`]
//! carries the [`SeccompPolicyKind`] and the sandbox's `pre_exec`
//! calls [`apply`] between fork and exec. A future PR can drop the
//! BPF program into [`apply_strict_rust_build`] without touching
//! any caller. The default install runs with
//! [`SeccompPolicyKind::Permissive`] so the sandbox is unchanged for
//! existing runs.
//!
//! Compliance: catalog `10-integrada-v0` §D.11.7.

use serde::{Deserialize, Serialize};

/// Closed enum of seccomp policies the sandbox can apply to the
/// subprocess. Catalog §D.11.7.
///
/// Mirrors the design of [`crate::sandbox::NetworkPolicy`]: closed
/// enum, serde `snake_case`, default = the most restrictive variant
/// the operator can leave running without enabling anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeccompPolicyKind {
    /// No filtering. `apply(Permissive)` is a no-op so the default
    /// install is byte-for-byte identical to before this PR landed.
    #[default]
    Permissive,
    /// Allow only the syscalls needed to compile a Rust crate from
    /// scratch: read, write, openat, close, mmap, mprotect, munmap,
    /// brk, rt_sigaction, rt_sigprocmask, exit_group, exit, futex,
    /// getrandom, arch_prctl, set_tid_address, set_robust_list,
    /// madvise, and a handful of close relatives.
    ///
    /// The BPF program is built manually with `libc::sock_filter`
    /// (catalog no-go forbids `seccompiler` / `libseccomp`) and
    /// installed via `prctl(PR_SET_NO_NEW_PRIVS, 1)` +
    /// `seccomp(SECCOMP_MODE_FILTER, ...)`. See
    /// <https://www.kernel.org/doc/Documentation/prctl/seccomp_filter.txt>
    /// and <https://github.com/seccomp/libseccomp> for reference.
    StrictRustBuild,
}

/// Apply the seccomp policy to the current process.
///
/// Unix-only: the function returns `Ok(())` on every other platform
/// so cross-platform builds keep linking. On Linux the policy is
/// loaded into the calling process; the natural call site is inside
/// `tokio::process::Command::pre_exec`, between fork and exec, so the
/// filter applies to the child only and the parent keeps running
/// unrestricted.
///
/// # Errors
///
/// Returns [`crate::Error::InvalidState`] when the kernel rejects
/// the BPF program (malformed instructions, missing
/// `CONFIG_SECCOMP`, no `CAP_SYS_ADMIN`, etc.). On
/// [`SeccompPolicyKind::Permissive`] the function is infallible.
pub fn apply(kind: SeccompPolicyKind) -> crate::error::Result<()> {
    apply_for_target(kind)
}

/// Unix implementation of [`apply`]. The BPF program lives behind
/// [`apply_strict_rust_build`]; the no-op default ([`SeccompPolicyKind::Permissive`])
/// short-circuits here.
#[cfg(unix)]
fn apply_for_target(kind: SeccompPolicyKind) -> crate::error::Result<()> {
    match kind {
        SeccompPolicyKind::Permissive => Ok(()),
        SeccompPolicyKind::StrictRustBuild => apply_strict_rust_build(),
    }
}

/// Non-Unix implementation of [`apply`]. Always a no-op so
/// cross-platform builds link.
#[cfg(not(unix))]
fn apply_for_target(_kind: SeccompPolicyKind) -> crate::error::Result<()> {
    Ok(())
}

/// Build the BPF program for the Rust-build allow-list and load it
/// into the calling process via
/// `prctl(PR_SET_NO_NEW_PRIVS, 1)` + `seccomp(SECCOMP_MODE_FILTER, ...)`.
///
/// # Status
///
/// Stubbed with `todo!()` because:
///
/// 1. The allow-list is ~30 syscalls and each one needs the right
///    BPF instruction encoding (audit + benchmark pass to pin a
///    correct program). Out of scope for the partial PR.
/// 2. Exercising the loader requires a CI runner with
///    `CONFIG_SECCOMP` and root privileges so the BPF program can be
///    installed and the sandbox can survive without panicking on
///    the first disallowed syscall. CI does not provide that.
///
/// A future PR will replace this stub with the real BPF program.
/// The call site ([`crate::sandbox::process::Sandbox::run_in_with_limits`])
/// is already wired so the migration is drop-in: replace the body
/// of this function and every `pre_exec` chain keeps working.
#[cfg(unix)]
fn apply_strict_rust_build() -> crate::error::Result<()> {
    // Future implementation outline:
    //   1. Build a `Vec<libc::sock_filter>` that:
    //        - loads arch (`BPF_LD | BPF_W | BPF_ABS`, offsetof(seccomp_data, arch))
    //        - jumps to KILL if the arch is not the expected one
    //        - loads the syscall number
    //          (`BPF_LD | BPF_W | BPF_ABS`, offsetof(seccomp_data, nr))
    //        - allows the configured set (see module doc) via
    //          `BPF_JMP | BPF_JEQ` + `BPF_RET | BPF_K`
    //        - returns `SECCOMP_RET_KILL_PROCESS` for everything else
    //   2. Call `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` so
    //      unprivileged processes can install a BPF filter.
    //   3. Call `seccomp(SECCOMP_MODE_FILTER, 0, &prog)` to load
    //      the program. From this point on, every disallowed
    //      syscall is killed by the kernel.
    todo!(
        "implement BPF program for SeccompPolicyKind::StrictRustBuild (D.11.7); \
         see https://www.kernel.org/doc/Documentation/prctl/seccomp_filter.txt \
         and https://github.com/seccomp/libseccomp for reference"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catalog §D.11.7: the default value of [`SeccompPolicyKind`] is
    /// [`SeccompPolicyKind::Permissive`] so the default install is
    /// unaffected by this PR. Pin the default so a refactor that
    /// flips it trips the test before it lands in production.
    #[test]
    fn seccomp_policy_kind_default_is_permissive() {
        assert_eq!(SeccompPolicyKind::default(), SeccompPolicyKind::Permissive);
    }

    /// Catalog §D.11.7: serde round-trips every variant in
    /// `snake_case` so operators can pin their choice in
    /// `~/.config/moagan/config.toml` with
    /// `sandbox_seccomp = "strict_rust_build"`. Pin the wire format
    /// here so a rename surfaces as a test failure rather than a
    /// silent TOML breakage.
    #[test]
    fn seccomp_policy_kind_serializes_to_snake_case() {
        let permissive = serde_json::to_string(&SeccompPolicyKind::Permissive).unwrap();
        assert!(
            permissive.contains("permissive"),
            "Permissive must serialise as snake_case, got {permissive}"
        );
        let strict = serde_json::to_string(&SeccompPolicyKind::StrictRustBuild).unwrap();
        assert!(
            strict.contains("strict_rust_build"),
            "StrictRustBuild must serialise as strict_rust_build, got {strict}"
        );
    }

    /// Catalog §D.11.7: serde accepts the snake_case form on the way
    /// back, including the operator-facing strings
    /// `"permissive"` and `"strict_rust_build"`. Pin the deserialiser
    /// so a rename breaks the test rather than a TOML reload.
    #[test]
    fn seccomp_policy_kind_deserializes_from_snake_case() {
        let permissive: SeccompPolicyKind = serde_json::from_str(r#""permissive""#).unwrap();
        assert_eq!(permissive, SeccompPolicyKind::Permissive);
        let strict: SeccompPolicyKind = serde_json::from_str(r#""strict_rust_build""#).unwrap();
        assert_eq!(strict, SeccompPolicyKind::StrictRustBuild);
    }

    /// Catalog §D.11.7: applying [`SeccompPolicyKind::Permissive`]
    /// is a documented no-op (the function returns `Ok(())`
    /// without touching the kernel). The test runs on every
    /// platform because [`apply`] is a no-op outside Unix as well.
    #[test]
    fn seccomp_apply_permissive_is_noop() {
        let result = apply(SeccompPolicyKind::Permissive);
        assert!(
            result.is_ok(),
            "Permissive policy must be a no-op, got {result:?}"
        );
    }

    /// Catalog §D.11.7: outside Linux the function short-circuits to
    /// `Ok(())` so cross-platform builds link and the wire-up does
    /// not have to be gated twice. Gated `#[cfg(not(unix))]` so the
    /// test compiles on every platform; on Linux the
    /// `#[cfg(unix)]` path is exercised by
    /// [`seccomp_apply_permissive_is_noop`] which runs on every
    /// target.
    #[cfg(not(unix))]
    #[test]
    fn seccomp_apply_on_non_unix_returns_ok() {
        let permissive = apply(SeccompPolicyKind::Permissive);
        let strict = apply(SeccompPolicyKind::StrictRustBuild);
        assert!(permissive.is_ok(), "non-Unix Permissive must be Ok");
        assert!(strict.is_ok(), "non-Unix StrictRustBuild must be Ok");
    }

    /// Equality + `Copy` are part of the public contract: the
    /// policy is stored on [`crate::config::Config`] and
    /// [`crate::sandbox::process::SandboxConfig`] by value, so the
    /// enum must satisfy `Copy` and `Eq` for the same reasons as
    /// [`crate::sandbox::NetworkPolicy`].
    #[test]
    fn seccomp_policy_kind_is_copy_and_eq() {
        let a = SeccompPolicyKind::Permissive;
        let b = a;
        assert_eq!(a, b);
        let strict = SeccompPolicyKind::StrictRustBuild;
        assert_ne!(a, strict);
    }
}
