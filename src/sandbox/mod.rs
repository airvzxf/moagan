//! Subprocess sandbox for executable validation.
//!
//! Validators (Rust, Python, TypeScript, SQL) hand the proposal's
//! artefacts to a [`Sandbox`] and receive back a [`SandboxResult`]
//! with the captured stdout/stderr, exit code, and elapsed time.
//!
//! The first version is a subprocess plus timeout plus allowlist plus
//! denylist, as documented in `proposal-02-rust.md` §7. The D.11
//! hardening now includes bounded output, command profiles, argv
//! redaction, and binary preflight; namespace and resource controls
//! remain catalog overlays.
//!
//! See [`process::Sandbox`] for the entry point and
//! [`allowlist`] for the command policy.

pub mod allowlist;
pub mod process;

pub use allowlist::{
    Allowlist, DEFAULT_ALLOWLIST, DEFAULT_DENYLIST, Denylist, contains_deny_token, is_allowed,
};
pub use process::{
    COMMAND_CONFIGS, CommandConfig, Sandbox, SandboxConfig, SandboxError, SandboxResult,
    SandboxStatus, config_for, strip_secrets, verify_binary_exists,
};
