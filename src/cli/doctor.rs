//! `moagan doctor` — real environment checks.
//!
//! Verifies the local environment is ready to run a smoke:
//!   1. The provider API key is set (when the default provider is one
//!      that needs a key).
//!   2. `MOAGAN_HOME` resolves and is writable.
//!   3. The SQLite index can be opened and the schema is current.
//!
//! Each check prints a single line, prefixed with `[OK]`, `[WARN]` or
//! `[FAIL]`. The overall exit code is non-zero when any check fails.

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::fs_layout::MoaganHome;
use crate::storage::sqlite::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "[OK]",
            Self::Warn => "[WARN]",
            Self::Fail => "[FAIL]",
        }
    }
}

struct Check {
    name: String,
    status: Status,
    detail: String,
}

fn emit(check: Check, any_fail: &mut bool, any_warn: &mut bool) {
    if check.status == Status::Fail {
        *any_fail = true;
    }
    if check.status == Status::Warn {
        *any_warn = true;
    }
    println!("{} {:<32} {}", check.status.tag(), check.name, check.detail);
}

fn check_api_key(cfg: &Config) -> Check {
    use crate::llm::api_keys::lookup_key;
    let mut missing: Vec<String> = Vec::new();
    let mut seen_kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (name, spec) in &cfg.providers {
        // `mock` does not need an API key. Every other kind goes
        // through the unified lookup (api_keys.toml > env var).
        if spec.kind == "mock" {
            continue;
        }
        if !seen_kinds.insert(spec.kind.clone()) {
            // Skip duplicate kinds; one env var / api_keys.toml entry
            // services every provider alias of the same kind. The
            // first iteration reports it; subsequent iterations
            // (e.g. `minimax` + `minimax-m2.7`) just re-confirm.
            continue;
        }
        match lookup_key(&spec.kind, None) {
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                let kind = spec.kind.clone();
                missing.push(format!("{name} ({kind}: api_keys.toml spec unresolvable)"));
            }
            None => {
                let kind = spec.kind.clone();
                missing.push(format!(
                    "{name} ({kind}: env var unset and no api_keys.toml entry)"
                ));
            }
        }
    }
    if missing.is_empty() {
        Check {
            name: "api_key".to_string(),
            status: Status::Ok,
            detail: "all required provider API keys resolve (api_keys.toml > env var)".into(),
        }
    } else {
        Check {
            name: "api_key".to_string(),
            status: Status::Fail,
            detail: format!(
                "missing/unresolvable API keys for: {}; check env vars or <MOAGAN_HOME>/api_keys.toml",
                missing.join(", ")
            ),
        }
    }
}

fn check_home() -> Check {
    match MoaganHome::resolve() {
        Ok(home) => match home.ensure() {
            Ok(()) => {
                let path = home.root();
                let test_path: &Path = path;
                let probe = test_path.join(".moagan-doctor-probe");
                match std::fs::write(&probe, b"ok") {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&probe);
                        Check {
                            name: "home".to_string(),
                            status: Status::Ok,
                            detail: format!("writable at {}", path.display()),
                        }
                    }
                    Err(e) => Check {
                        name: "home".to_string(),
                        status: Status::Fail,
                        detail: format!("MOAGAN_HOME={} is not writable: {e}", path.display()),
                    },
                }
            }
            Err(e) => Check {
                name: "home".to_string(),
                status: Status::Fail,
                detail: format!("MOAGAN_HOME ensure failed: {e}"),
            },
        },
        Err(e) => Check {
            name: "home".to_string(),
            status: Status::Fail,
            detail: format!("MOAGAN_HOME could not be resolved: {e}"),
        },
    }
}

fn check_sqlite() -> Check {
    let home = match MoaganHome::resolve() {
        Ok(h) => h,
        Err(_) => {
            return Check {
                name: "sqlite".to_string(),
                status: Status::Warn,
                detail: "skipped: MOAGAN_HOME not resolvable".into(),
            };
        }
    };
    match Db::open(&home.meta_db_path()) {
        Ok(_db) => Check {
            name: "sqlite".to_string(),
            status: Status::Ok,
            detail: format!("opened at {}", home.meta_db_path().display()),
        },
        Err(e) => Check {
            name: "sqlite".to_string(),
            status: Status::Fail,
            detail: format!("open failed: {e}"),
        },
    }
}

fn check_provider_config(cfg: &Config) -> Check {
    if cfg.providers.is_empty() {
        return Check {
            name: "providers".to_string(),
            status: Status::Warn,
            detail: "no providers registered".into(),
        };
    }
    Check {
        name: "providers".to_string(),
        status: Status::Ok,
        detail: format!("{} provider(s) configured", cfg.providers.len()),
    }
}

/// Build the per-kind model summary. Returns a vector of
/// `(label, models)` pairs in stable (alphabetical) order; the caller
/// prints them with the standard check-line format. Q5: surfaces the
/// canonical MiniMax models (M3, M2.7, M2.7-highspeed, M2.5) so
/// operators know what `--provider minimax-m2.5` resolves to without
/// grepping the source. `kind` is the implementation name (e.g.
/// `minimax`, `mock`); `models` is the sorted list of distinct
/// `model` values across every provider entry of that kind.
fn models_per_provider(cfg: &Config) -> Vec<(String, Vec<String>)> {
    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for spec in cfg.providers.values() {
        by_kind
            .entry(spec.kind.clone())
            .or_default()
            .push(spec.model.clone());
    }
    by_kind
        .into_iter()
        .map(|(kind, mut models)| {
            models.sort();
            models.dedup();
            (format!("models for provider '{kind}'"), models)
        })
        .collect()
}

/// Run every check and return 0 if everything is OK, 1 otherwise.
pub fn run() -> Result<i32> {
    let cfg = Config::load()?;
    let mut any_fail = false;
    let mut any_warn = false;
    emit(check_provider_config(&cfg), &mut any_fail, &mut any_warn);
    // Q5: per-kind model listing. Printed in the same `[OK]` /
    // status-tag style as the other checks for grep-friendliness.
    for (label, models) in models_per_provider(&cfg) {
        let status = if models.is_empty() {
            Status::Warn
        } else {
            Status::Ok
        };
        emit(
            Check {
                name: label,
                status,
                detail: models.join(", "),
            },
            &mut any_fail,
            &mut any_warn,
        );
    }
    emit(check_api_key(&cfg), &mut any_fail, &mut any_warn);
    emit(check_home(), &mut any_fail, &mut any_warn);
    emit(check_sqlite(), &mut any_fail, &mut any_warn);
    if any_fail {
        println!();
        println!("doctor: FAIL — see [FAIL] lines above");
        Ok(1)
    } else if any_warn {
        println!();
        println!("doctor: WARN — see [WARN] lines above");
        Ok(0)
    } else {
        println!();
        println!("doctor: OK");
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;

    #[test]
    fn status_tags_are_stable() {
        assert_eq!(Status::Ok.tag(), "[OK]");
        assert_eq!(Status::Warn.tag(), "[WARN]");
        assert_eq!(Status::Fail.tag(), "[FAIL]");
    }

    /// Q5: `models_per_provider` returns one (label, models) pair per
    /// distinct provider kind, with the model list sorted /
    /// deduplicated. The label follows the `models for provider
    /// '<kind>'` contract that operators grep against in the doctor
    /// output.
    #[test]
    fn models_per_provider_groups_by_kind_and_dedupes() {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "minimax".into(),
            ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic/v1".into(),
                model: "MiniMax-M3".into(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "minimax-m2.7".into(),
            ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic/v1".into(),
                model: "MiniMax-M2.7".into(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "minimax-dup".into(),
            ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic/v1".into(),
                model: "MiniMax-M3".into(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "mock".into(),
            ProviderConfig {
                kind: "mock".into(),
                endpoint: "mock://local".into(),
                model: "mock-model".into(),
                ..ProviderConfig::default()
            },
        );
        let cfg = Config {
            providers,
            ..Config::default()
        };

        let entries = models_per_provider(&cfg);
        // Alphabetical by kind: minimax before mock.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "models for provider 'minimax'");
        assert_eq!(
            entries[0].1,
            vec!["MiniMax-M2.7".to_owned(), "MiniMax-M3".to_owned()]
        );
        assert_eq!(entries[1].0, "models for provider 'mock'");
        assert_eq!(entries[1].1, vec!["mock-model".to_owned()]);
    }

    // ----------------------------------------------------------------
    // PR-B2: `check_api_key` covers every keyed provider kind.
    //
    // These tests pin the contract that `moagan doctor` reports the
    // MINIMAX / DEEPSEEK / OPENCODE_GO env vars through the unified
    // `api_keys::lookup_key` helper (not just MINIMAX_API_KEY as the
    // pre-PR-B2 check did).
    // ----------------------------------------------------------------

    /// Locks every test in this module that mutates process-wide env
    /// state (provider API keys + `MOAGAN_HOME`). Acquired alongside
    /// the crate-wide `TEST_API_KEYS_LOCK` (in `src/lib.rs`) so
    /// parallel-running LLM provider tests cannot poison the env
    /// vars this module reads.
    static TEST_DOCTOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that snapshots every API-key env var on creation
    /// and restores them on Drop. A test failure cannot leak mutated
    /// env vars into the next test on the same thread.
    struct ApiKeyEnvGuard {
        minimax: Option<String>,
        deepseek: Option<String>,
        opencode_go: Option<String>,
        moagan_home: Option<std::ffi::OsString>,
    }

    impl ApiKeyEnvGuard {
        fn new() -> Self {
            Self {
                minimax: std::env::var("MINIMAX_API_KEY").ok(),
                deepseek: std::env::var("DEEPSEEK_API_KEY").ok(),
                opencode_go: std::env::var("OPENCODE_GO_API_KEY").ok(),
                moagan_home: std::env::var_os("MOAGAN_HOME"),
            }
        }
    }

    impl Drop for ApiKeyEnvGuard {
        fn drop(&mut self) {
            restore_or_remove("MINIMAX_API_KEY", self.minimax.as_deref());
            restore_or_remove("DEEPSEEK_API_KEY", self.deepseek.as_deref());
            restore_or_remove("OPENCODE_GO_API_KEY", self.opencode_go.as_deref());
            if let Some(v) = &self.moagan_home {
                unsafe {
                    std::env::set_var("MOAGAN_HOME", v);
                }
            } else {
                unsafe {
                    std::env::remove_var("MOAGAN_HOME");
                }
            }
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

    /// Build a minimal `Config` that contains every provider kind
    /// `moagan` registers by default. Used as the substrate for the
    /// `check_api_key` tests so the helper sees the full set of
    /// kinds it must iterate over.
    fn three_kind_config() -> Config {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "minimax".into(),
            ProviderConfig {
                kind: "minimax".into(),
                endpoint: "https://api.minimax.io/anthropic/v1".into(),
                model: "MiniMax-M3".into(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "deepseek".into(),
            ProviderConfig {
                kind: "deepseek".into(),
                endpoint: "https://api.deepseek.com/v1".into(),
                model: "deepseek-v4-flash".into(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "opencode_go".into(),
            ProviderConfig {
                kind: "opencode_go".into(),
                endpoint: "https://opencode.ai/zen/go/v1".into(),
                model: "kimi-k2.7-code".into(),
                ..ProviderConfig::default()
            },
        );
        Config {
            providers,
            ..Config::default()
        }
    }

    #[test]
    fn check_api_key_passes_when_all_three_env_vars_set() {
        let _lock = TEST_DOCTOR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _api_lock = crate::TEST_API_KEYS_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _home_lock = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = ApiKeyEnvGuard::new();
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "sk-minimax");
            std::env::set_var("DEEPSEEK_API_KEY", "sk-deepseek");
            std::env::set_var("OPENCODE_GO_API_KEY", "sk-opencode");
            std::env::remove_var("MOAGAN_HOME");
        }
        let check = check_api_key(&three_kind_config());
        assert_eq!(
            check.status,
            Status::Ok,
            "doctor api_key check must be OK when all three env vars are set; detail: {}",
            check.detail
        );
        assert!(
            check
                .detail
                .contains("all required provider API keys resolve"),
            "OK detail must explain the resolution path; got: {}",
            check.detail
        );
    }

    #[test]
    fn check_api_key_fails_when_deepseek_key_missing() {
        let _lock = TEST_DOCTOR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _api_lock = crate::TEST_API_KEYS_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _home_lock = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = ApiKeyEnvGuard::new();
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "sk-minimax");
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::set_var("OPENCODE_GO_API_KEY", "sk-opencode");
            std::env::remove_var("MOAGAN_HOME");
        }
        let check = check_api_key(&three_kind_config());
        assert_eq!(
            check.status,
            Status::Fail,
            "missing DEEPSEEK_API_KEY must surface as Fail; detail: {}",
            check.detail
        );
        assert!(
            check.detail.contains("deepseek"),
            "Fail detail must name the missing kind; got: {}",
            check.detail
        );
    }

    #[test]
    fn check_api_key_fails_when_opencode_go_key_missing() {
        let _lock = TEST_DOCTOR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _api_lock = crate::TEST_API_KEYS_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _home_lock = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = ApiKeyEnvGuard::new();
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "sk-minimax");
            std::env::set_var("DEEPSEEK_API_KEY", "sk-deepseek");
            std::env::remove_var("OPENCODE_GO_API_KEY");
            std::env::remove_var("MOAGAN_HOME");
        }
        let check = check_api_key(&three_kind_config());
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains("opencode_go"),
            "Fail detail must name the missing kind; got: {}",
            check.detail
        );
    }

    #[test]
    fn check_api_key_passes_for_mock_only_config() {
        let _lock = TEST_DOCTOR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _api_lock = crate::TEST_API_KEYS_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _home_lock = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = ApiKeyEnvGuard::new();
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::remove_var("OPENCODE_GO_API_KEY");
            std::env::remove_var("MOAGAN_HOME");
        }
        // A mock-only config requires no keys; the check must
        // report OK regardless of the env-var state.
        let cfg = Config {
            providers: std::collections::BTreeMap::from([(
                "mock".to_owned(),
                ProviderConfig {
                    kind: "mock".to_owned(),
                    endpoint: "mock://local".to_owned(),
                    model: "mock-model".to_owned(),
                    ..ProviderConfig::default()
                },
            )]),
            ..Config::default()
        };
        let check = check_api_key(&cfg);
        assert_eq!(check.status, Status::Ok);
    }

    #[test]
    fn check_api_key_resolves_via_api_keys_toml() {
        // `api_keys.toml` is the operator's explicit override. When
        // the file contains the key spec, the doctor check must
        // accept it without requiring the direct env var fallback.
        let _lock = TEST_DOCTOR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _api_lock = crate::TEST_API_KEYS_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _home_lock = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env = ApiKeyEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("api_keys.toml"),
            r#"
[providers]
deepseek = "env:DOCTOR_TEST_DEEPSEEK_KEY_B2"
"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
            std::env::set_var("DOCTOR_TEST_DEEPSEEK_KEY_B2", "sk-from-file");
            std::env::set_var("MINIMAX_API_KEY", "sk-minimax");
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::set_var("OPENCODE_GO_API_KEY", "sk-opencode");
        }
        let check = check_api_key(&three_kind_config());
        assert_eq!(
            check.status,
            Status::Ok,
            "api_keys.toml entry must satisfy doctor; detail: {}",
            check.detail
        );
        unsafe {
            std::env::remove_var("DOCTOR_TEST_DEEPSEEK_KEY_B2");
        }
    }
}
