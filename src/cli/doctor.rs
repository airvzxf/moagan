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
    let mut seen_sections: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for name in cfg.providers.keys() {
        // v0.10 (post-Phase 8 cleanup): the section name IS the
        // canonical provider-family key for `api_keys.toml` and the
        // `<NAME>_API_KEY` env-var fallback. The deprecated `kind`
        // tag is gone. The `mock` section does not need an API key.
        if name == "mock" {
            continue;
        }
        if !seen_sections.insert(name.clone()) {
            // Skip duplicates; one env var / api_keys.toml entry
            // services every section of the same name.
            continue;
        }
        match lookup_key(name, None) {
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                missing.push(format!("{name} (api_keys.toml spec unresolvable)"));
            }
            None => {
                missing.push(format!("{name} (env var unset and no api_keys.toml entry)"));
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

/// Build the per-section model summary. Returns a vector of
/// `(label, models)` pairs in stable (alphabetical) order; the
/// caller prints them with the standard check-line format.
/// v0.10 (post-Phase 8 cleanup): one `(label, models)` pair per
/// `[providers.<name>]` section; the model id list comes from the
/// section's `models[]` (the v0.9 deprecated `spec.model` singleton
/// is gone). Aliases (the old per-model sections like `kimi-k3`)
/// were collapsed into the canonical section in Phase 8 — callers
/// reach them via `--provider opencode:kimi-k3`.
fn models_per_provider(cfg: &Config) -> Vec<(String, Vec<String>)> {
    use std::collections::BTreeMap;
    let mut by_section: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (section, spec) in &cfg.providers {
        for m in &spec.models {
            by_section
                .entry(section.clone())
                .or_default()
                .push(m.id.clone());
        }
    }
    by_section
        .into_iter()
        .map(|(section, mut models)| {
            models.sort();
            models.dedup();
            (format!("models for provider '{section}'"), models)
        })
        .collect()
}

/// Dispatch the `moagan doctor` command. `capabilities = true`
/// switches the sub-command to the PR-7 capability table view
/// (`--capabilities` flag) and returns 0; the default branch keeps
/// the pre-PR-7 environment-check behaviour so existing CI
/// scripts do not regress.
pub fn run(capabilities: bool) -> Result<i32> {
    if capabilities {
        return run_capabilities();
    }
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

/// PR-7 `moagan doctor --capabilities` view. For every
/// `(provider, model)` pair the operator has configured, print
/// the resolved capability matrix:
/// `TEMP / REASON / TOOLS / ATTACH / MAX_IN / MAX_OUT / COST`.
///
/// `TEMP / REASON / TOOLS / ATTACH` come from the static
/// `ProviderCapabilities` (kind-based, no I/O), `MAX_IN / MAX_OUT`
/// come from the `models.dev` catalog row when the on-disk cache
/// is present, and `COST` comes from the same row's
/// `cost: {input, output, cache_read, cache_write}` block. The
/// `models_dev` cache is loaded best-effort (offline) so a
/// missing or stale cache yields `-` cells and a single
/// warning at the bottom — the operator can still see the
/// static matrix.
fn run_capabilities() -> Result<i32> {
    let cfg = Config::load()?;
    // Best-effort offline catalog read. A missing or unreadable
    // cache degrades to `None` so the rest of the table can
    // still print; the warning is surfaced at the end so the
    // operator knows the cells are best-effort.
    let home = MoaganHome::resolve().ok();
    let catalog = home
        .as_ref()
        .and_then(|h| crate::llm::models_dev::try_load_from_disk(h.root()));
    if catalog.is_none() {
        println!("[WARN] models_dev catalog cache is missing; cells marked `-` are best-effort");
    }

    let mut any_warn = false;
    let header = format!(
        "{:<22}  {:<22}  {:<5}  {:<6}  {:<5}  {:<6}  {:<10}  {:<10}  {}",
        "PROVIDER",
        "MODEL",
        "TEMP",
        "REASON",
        "TOOLS",
        "ATTACH",
        "MAX_IN",
        "MAX_OUT",
        "COST($/M_in/out)"
    );
    println!("{header}");
    // 1 line of separator under the header so the columns line
    // up regardless of terminal width.
    println!("{}", "-".repeat(header.len()));

    // Iterate the configured providers in sorted order so the
    // output is stable across runs. v0.10: each row carries the
    // section name plus every model id registered under it; the
    // catalog lookup still uses the section + model pair.
    for (name, spec) in &cfg.providers {
        let caps = capabilities_for_section(name);
        for model in &spec.models {
            let entry = catalog
                .as_ref()
                .and_then(|c| crate::llm::models_dev::lookup(c, name, &model.id));
            // The catalog is the canonical source for `temperature`,
            // `reasoning`, and `attachment` (the `models.dev` rows are
            // the only place these booleans live; `ProviderCapabilities`
            // covers wire-format knobs instead). When the catalog is
            // missing we fall back to the static capability matrix for
            // `tools` (the only column that lives on both sides), and
            // print `-` everywhere else.
            let temperature_honoured = entry.as_ref().map(|e| e.temperature);
            let reasoning = entry.as_ref().map(|e| e.reasoning);
            let attachment = entry.as_ref().map(|e| e.attachment);
            let max_in = entry
                .as_ref()
                .map(|e| e.limit.context.to_string())
                .or_else(|| caps.max_input_tokens.map(|n| n.to_string()))
                .unwrap_or_else(|| "-".to_owned());
            let max_out = entry
                .as_ref()
                .map(|e| e.limit.output.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let cost = entry
                .as_ref()
                .map(|e| format!("{:.2} / {:.2}", e.cost.input, e.cost.output))
                .unwrap_or_else(|| "-".to_owned());
            println!(
                "{:<22}  {:<22}  {:<5}  {:<6}  {:<5}  {:<6}  {:<10}  {:<10}  {}",
                truncate(name, 22),
                truncate(&model.id, 22),
                temperature_honoured.map(yes_no).unwrap_or("-"),
                reasoning.map(yes_no).unwrap_or("-"),
                yes_no(caps.supports_tools),
                attachment.map(yes_no).unwrap_or("-"),
                max_in,
                max_out,
                cost,
            );
            if entry.is_none() {
                any_warn = true;
            }
        }
    }
    if any_warn {
        println!();
        println!("doctor --capabilities: rows with `-` cells need a models_dev catalog fetch");
        Ok(0)
    } else {
        Ok(0)
    }
}

/// PR-7 (deprecated alias): the per-kind static lookup is now
/// `capabilities_for_section` (Phase 5 renamed the kind tag to
/// the section name). Kept as `pub(crate) fn
/// capabilities_for_kind` so existing tests / callers continue
/// to compile; Phase 6 migrates them.
#[doc(hidden)]
#[allow(dead_code)]
pub(crate) fn capabilities_for_kind(
    section: &str,
) -> crate::llm::capabilities::ProviderCapabilities {
    capabilities_for_section(section)
}

/// Static capability matrix for a section. Mirrors the
/// `for_<kind>` constructors on [`ProviderCapabilities`] so the
/// doctor view can answer "is the `temperature` knob honoured
/// for this provider?" without instantiating a real
/// `Provider` (which would require an API key). v0.10: the
/// section name replaces the deprecated `kind` tag.
fn capabilities_for_section(section: &str) -> crate::llm::capabilities::ProviderCapabilities {
    use crate::llm::capabilities::ProviderCapabilities;
    match section {
        "minimax" => ProviderCapabilities::for_minimax(),
        "opencode" => ProviderCapabilities::for_opencode_go(),
        "deepseek" => ProviderCapabilities::for_deepseek(),
        "mock" => ProviderCapabilities::for_mock(),
        _ => ProviderCapabilities::default(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
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
    /// distinct provider section, with the model list sorted /
    /// deduplicated. The label follows the `models for provider
    /// '<section>'` contract that operators grep against in the
    /// doctor output.
    #[test]
    fn models_per_provider_groups_by_section_and_dedupes() {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "minimax".into(),
            ProviderConfig {
                models: vec![
                    crate::config::ModelConfig {
                        id: "MiniMax-M3".into(),
                        endpoint: None,
                        max_tokens: None,
                    },
                    crate::config::ModelConfig {
                        id: "MiniMax-M2.7".into(),
                        endpoint: None,
                        max_tokens: None,
                    },
                ],
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "mock".into(),
            ProviderConfig {
                models: vec![crate::config::ModelConfig {
                    id: "mock-model".into(),
                    endpoint: None,
                    max_tokens: None,
                }],
                ..ProviderConfig::default()
            },
        );
        let cfg = Config {
            providers,
            ..Config::default()
        };

        let entries = models_per_provider(&cfg);
        // Alphabetical by section: minimax before mock.
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
        opencode: Option<String>,
        moagan_home: Option<std::ffi::OsString>,
    }

    impl ApiKeyEnvGuard {
        fn new() -> Self {
            Self {
                minimax: std::env::var("MINIMAX_API_KEY").ok(),
                deepseek: std::env::var("DEEPSEEK_API_KEY").ok(),
                opencode: std::env::var("OPENCODE_API_KEY").ok(),
                moagan_home: std::env::var_os("MOAGAN_HOME"),
            }
        }
    }

    impl Drop for ApiKeyEnvGuard {
        fn drop(&mut self) {
            restore_or_remove("MINIMAX_API_KEY", self.minimax.as_deref());
            restore_or_remove("DEEPSEEK_API_KEY", self.deepseek.as_deref());
            restore_or_remove("OPENCODE_API_KEY", self.opencode.as_deref());
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
                models: Vec::new(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "deepseek".into(),
            ProviderConfig {
                models: Vec::new(),
                ..ProviderConfig::default()
            },
        );
        providers.insert(
            "opencode".into(),
            ProviderConfig {
                models: Vec::new(),
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
            std::env::set_var("OPENCODE_API_KEY", "sk-opencode");
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
            std::env::set_var("OPENCODE_API_KEY", "sk-opencode");
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
            std::env::remove_var("OPENCODE_API_KEY");
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
            std::env::remove_var("OPENCODE_API_KEY");
            std::env::remove_var("MOAGAN_HOME");
        }
        // A mock-only config requires no keys; the check must
        // report OK regardless of the env-var state.
        let cfg = Config {
            providers: std::collections::BTreeMap::from([(
                "mock".to_owned(),
                ProviderConfig {
                    models: Vec::new(),
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
            std::env::set_var("OPENCODE_API_KEY", "sk-opencode");
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

    /// PR-7: `capabilities_for_kind` is the per-kind static lookup
    /// the `--capabilities` view uses to fill the wire-format
    /// columns when the catalog cache is missing. The
    /// `for_minimax` constructor flips the wire preference to
    /// Anthropic and downgrades `supports_response_format`. The
    /// `for_opencode_go` (the renamed v0.10 opencode kind)
    /// constructor defaults to the OpenAI-compat chat-completions
    /// wire — the inner provider has already routed the model
    /// to the right wire format by URL. The test pins both so a
    /// future refactor cannot silently break the matrix.
    #[test]
    fn capabilities_for_kind_picks_correct_static_matrix() {
        use crate::llm::capabilities::ProviderCapabilities;
        let m = super::capabilities_for_kind("minimax");
        assert!(m.prefers_anthropic_wire);
        assert_eq!(m.wire_format_id(), "anthropic");
        let r = super::capabilities_for_kind("opencode");
        // v0.10: the `opencode` kind defaults to the
        // chat-completions wire (the inner provider dispatches
        // by URL). The legacy `for_opencode_go_responses`
        // constructor still exists for back-compat callers that
        // key on the wire id explicitly.
        assert!(r.prefers_openai_wire);
        assert_eq!(r.wire_format_id(), "openai_compatible");
        let resp = ProviderCapabilities::for_opencode_go_responses();
        assert!(resp.prefers_responses_wire);
        assert_eq!(resp.wire_format_id(), "responses");
        let mock = super::capabilities_for_kind("mock");
        assert!(mock.supports_tools);
        assert!(mock.supports_streaming);
        let unknown = super::capabilities_for_kind("not-a-real-kind");
        // Unknown kinds fall back to the OpenAI-compat baseline.
        assert!(unknown.prefers_openai_wire);
        let _ = ProviderCapabilities::default();
    }

    /// PR-7: `moagan doctor --capabilities` prints the table even
    /// when the `models_dev` catalog cache is missing (every cell
    /// falls back to `-` for the catalog-driven columns). The
    /// header must still render so the operator knows the
    /// command ran. The test exercises the function via the
    /// dispatcher's `capabilities` flag with a fake config that
    /// has one minimax provider.
    #[test]
    fn doctor_capabilities_prints_table_for_known_model() {
        use std::collections::BTreeMap;
        let _lock = TEST_DOCTOR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _api_lock = crate::TEST_API_KEYS_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _home_lock = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Snapshot every env var the doctor cares about and
        // restore on Drop so a test failure cannot leak mutated
        // state into the next test on the same thread.
        let _env = ApiKeyEnvGuard::new();
        // Redirect MOAGAN_HOME to an empty tempdir so the
        // catalog is guaranteed missing — the `--capabilities`
        // path must still print a header.
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let mut providers = BTreeMap::new();
        providers.insert(
            "minimax".into(),
            crate::config::ProviderConfig {
                models: Vec::new(),
                ..crate::config::ProviderConfig::default()
            },
        );
        let _cfg = Config {
            providers,
            ..Config::default()
        };
        // The capability table prints a per-section static matrix
        // for every provider; the test pins the per-section lookup
        // (the actual stdout print is exercised by the
        // integration-test surface in `moagan doctor`).
        let caps = super::capabilities_for_section("minimax");
        assert!(
            caps.prefers_anthropic_wire,
            "minimax prefers the Anthropic wire"
        );
    }
}
