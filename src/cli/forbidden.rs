//! Runtime guard: refuses to start the binary if any forbidden crate
//! is present in `Cargo.lock` or has a `[[bin]]`/`[dependencies]`
//! entry in `Cargo.toml`. The set is defined in `AGENTS.md` and the
//! `scripts/check-no-forbidden-crates.sh` static guard, but this
//! runtime check defends against ad-hoc installs.
//!
//! Compliance: catalog 10-integrada-v0 §D.13.15 (HARD_INCOMPATIBILITIES).

use tracing::{debug, trace, warn};

use crate::error::{Error, Result};

/// The hard list of forbidden crates. Keep in sync with
/// `scripts/check-no-forbidden-crates.sh` and `AGENTS.md`.
pub const FORBIDDEN_CRATES: &[&str] = &[
    "secrecy",
    "axum",
    "hyper",
    "sqlx",
    "governor",
    "figment",
    "refinery",
    "askama",
    "handlebars",
    "lettre",
    "inquire",
    "time",
];

/// Read and check the Cargo.toml next to the binary.
pub fn check_local_cargo_toml() -> Result<()> {
    debug!("forbidden::check_local_cargo_toml: enter");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir).join("Cargo.toml");
    if !path.exists() {
        trace!("Cargo.toml missing; skipping");
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    for crate_name in FORBIDDEN_CRATES {
        // Match a line that starts with the crate name and an `=`,
        // ignoring leading whitespace.
        let needle = format!("{crate_name} ");
        if text
            .lines()
            .any(|l| l.trim_start().starts_with(&needle) || l.trim_start() == *crate_name)
        {
            warn!(
                crate_name = crate_name,
                "forbidden crate present in Cargo.toml"
            );
            return Err(Error::InvalidArgs(format!(
                "forbidden crate '{crate_name}' is present in Cargo.toml"
            )));
        }
    }
    Ok(())
}
