//! D.35.3: `api_keys.toml` precedence chain.
//!
//! Reads `<MOAGAN_HOME>/api_keys.toml` if present. Format:
//!
//! ```toml
//! [providers]
//! minimax = "env:MINIMAX_API_KEY"
//! openai_compat = "file:/path/to/key"
//! ```
//!
//! Resolution precedence: CLI spec > `api_keys.toml` > env var.
//! Literal keys (anything that does not start with `env:` or
//! `file:`) are only honoured when `MOAGAN_API_KEY_ALLOW_LITERAL`
//! is `1` or `true` (D.35.4).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// Parsed `api_keys.toml` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiKeysFile {
    /// Provider name → spec string. The spec can be `env:NAME`,
    /// `file:/path/to/key`, or a literal (gated by
    /// `MOAGAN_API_KEY_ALLOW_LITERAL`).
    #[serde(default)]
    pub providers: HashMap<String, String>,
}

impl ApiKeysFile {
    /// Load from `<home>/api_keys.toml`. Missing files, malformed
    /// TOML, and missing `[providers]` tables all yield a default
    /// (empty) file rather than an error — operators can hand-edit
    /// the file without the binary refusing to start.
    pub fn load(home: &Path) -> Self {
        let path = home.join("api_keys.toml");
        if !path.exists() {
            tracing::trace!(path = %path.display(), "ApiKeysFile: no file at path");
            return Self::default();
        }
        let raw = std::fs::read_to_string(&path).ok();
        let parsed = raw.as_deref().and_then(|s| toml::from_str::<Self>(s).ok());
        match parsed {
            Some(file) => {
                tracing::debug!(
                    path = %path.display(),
                    providers = file.providers.len(),
                    "ApiKeysFile: loaded"
                );
                file
            }
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "ApiKeysFile: parse failed; defaulting to empty"
                );
                Self::default()
            }
        }
    }

    /// Resolve the spec for `provider` against the given default
    /// `env_var`. Returns `None` if the provider has no entry in
    /// the file or if the entry cannot be materialised.
    pub fn resolve(&self, provider: &str, env_var: &str) -> Option<String> {
        let spec = self.providers.get(provider)?;
        tracing::trace!(provider, env_var, "ApiKeysFile: resolving spec");
        resolve_spec(spec, env_var)
    }
}

/// Resolve a single spec string into a concrete secret value.
/// `env_var` is the fallback used by the caller when no spec was
/// configured; it is intentionally unused when the spec itself is
/// an `env:` reference (which is fully self-contained).
fn resolve_spec(spec: &str, env_var: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix("env:") {
        let v = std::env::var(rest).ok();
        tracing::trace!(
            env = rest,
            present = v.is_some(),
            "ApiKeysFile: env spec resolve"
        );
        v
    } else if let Some(rest) = spec.strip_prefix("file:") {
        let v = std::fs::read_to_string(rest)
            .ok()
            .map(|s| s.trim().to_string());
        tracing::trace!(
            path = rest,
            present = v.is_some(),
            "ApiKeysFile: file spec resolve"
        );
        v
    } else {
        let _ = env_var;
        if literal_allowed() {
            tracing::debug!("ApiKeysFile: literal spec allowed");
            Some(spec.to_string())
        } else {
            tracing::debug!("ApiKeysFile: literal spec blocked (opt-in not set)");
            None
        }
    }
}

/// D.35.4: literal key only allowed with `MOAGAN_API_KEY_ALLOW_LITERAL`.
/// Off by default so committing a `api_keys.toml` with a literal key
/// into a repo is a no-op until the operator opts in.
pub fn literal_allowed() -> bool {
    std::env::var("MOAGAN_API_KEY_ALLOW_LITERAL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// D.35.5: first-use lazy resolution. The lookup happens once, on
/// the first call to [`LazyApiKey::resolve`], and the result is
/// cached in a [`std::sync::OnceLock`] so subsequent calls are
/// branch-free.
pub struct LazyApiKey {
    spec: String,
    env_var: String,
    cached: std::sync::OnceLock<Result<String, String>>,
}

impl LazyApiKey {
    /// Build a lazy key that resolves only from the env var
    /// `env_var`. Equivalent to the v0.1 behaviour.
    pub fn new(env_var: &str) -> Self {
        tracing::debug!(env_var, "LazyApiKey: constructed (env fallback)");
        Self {
            spec: String::new(),
            env_var: env_var.to_string(),
            cached: std::sync::OnceLock::new(),
        }
    }

    /// Resolve the key, or return a stable `"not found"` error
    /// string when neither the spec nor the env var yield a value.
    /// After the first call, the answer is served from the cache.
    pub fn resolve(&self) -> Result<&str, &str> {
        let cached = self.cached.get_or_init(|| {
            let result = if !self.spec.is_empty() {
                resolve_spec(&self.spec, &self.env_var)
            } else {
                std::env::var(&self.env_var).ok()
            };
            match result {
                Some(value) => {
                    tracing::debug!(env_var = %self.env_var, "LazyApiKey: resolved ok");
                    Ok(value)
                }
                None => {
                    tracing::warn!(env_var = %self.env_var, "LazyApiKey: resolution failed (not found)");
                    Err("not found".to_string())
                }
            }
        });
        cached.as_ref().map(|s| s.as_str()).map_err(|e| e.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_keys(home: &Path, body: &str) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("api_keys.toml"), body).unwrap();
    }

    #[test]
    fn api_keys_file_loads_from_path() {
        let tmp = tempfile::tempdir().unwrap();
        write_keys(
            tmp.path(),
            r#"[providers]
minimax = "env:MINIMAX_API_KEY"
"#,
        );
        let file = ApiKeysFile::load(tmp.path());
        assert_eq!(
            file.providers.get("minimax").map(String::as_str),
            Some("env:MINIMAX_API_KEY"),
        );
    }

    #[test]
    fn api_keys_file_resolves_env_spec() {
        let tmp = tempfile::tempdir().unwrap();
        write_keys(
            tmp.path(),
            r#"[providers]
minimax = "env:LAZY_TEST_ENV_KEY_35_3"
"#,
        );
        // SAFETY: env var access is serialised by the test runner
        // only via distinct key names; this test owns LAZY_TEST_*.
        unsafe {
            std::env::set_var("LAZY_TEST_ENV_KEY_35_3", "sk-cp-test");
        }
        let file = ApiKeysFile::load(tmp.path());
        let got = file
            .resolve("minimax", "MINIMAX_API_KEY")
            .expect("env spec resolves");
        assert_eq!(got, "sk-cp-test");
        unsafe {
            std::env::remove_var("LAZY_TEST_ENV_KEY_35_3");
        }
    }

    #[test]
    fn api_keys_file_resolves_file_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let keyfile = tmp.path().join("secret.txt");
        std::fs::write(&keyfile, "  sk-from-file  \n").unwrap();
        write_keys(
            tmp.path(),
            &format!("[providers]\nminimax = \"file:{}\"\n", keyfile.display()),
        );
        let file = ApiKeysFile::load(tmp.path());
        let got = file
            .resolve("minimax", "MINIMAX_API_KEY")
            .expect("file spec resolves");
        assert_eq!(got, "sk-from-file", "file contents are trimmed");
    }

    #[test]
    fn api_keys_file_literal_blocked_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        write_keys(
            tmp.path(),
            r#"[providers]
minimax = "sk-cp-literal"
"#,
        );
        let file = ApiKeysFile::load(tmp.path());
        // Without MOAGAN_API_KEY_ALLOW_LITERAL the literal is rejected.
        let prev = std::env::var("MOAGAN_API_KEY_ALLOW_LITERAL").ok();
        unsafe {
            std::env::remove_var("MOAGAN_API_KEY_ALLOW_LITERAL");
        }
        assert!(
            file.resolve("minimax", "MINIMAX_API_KEY").is_none(),
            "literal key must be rejected when env opt-in is unset"
        );
        unsafe {
            std::env::set_var("MOAGAN_API_KEY_ALLOW_LITERAL", "1");
        }
        assert_eq!(
            file.resolve("minimax", "MINIMAX_API_KEY").as_deref(),
            Some("sk-cp-literal"),
            "literal key is honoured when MOAGAN_API_KEY_ALLOW_LITERAL=1"
        );
        match prev {
            Some(v) => unsafe {
                std::env::set_var("MOAGAN_API_KEY_ALLOW_LITERAL", v);
            },
            None => unsafe {
                std::env::remove_var("MOAGAN_API_KEY_ALLOW_LITERAL");
            },
        }
    }

    #[test]
    fn api_keys_file_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let file = ApiKeysFile::load(tmp.path());
        assert!(file.providers.is_empty());
        assert!(file.resolve("minimax", "MINIMAX_API_KEY").is_none());
    }

    #[test]
    fn lazy_api_key_caches_resolution() {
        // SAFETY: distinct key name to avoid stepping on other tests.
        unsafe {
            std::env::set_var("LAZY_TEST_ENV_KEY_35_5", "first");
        }
        let key = LazyApiKey::new("LAZY_TEST_ENV_KEY_35_5");
        assert_eq!(key.resolve().unwrap(), "first");
        // Mutate the env after the first resolve: the cache must
        // keep returning the original value.
        unsafe {
            std::env::set_var("LAZY_TEST_ENV_KEY_35_5", "second");
        }
        assert_eq!(
            key.resolve().unwrap(),
            "first",
            "lazy resolution must cache the first result"
        );
        unsafe {
            std::env::remove_var("LAZY_TEST_ENV_KEY_35_5");
        }
    }
}
