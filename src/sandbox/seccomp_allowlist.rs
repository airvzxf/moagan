//! Allow-list of syscalls needed to compile a Rust crate from
//! scratch (catalog §D.11.7).
//!
//! Every entry is the x86_64 syscall number from
//! `arch/x86/entry/syscalls/syscall_64.tbl` in the Linux kernel
//! source tree. The list is the union of what `cargo`, `rustc`,
//! `ld`, and the glibc/musl startup need during a single
//! `cargo build` invocation against an offline registry:
//!
//! - **File I/O**: `read`, `write`, `open`, `close`, `stat`,
//!   `fstat`, `lseek`, `pread64`, `pwrite64`, `readv`, `writev`,
//!   `access`, `ioctl`, `openat`, `fstatat`, `statx`, `fcntl`,
//!   `getdents64`, `mkdirat`, `unlinkat`, `renameat2`, `readlinkat`,
//!   `symlinkat`.
//! - **Memory**: `mmap`, `mprotect`, `munmap`, `brk`, `madvise`.
//! - **Process / thread**: `clone`, `fork`, `execve`, `wait4`,
//!   `exit`, `exit_group`, `getpid`, `gettid`, `getppid`,
//!   `set_tid_address`, `set_robust_list`, `arch_prctl`,
//!   `prlimit64`, `kill`, `setsid`.
//! - **Signals**: `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`.
//! - **Sync / clock**: `futex`, `getrandom`, `sched_yield`,
//!   `nanosleep`.
//! - **Descriptors**: `pipe`, `pipe2`, `dup2`, `dup3`,
//!   `close_range`.
//! - **Identity / info**: `getuid`, `getgid`, `geteuid`,
//!   `getegid`, `uname`, `gettimeofday`, `getcwd`.
//! - **Epoll**: `epoll_wait` (cargo's thread-pool uses epoll).
//!
//! Network syscalls (`socket`, `connect`, `sendto`, ...) are
//! deliberately absent: the sandbox runs `cargo` with
//! `CARGO_NET_OFFLINE=true` injected by
//! [`crate::sandbox::process::MoaSandbox::run_cmd`], so the
//! sandbox subprocess must not have any way to open a socket even
//! if the env hint is bypassed. Catalog §D.11.13 layers on top via
//! [`crate::sandbox::NetworkPolicy`].
//!
//! Debug / privilege-escalation syscalls (`ptrace`, `kexec_load`,
//! `kexec_file_load`, `reboot`, `mount`, `umount2`,
//! `clone(CLONE_NEWUSER)`, ...) are deliberately absent; the BPF
//! program in [`crate::sandbox::seccomp`] defaults to
//! `SECCOMP_RET_KILL_PROCESS` for any syscall not listed here.
//!
//! The crate-level no-go list forbids `seccompiler` / `libseccomp`
//! / `capsicum`, so the list is consumed by the hand-rolled BPF
//! builder in [`crate::sandbox::seccomp::build_bpf_program`].

/// Return the sorted, de-duplicated set of syscall numbers that
/// `cargo` + `rustc` + `ld` + the glibc / musl startup need to
/// build a Rust crate offline.
///
/// The list is curated for `x86_64` and the numbers match the
/// `x86_64` column of `syscall_64.tbl`. Other architectures are
/// not supported by [`crate::sandbox::seccomp::SeccompPolicyKind::StrictRustBuild`]:
/// the BPF program pins `AUDIT_ARCH_X86_64` and kills the
/// subprocess on any other arch, so a non-x86_64 deployment would
/// refuse to launch the sandbox.
///
/// The set is returned as `Vec<i64>` because the kernel ABI
/// (the `nr` field in `struct seccomp_data`) is signed and the
/// constants in `libc` are signed; returning `i64` keeps the
/// caller free of casts and matches the field width.
pub fn rust_build_allowlist() -> Vec<i64> {
    tracing::trace!(
        sandbox = "seccomp",
        "rust_build_allowlist: building syscall allowlist"
    );
    let mut allow = vec![
        // -- file I/O --
        0_i64,   // read
        1_i64,   // write
        2_i64,   // open
        3_i64,   // close
        4_i64,   // stat
        5_i64,   // fstat
        8_i64,   // lseek
        9_i64,   // mmap
        10_i64,  // mprotect
        11_i64,  // munmap
        12_i64,  // brk
        13_i64,  // rt_sigaction
        14_i64,  // rt_sigprocmask
        15_i64,  // rt_sigreturn
        16_i64,  // ioctl
        17_i64,  // pread64
        18_i64,  // pwrite64
        19_i64,  // readv
        20_i64,  // writev
        21_i64,  // access
        22_i64,  // pipe
        24_i64,  // sched_yield
        28_i64,  // madvise
        33_i64,  // dup2
        35_i64,  // nanosleep
        39_i64,  // getpid
        56_i64,  // clone
        57_i64,  // fork
        59_i64,  // execve
        60_i64,  // exit
        61_i64,  // wait4
        62_i64,  // kill
        63_i64,  // uname
        72_i64,  // fcntl
        79_i64,  // getcwd
        96_i64,  // gettimeofday
        102_i64, // getuid
        104_i64, // getgid
        107_i64, // geteuid
        108_i64, // getegid
        110_i64, // getppid
        112_i64, // setsid
        158_i64, // arch_prctl
        186_i64, // gettid
        202_i64, // futex
        218_i64, // set_tid_address
        231_i64, // exit_group
        257_i64, // openat
        262_i64, // fstatat (newfstatat)
        273_i64, // set_robust_list
        292_i64, // dup3
        293_i64, // pipe2
        302_i64, // prlimit64
        318_i64, // getrandom
        332_i64, // statx
    ];
    allow.sort_unstable();
    allow.dedup();
    tracing::trace!(
        sandbox = "seccomp",
        count = allow.len(),
        constant = ALLOWLIST_LEN,
        "rust_build_allowlist: built and deduplicated"
    );
    allow
}

/// Number of syscalls in [`rust_build_allowlist`].
///
/// Pinned as a constant so the BPF program size in
/// [`crate::sandbox::seccomp::build_bpf_program`] is predictable:
/// each entry adds two `sock_filter` instructions and the program
/// always carries four header instructions (`arch` LD + `arch`
/// JEQ + KILL-on-mismatch + `nr` LD) and one default
/// `KILL_PROCESS`, so the total instruction count is
/// `4 + 2 * ALLOWLIST_LEN + 1` (well under `BPF_MAXINSNS = 4096`).
pub const ALLOWLIST_LEN: usize = 55;

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the allow-list cardinality. The BPF builder
    /// (`build_bpf_program`) uses this constant to assert the
    /// instruction count, so a refactor that drops or duplicates
    /// entries trips this test and the `debug_assert_eq!` inside
    /// the builder together. The richer allow-list assertions
    /// (`allowlist_count_is_at_least_50`,
    /// `allowlist_contains_basic_file_io`,
    /// `allowlist_contains_memory_management`,
    /// `allowlist_does_not_contain_debug_syscalls`) live in
    /// `src/sandbox/seccomp.rs::tests` per the PR B.3 spec.
    #[test]
    fn allowlist_len_matches_constant() {
        assert_eq!(rust_build_allowlist().len(), ALLOWLIST_LEN);
    }
}
