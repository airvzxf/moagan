//! Unified API-key resolution with `api_keys.toml` precedence.
//!
//! Lookup order per provider kind (`"minimax"`, `"deepseek"`,
//! `"opencode"`):
//!
//! 1. `<MOAGAN_HOME>/api_keys.toml` entry for the kind, parsed by
//!    [`super::api_keys_file::ApiKeysFile`]. The spec string is one
//!    of:
//!      - `env:<VAR>`      → read env var `<VAR>` (the common case).
//!      - `file:/abs/path` → read first line, trim whitespace.
//!      - literal          → only honoured when
//!        `MOAGAN_API_KEY_ALLOW_LITERAL=1` (off by default).
//! 2. Direct env var fallback (today's behaviour):
//!      - `minimax`         → `MINIMAX_API_KEY`
//!      - `deepseek`        → `DEEPSEEK_API_KEY`
//!      - `opencode`        → `OPENCODE_API_KEY`
//!
//! If both (1) and (2) are present, the `api_keys.toml` value wins
//! — the file is the operator's explicit override. A spec that
//! fails to resolve (e.g. `env:NAME` with `NAME` unset) is reported
//! as `Err(_)` rather than silently falling back to the env var:
//! the operator wrote the spec for a reason.
//!
//! See `src/llm/api_keys_file.rs` for the on-disk format and the
//! underlying parser.

use std::path::PathBuf;

use crate::error::Error;
use crate::fs_layout::MoaganHome;

use super::api_keys_file::{ApiKeysFile, literal_allowed};

/// Canonical per-kind env var name. `None` for kinds that do not
/// need a key (currently just `mock`).
fn env_var_for(kind: &str) -> Option<&'static str> {
    match kind {
        "minimax" => Some("MINIMAX_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "opencode" => Some("OPENCODE_API_KEY"),
        _ => None,
    }
}

/// Read the direct env var, ignoring blank values (mirrors the
/// previous `from_config` behaviour: a whitespace-only export was
/// treated as absent).
fn env_var_value(env_var: &str) -> Option<String> {
    std::env::var(env_var).ok().filter(|s| !s.trim().is_empty())
}

/// Look up the API key for `kind` (e.g. `"minimax"`, `"deepseek"`,
/// `"opencode_go"`).
///
/// Returns:
///
/// - `Some(Ok(value))` — resolved via the `api_keys.toml` spec or
///   the direct env var fallback.
/// - `Some(Err(_))` — a spec was configured but unresolvable (e.g.
///   `env:NAME` with `NAME` unset, `file:` path missing/unreadable,
///   literal spec without `MOAGAN_API_KEY_ALLOW_LITERAL`). The
///   provider constructor surfaces this as `Error::InvalidApiKey`.
/// - `None` — neither a spec nor the env var yielded a value.
///
/// `home_override` lets tests pin the `api_keys.toml` location
/// without mutating the `MOAGAN_HOME` env var; production callers
/// pass `None`.
pub fn lookup_key(
    kind: &str,
    home_override: Option<&std::path::Path>,
) -> Option<Result<String, Error>> {
    let env_var = env_var_for(kind)?;
    let home_path: PathBuf = match home_override {
        Some(p) => p.to_path_buf(),
        None => match MoaganHome::resolve() {
            Ok(h) => h.root().to_path_buf(),
            Err(_) => return env_var_value(env_var).map(Ok),
        },
    };
    let file = ApiKeysFile::load(&home_path);
    if let Some(spec) = file.providers.get(kind) {
        // The spec is authoritative. We do NOT fall back to the
        // direct env var when the spec fails — the operator chose
        // this entry explicitly.
        return Some(resolve_spec(spec, env_var));
    }
    env_var_value(env_var).map(Ok)
}

fn resolve_spec(spec: &str, env_var: &str) -> Result<String, Error> {
    let trimmed_spec = spec.trim();
    if let Some(rest) = trimmed_spec.strip_prefix("env:") {
        let var = rest.trim();
        std::env::var(var).ok().filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            Error::InvalidApiKey {
                message: format!(
                    "api_keys.toml spec {spec:?} requested env var {var:?} for {env_var:?}, which is unset or blank"
                ),
                http_status: None,
            }
        })
    } else if let Some(rest) = trimmed_spec.strip_prefix("file:") {
        let path = PathBuf::from(rest.trim());
        std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::InvalidApiKey {
                    message: format!(
                        "api_keys.toml spec {spec:?} requested file {path:?} for {env_var:?}, which could not be read or was empty"
                    ),
                    http_status: None,
                }
            })
    } else if literal_allowed() {
        if trimmed_spec.is_empty() {
            Err(Error::InvalidApiKey {
                message: format!("api_keys.toml literal spec for {env_var:?} was empty"),
                http_status: None,
            })
        } else {
            Ok(trimmed_spec.to_string())
        }
    } else {
        Err(Error::InvalidApiKey {
            message: format!(
                "api_keys.toml spec {spec:?} for {env_var:?} is a literal but MOAGAN_API_KEY_ALLOW_LITERAL is not set"
            ),
            http_status: None,
        })
    }
}

/// Resolve the api-keys table key for a `ResolvedModelConfig`.
///
/// v0.10 (post-Phase 8 cleanup): the section name IS the canonical
/// provider-family key — the v0.9 / Phase 1 `kind` tag is gone.
/// `api_keys.toml` is keyed on the same name the operator passes
/// to `--provider` (e.g. `minimax`, `opencode`, `deepseek`), and
/// the env-var fallback (`MINIMAX_API_KEY` / `OPENCODE_API_KEY` /
/// `DEEPSEEK_API_KEY`) is the uppercased `_API_KEY`-suffixed
/// version of that name.
pub(crate) fn lookup_kind_for_resolved(resolved: &crate::config::ResolvedModelConfig) -> String {
    resolved.section.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_API_KEYS_LOCK;

    /// RAII guard that snapshots every API-key env var on creation
    /// and restores them on Drop. A test failure cannot leak mutated
    /// env vars into the next test on the same thread.
    struct EnvGuard {
        minimax: Option<String>,
        deepseek: Option<String>,
        opencode: Option<String>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                minimax: std::env::var("MINIMAX_API_KEY").ok(),
                deepseek: std::env::var("DEEPSEEK_API_KEY").ok(),
                opencode: std::env::var("OPENCODE_API_KEY").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            restore_or_remove("MINIMAX_API_KEY", self.minimax.as_deref());
            restore_or_remove("DEEPSEEK_API_KEY", self.deepseek.as_deref());
            restore_or_remove("OPENCODE_API_KEY", self.opencode.as_deref());
        }
    }

    fn restore_or_remove(name: &str, snapshot: Option<&str>) {
        match snapshot {
            Some(v) => unsafe {
                std::env::set_var(name, v);
            },
            None => unsafe {
                std::env::remove_var(name);
            },
        }
    }

    fn write_keys(home: &PathBuf, body: &str) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("api_keys.toml"), body).unwrap();
    }

    /// SAFETY: every test in this module owns a uniquely-named env
    /// var (prefix `MOAGAN_API_KEYS_TEST_*`) so cross-test pollution
    /// is impossible without `unsafe { set_var }`.
    fn unique_env(prefix: &str) -> String {
        format!("MOAGAN_API_KEYS_TEST_{prefix}_{}", std::process::id())
    }

    #[test]
    fn lookup_key_env_spec_resolves() {
        let _lock = TEST_API_KEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let env_name = unique_env("ENV_SPEC");
        write_keys(
            &tmp.path().to_path_buf(),
            &format!("[providers]\nminimax = \"env:{env_name}\"\n"),
        );
        unsafe {
            std::env::set_var(&env_name, "sk-cp-test");
        }
        let got = lookup_key("minimax", Some(tmp.path()))
            .expect("env spec resolves")
            .expect("Ok");
        assert_eq!(got, "sk-cp-test");
        unsafe {
            std::env::remove_var(&env_name);
        }
    }

    #[test]
    fn lookup_key_file_spec_resolves_and_trims() {
        let _lock = TEST_API_KEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let keyfile = tmp.path().join("secret.txt");
        std::fs::write(&keyfile, "  sk-from-file  \n").unwrap();
        write_keys(
            &tmp.path().to_path_buf(),
            &format!("[providers]\nminimax = \"file:{}\"\n", keyfile.display()),
        );
        let got = lookup_key("minimax", Some(tmp.path()))
            .expect("file spec resolves")
            .expect("Ok");
        assert_eq!(got, "sk-from-file");
    }

    #[test]
    fn lookup_key_spec_overrides_direct_env_var() {
        // If api_keys.toml points at one env var, the direct
        // env-var fallback for the kind must NOT be consulted.
        let _lock = TEST_API_KEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let env_name = unique_env("SPEC_WINS");
        write_keys(
            &tmp.path().to_path_buf(),
            &format!("[providers]\nminimax = \"env:{env_name}\"\n"),
        );
        unsafe {
            std::env::set_var(&env_name, "from-spec");
            std::env::set_var("MINIMAX_API_KEY", "should-be-ignored");
        }
        let got = lookup_key("minimax", Some(tmp.path()))
            .expect("env spec resolves")
            .expect("Ok");
        assert_eq!(got, "from-spec");
        unsafe {
            std::env::remove_var(&env_name);
            std::env::remove_var("MINIMAX_API_KEY");
        }
    }

    #[test]
    fn lookup_key_missing_spec_falls_back_to_env() {
        // No entry in api_keys.toml → direct env var (today's
        // behaviour) must still work.
        let _lock = TEST_API_KEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        write_keys(
            &tmp.path().to_path_buf(),
            "[providers]\nother = \"env:NOPE\"\n",
        );
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "from-env");
        }
        let got = lookup_key("minimax", Some(tmp.path()))
            .expect("env fallback resolves")
            .expect("Ok");
        assert_eq!(got, "from-env");
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
        }
    }

    #[test]
    fn lookup_key_returns_none_when_nothing_configured() {
        let _lock = TEST_API_KEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
        }
        let result = lookup_key("minimax", Some(tmp.path()));
        assert!(
            result.is_none(),
            "no spec + no env var must yield None, got {result:?}"
        );
    }

    #[test]
    fn lookup_key_returns_err_when_spec_unresolvable() {
        let _lock = TEST_API_KEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let env_name = unique_env("UNSET");
        write_keys(
            &tmp.path().to_path_buf(),
            &format!("[providers]\nminimax = \"env:{env_name}\"\n"),
        );
        unsafe {
            std::env::remove_var(&env_name);
        }
        let result =
            lookup_key("minimax", Some(tmp.path())).expect("spec was configured, must return Some");
        let err = result.expect_err("missing env var must surface Err");
        assert!(matches!(err, Error::InvalidApiKey { .. }));
    }

    #[test]
    fn lookup_key_unknown_kind_returns_none() {
        // "mock" and other unknown kinds are not keyed, so the
        // helper returns None and the caller (registry) treats it
        // as "no key needed".
        let tmp = tempfile::tempdir().unwrap();
        let got = lookup_key("mock", Some(tmp.path()));
        assert!(got.is_none(), "mock must not require a key, got {got:?}");
    }
}
