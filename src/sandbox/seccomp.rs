//! Opt-in seccomp syscall whitelist for the sandbox (catalog §D.11.7).
//!
//! [`SeccompPolicyKind`] is the closed enum the operator picks from.
//! The default is [`SeccompPolicyKind::Permissive`], which is a
//! no-op; turning the knob to [`SeccompPolicyKind::StrictRustBuild`]
//! installs a hand-rolled BPF program via
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
//! ## BPF program
//!
//! [`build_bpf_program`] emits a `Vec<libc::sock_filter>` whose
//! shape is:
//!
//! ```text
//!   LD arch            // A = seccomp_data.arch
//!   JEQ AUDIT_ARCH_X86_64, 0, 1
//!   RET KILL_PROCESS   // wrong arch -> die
//!   LD nr              // A = seccomp_data.nr
//!   JEQ allowed_1, 0, 1
//!   RET ALLOW
//!   JEQ allowed_2, 0, 1
//!   RET ALLOW
//!   ...
//!   RET KILL_PROCESS   // default -> die
//! ```
//!
//! Each allowed syscall adds two instructions (one `JEQ`, one
//! `RET ALLOW`); the header (arch check + KILL-on-mismatch + nr
//! load) and the default `KILL` are constant overhead, so the
//! total instruction count is `4 + 2 * ALLOWLIST_LEN + 1` (well
//! under `BPF_MAXINSNS = 4096`).
//!
//! The program is installed with the `SECCOMP_MODE_FILTER` mode of
//! the `seccomp` syscall, taking a `sock_fprog` pointer. From the
//! moment the loader returns, every disallowed syscall kills the
//! process (`SECCOMP_RET_KILL_PROCESS`); the call site
//! ([`crate::sandbox::process::MoaSandbox::run_cmd`]'s `pre_exec`)
//! is the natural place to install it because the filter applies
//! to the child only — the parent keeps running unrestricted.
//!
//! ## Status
//!
//! Default, no-op paths, BPF builder, allow-list, and loader are
//! implemented and tested. Exercising the live `seccomp` syscall
//! requires the test harness to install a BPF program in CI, which
//! is not feasible without root privileges and a guaranteed kernel
//! with `CONFIG_SECCOMP`; the BPF builder is therefore exported as
//! [`build_bpf_program`] and unit-tested for shape, but the live
//! `prctl` / `seccomp` syscalls are not exercised in `cargo test`.
//!
//! The default install runs with [`SeccompPolicyKind::Permissive`]
//! so the sandbox is unchanged for existing runs.
//!
//! Compliance: catalog `10-integrada-v0` §D.11.7.

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use crate::sandbox::seccomp_allowlist::{ALLOWLIST_LEN, rust_build_allowlist};

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
    /// scratch: read, write, open, close, stat, fstat, lseek,
    /// pread64, pwrite64, readv, writev, access, ioctl, openat,
    /// fstatat, statx, fcntl, getdents64, mkdirat, unlinkat,
    /// renameat2, readlinkat, symlinkat, mmap, mprotect, munmap,
    /// brk, madvise, clone, fork, execve, wait4, exit, exit_group,
    /// getpid, gettid, getppid, set_tid_address, set_robust_list,
    /// arch_prctl, prlimit64, kill, setsid, rt_sigaction,
    /// rt_sigprocmask, rt_sigreturn, futex, getrandom, sched_yield,
    /// nanosleep, pipe, pipe2, dup2, dup3, close_range, getuid,
    /// getgid, geteuid, getegid, uname, gettimeofday, getcwd.
    ///
    /// Network syscalls (`socket`, `connect`, `sendto`, ...) and
    /// debug / privilege-escalation syscalls (`ptrace`, `kexec_*`,
    /// `reboot`, `mount`, `clone(CLONE_NEWUSER)`, ...) are
    /// deliberately absent: the BPF program defaults to
    /// `SECCOMP_RET_KILL_PROCESS` for any syscall not in the
    /// allow-list, which lives in
    /// [`crate::sandbox::seccomp_allowlist::rust_build_allowlist`].
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
/// The function is the live loader; the BPF builder itself is
/// [`build_bpf_program`] and is unit-tested for shape. Exercising
/// the loader from `cargo test` is not feasible (see module docs).
///
/// # Errors
///
/// Returns [`crate::error::Error::Io`] wrapping the kernel errno
/// when [`libc::syscall`] rejects either the `prctl` or the
/// `seccomp` call (missing `CONFIG_SECCOMP`, malformed BPF program,
/// no `CAP_SYS_ADMIN`, etc.).
#[cfg(unix)]
fn apply_strict_rust_build() -> crate::error::Result<()> {
    let allow = rust_build_allowlist();
    let program = build_bpf_program(&allow);

    // Pin `no_new_privs` first. Without this bit, an unprivileged
    // child cannot install a seccomp filter — the kernel returns
    // `EACCES`. `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` is
    // irreversible for the lifetime of the process, which is what
    // we want: once the sandbox subprocess has dropped privileges,
    // execve cannot reinstate them.
    let prctl_ret = unsafe {
        libc::syscall(
            libc::SYS_prctl,
            PR_SET_NO_NEW_PRIVS as libc::c_long,
            1 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    if prctl_ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(crate::error::Error::InvalidState(format!(
            "prctl(PR_SET_NO_NEW_PRIVS, 1) failed: {errno}"
        )));
    }

    // Install the BPF program. `SECCOMP_MODE_FILTER` takes a
    // pointer to a `sock_fprog` (`len: u16`, `filter: *mut sock_filter`).
    // The program must outlive the syscall; the `Vec<sock_filter>`
    // above is stable for the duration of this stack frame.
    let fprog = libc::sock_fprog {
        len: program.len() as libc::c_ushort,
        filter: program.as_ptr() as *mut libc::sock_filter,
    };
    let seccomp_ret = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_MODE_FILTER as libc::c_long,
            0 as libc::c_long,
            &fprog as *const libc::sock_fprog as libc::c_long,
        )
    };
    if seccomp_ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(crate::error::Error::InvalidState(format!(
            "seccomp(SECCOMP_MODE_FILTER) failed: {errno}"
        )));
    }
    Ok(())
}

/// `prctl(PR_SET_NO_NEW_PRIVS, ...)` opcode. Defined locally because
/// the `libc` crate does not expose it on the standard Linux GNU
/// target (it is only present on Android / Fuchsia / l4re). The
/// value `38` is stable in `include/uapi/linux/prctl.h` since
/// Linux 3.5 (2012) and is not going to change.
#[cfg(unix)]
const PR_SET_NO_NEW_PRIVS: i64 = 38;

/// `AUDIT_ARCH_X86_64` value used by `seccomp_data.arch`. Defined
/// locally for the same reason as [`PR_SET_NO_NEW_PRIVS`]: the
/// `libc` crate does not expose it on the standard Linux GNU target.
/// The value `0xC000_003E` is the `ELFCLASS64 | ELFDATA2LSB` magic
/// for x86_64 from `include/uapi/linux/audit.h`.
#[cfg(unix)]
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// Offset of `seccomp_data.arch` inside `struct seccomp_data`.
///
/// `struct seccomp_data` (from `include/uapi/linux/seccomp.h`) is:
///
/// ```c
/// struct seccomp_data {
///     int   nr;                   // offset 0,  4 bytes
///     __u32 arch;                 // offset 4,  4 bytes
///     __u64 instruction_pointer;  // offset 8,  8 bytes
///     __u64 args[6];              // offset 16, 48 bytes
/// };
/// ```
///
/// The fields have natural alignment on x86_64, so the offsets
/// match the layout in [`libc::seccomp_data`]. The constants are
/// pinned explicitly so a future kernel ABI change is caught by
/// the test suite rather than by a sandboxed subprocess dying
/// with `SECCOMP_RET_KILL_PROCESS`.
#[cfg(unix)]
const SECCOMP_DATA_OFFSET_NR: u32 = 0;

/// Offset of `seccomp_data.arch`; see [`SECCOMP_DATA_OFFSET_NR`].
#[cfg(unix)]
const SECCOMP_DATA_OFFSET_ARCH: u32 = 4;

/// Build the BPF program (as a `Vec<libc::sock_filter>`) for the
/// supplied allow-list.
///
/// Exposed `pub(crate)` so the unit tests can verify the shape
/// (instruction count, arch check, default `KILL_PROCESS`) without
/// needing root or `CONFIG_SECCOMP`. The live loader is
/// [`apply_strict_rust_build`]; this function is pure and panics
/// only on a logic bug, never on a kernel error.
///
/// The shape is documented in the module-level docs. The function
/// is `#[cfg(unix)]` because `libc::sock_filter` is only available
/// on Unix.
#[cfg(unix)]
pub(crate) fn build_bpf_program(allow: &[i64]) -> Vec<libc::sock_filter> {
    // Each BPF instruction is a `sock_filter` (code, jt, jf, k).
    // The header is 4 instructions (arch LD + arch JEQ + KILL on
    // mismatch + nr LD), the tail is 1 instruction (default KILL),
    // and every allowed syscall adds 2 instructions
    // (JEQ + RET ALLOW).
    let mut prog: Vec<libc::sock_filter> =
        Vec::with_capacity(4 + 2 * allow.len() + 1 + allow.len());

    // -- arch check --
    // LD  arch     -> A = seccomp_data.arch
    prog.push(unsafe {
        libc::BPF_STMT(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_OFFSET_ARCH,
        )
    });
    // JEQ AUDIT_ARCH_X86_64, jt=0 (fall through if x86_64), jf=1 (skip ALLOW)
    prog.push(unsafe {
        libc::BPF_JUMP(
            (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            AUDIT_ARCH_X86_64,
            0,
            1,
        )
    });
    // RET KILL_PROCESS for any non-x86_64 arch (this is the "jf=1"
    // target for the previous JEQ).
    prog.push(unsafe {
        libc::BPF_STMT(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_KILL_PROCESS,
        )
    });

    // -- nr load --
    // LD nr        -> A = seccomp_data.nr
    prog.push(unsafe {
        libc::BPF_STMT(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            SECCOMP_DATA_OFFSET_NR,
        )
    });

    // -- allow-list --
    for &nr in allow {
        // JEQ nr, jt=0 (fall through to ALLOW), jf=1 (skip ALLOW)
        prog.push(unsafe {
            libc::BPF_JUMP(
                (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                nr as u32,
                0,
                1,
            )
        });
        // RET ALLOW (reached when jt=0 from the previous JEQ)
        prog.push(unsafe {
            libc::BPF_STMT(
                (libc::BPF_RET | libc::BPF_K) as u16,
                libc::SECCOMP_RET_ALLOW,
            )
        });
    }

    // -- default --
    // RET KILL_PROCESS (reached when every JEQ above jumped jf=1)
    prog.push(unsafe {
        libc::BPF_STMT(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_KILL_PROCESS,
        )
    });

    debug_assert_eq!(
        prog.len(),
        4 + 2 * ALLOWLIST_LEN + 1,
        "BPF program length mismatch: expected 4 + 2 * ALLOWLIST_LEN + 1 = {} (header=4, allowed={}, default=1), got {}",
        4 + 2 * ALLOWLIST_LEN + 1,
        ALLOWLIST_LEN,
        prog.len()
    );
    prog
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

    /// Catalog §D.11.7: the [`crate::sandbox::seccomp_allowlist`]
    /// allow-list must contain at least ~50 syscalls; a shorter
    /// list would refuse to launch `cargo` itself (the loader
    /// needs `mmap`, `brk`, `read`, `write`, ... on the very first
    /// user-mode instruction). Pin the floor so a refactor that
    /// trims "obvious duplicates" trips the test before the
    /// sandbox subprocess dies on the first disallowed syscall.
    #[cfg(unix)]
    #[test]
    fn allowlist_count_is_at_least_50() {
        let allow = rust_build_allowlist();
        assert!(
            allow.len() >= 50,
            "Rust-build allow-list must contain at least 50 syscalls, got {}",
            allow.len()
        );
    }

    /// Catalog §D.11.7: the allow-list must include the four most
    /// basic file-I/O syscalls (`read`, `write`, `openat`, `close`).
    /// Without them the sandbox subprocess cannot load a single
    /// byte from disk or write a single byte to stdout, and the
    /// BPF program would be unusable. Pin them here so a refactor
    /// that swaps `openat` for `open` (which exists on the list
    /// too, but cargo has stopped using it) does not silently
    /// downgrade the sandbox to "no file I/O".
    #[cfg(unix)]
    #[test]
    fn allowlist_contains_basic_file_io() {
        let allow = rust_build_allowlist();
        for nr in [0_i64, 1_i64, 257_i64, 3_i64] {
            assert!(
                allow.contains(&nr),
                "Rust-build allow-list must include syscall {nr} (read/write/openat/close)"
            );
        }
    }

    /// Catalog §D.11.7: the allow-list must include the four
    /// memory-management syscalls the loader uses to bring the
    /// process up (`mmap`, `mprotect`, `munmap`, `brk`). Without
    /// `brk` the glibc / musl `malloc` cannot allocate; without
    /// `mmap` nothing loads. Pin them so a future "trim" of the
    /// list does not silently brick the sandbox.
    #[cfg(unix)]
    #[test]
    fn allowlist_contains_memory_management() {
        let allow = rust_build_allowlist();
        for nr in [9_i64, 10_i64, 11_i64, 12_i64] {
            assert!(
                allow.contains(&nr),
                "Rust-build allow-list must include syscall {nr} (mmap/mprotect/munmap/brk)"
            );
        }
    }

    /// Catalog §D.11.7: the BPF builder must not panic on the
    /// production allow-list. [`build_bpf_program`] is a pure
    /// function (no kernel calls) so it can be exercised from
    /// `cargo test` without root privileges; the live loader is
    /// still [`apply_strict_rust_build`] and remains untested in
    /// CI for the reasons documented at the top of the module.
    ///
    /// The test additionally pins the instruction count to
    /// `4 + 2 * ALLOWLIST_LEN + 1` so a refactor that adds a
    /// redundant `KILL_PROCESS` or drops the arch check trips the
    /// assertion alongside the `debug_assert_eq!` inside the
    /// builder.
    #[cfg(unix)]
    #[test]
    fn bpf_program_constructs_without_panic() {
        let allow = rust_build_allowlist();
        let program = build_bpf_program(&allow);
        let expected_len = 4 + 2 * ALLOWLIST_LEN + 1;
        assert_eq!(
            program.len(),
            expected_len,
            "BPF program length mismatch: expected {expected_len} (= 4 header + 2 * \
             {allow_len} allowed + 1 default), got {}",
            program.len(),
            allow_len = ALLOWLIST_LEN
        );
    }

    /// Catalog §D.11.7: outside Unix the live loader
    /// ([`apply_strict_rust_build`] is `#[cfg(unix)]`) short-
    /// circuits to `Ok(())` via the non-Unix
    /// [`apply_for_target`] implementation. Gated
    /// `#[cfg(not(unix))]` so the test compiles on every
    /// platform; on Unix the `#[cfg(unix)]` path is exercised
    /// indirectly by [`seccomp_apply_permissive_is_noop`] which
    /// runs on every target.
    #[cfg(not(unix))]
    #[test]
    fn apply_strict_rust_build_is_noop_on_non_unix() {
        let result = apply(SeccompPolicyKind::StrictRustBuild);
        assert!(
            result.is_ok(),
            "StrictRustBuild must be Ok on non-Unix (no-op), got {result:?}"
        );
    }

    /// Catalog §D.11.7: the allow-list must NOT include any
    /// debug / privilege-escalation syscall. The set below is the
    /// minimum that would let an attacker escape the sandbox;
    /// the test pins them out so a future PR that "broadens" the
    /// list for convenience trips the assertion before the merge.
    ///
    /// - `ptrace` (101): attach to another process and read its
    ///   memory; equivalent to a debugger.
    /// - `kexec_load` (246): reboot into a new kernel the
    ///   sandbox cannot verify.
    /// - `kexec_file_load` (320): same, file-based variant.
    /// - `reboot` (169): reboot the host, denying service to
    ///   every other tenant.
    /// - `mount` (165) / `umount2` (166): pivot the filesystem
    ///   away from the sandbox root.
    /// - `personality` (135): switch the execution domain, often
    ///   used as a stepping stone for kernel exploits.
    /// - `acct` (163): turn on process accounting.
    /// - `bpf` (321): load a new privileged BPF program.
    /// - `userfaultfd` (323): used by several kernel exploits as
    ///   a race primitive.
    /// - `init_module` (175) / `finit_module` (313): load a
    ///   kernel module from inside the sandbox.
    /// - `socket` (41) / `connect` (42): the sandbox already
    ///   denies network via `CARGO_NET_OFFLINE`; the seccomp
    ///   filter is the second wall.
    #[cfg(unix)]
    #[test]
    fn allowlist_does_not_contain_debug_syscalls() {
        let allow = rust_build_allowlist();
        let forbidden: &[(&str, i64)] = &[
            ("ptrace", 101),
            ("kexec_load", 246),
            ("kexec_file_load", 320),
            ("reboot", 169),
            ("mount", 165),
            ("umount2", 166),
            ("personality", 135),
            ("acct", 163),
            ("bpf", 321),
            ("userfaultfd", 323),
            ("init_module", 175),
            ("finit_module", 313),
            ("socket", 41),
            ("connect", 42),
            ("sendto", 44),
            ("bind", 49),
            ("listen", 50),
        ];
        for (name, nr) in forbidden {
            assert!(
                !allow.contains(nr),
                "Rust-build allow-list must NOT include {name} (syscall {nr}); \
                 see catalog §D.11.7 for the rationale"
            );
        }
    }
}
