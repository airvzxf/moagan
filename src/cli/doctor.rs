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
    let needs_key = |kind: &str| matches!(kind, "minimax");
    let mut missing: Vec<&str> = Vec::new();
    for (name, spec) in &cfg.providers {
        if needs_key(&spec.kind) && std::env::var("MINIMAX_API_KEY").is_err() {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        Check {
            name: "api_key".to_string(),
            status: Status::Ok,
            detail: "MINIMAX_API_KEY set (or no provider needs it)".into(),
        }
    } else {
        Check {
            name: "api_key".to_string(),
            status: Status::Fail,
            detail: format!(
                "MINIMAX_API_KEY not set; needed for providers: {}",
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
                // Duplicate model; must be deduped.
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
}
