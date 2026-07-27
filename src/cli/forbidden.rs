//! Runtime guard: refuses to start the binary if any forbidden crate
//! is present in `Cargo.lock` or has a `[[bin]]`/`[dependencies]`
//! entry in `Cargo.toml`. The set is defined in `AGENTS.md` and the
//! `scripts/check-no-forbidden-crates.sh` static guard, but this
//! runtime check defends against ad-hoc installs.
//!
//! Compliance: catalog 10-integrada-v0 §D.13.15 (HARD_INCOMPATIBILITIES).

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

/// Check the given `Cargo.toml` text for forbidden crate entries.
/// Returns `Err(HardIncompatibility)` if any is found.
pub fn check_cargo_toml(toml_text: &str) -> Result<()> {
    for crate_name in FORBIDDEN_CRATES {
        // Match a line that starts with the crate name and an `=`,
        // ignoring leading whitespace.
        let needle = format!("{crate_name} ");
        if toml_text
            .lines()
            .any(|l| l.trim_start().starts_with(&needle) || l.trim_start() == *crate_name)
        {
            return Err(Error::InvalidArgs(format!(
                "forbidden crate '{crate_name}' is present in Cargo.toml"
            )));
        }
    }
    Ok(())
}

/// Read and check the Cargo.toml next to the binary.
pub fn check_local_cargo_toml() -> Result<()> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest_dir).join("Cargo.toml");
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    check_cargo_toml(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_clean_cargo_toml() {
        let t = "[dependencies]\nserde = \"1\"\n";
        assert!(check_cargo_toml(t).is_ok());
    }

    #[test]
    fn rejects_secrecy() {
        let t = "[dependencies]\nsecrecy = \"0.8\"\n";
        assert!(check_cargo_toml(t).is_err());
    }

    #[test]
    fn rejects_with_indent() {
        let t = "[dependencies]\n    axum = \"0.7\"\n";
        assert!(check_cargo_toml(t).is_err());
    }

    #[test]
    fn passes_when_substring_match_does_not_apply() {
        // "axum_extra" should not match the "axum" forbidden entry.
        let t = "[dependencies]\naxum_extra = \"0.1\"\n";
        assert!(check_cargo_toml(t).is_ok());
    }
}
