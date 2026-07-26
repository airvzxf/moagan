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
    name: &'static str,
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
            name: "api_key",
            status: Status::Ok,
            detail: "MINIMAX_API_KEY set (or no provider needs it)".into(),
        }
    } else {
        Check {
            name: "api_key",
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
                            name: "home",
                            status: Status::Ok,
                            detail: format!("writable at {}", path.display()),
                        }
                    }
                    Err(e) => Check {
                        name: "home",
                        status: Status::Fail,
                        detail: format!("MOAGAN_HOME={} is not writable: {e}", path.display()),
                    },
                }
            }
            Err(e) => Check {
                name: "home",
                status: Status::Fail,
                detail: format!("MOAGAN_HOME ensure failed: {e}"),
            },
        },
        Err(e) => Check {
            name: "home",
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
                name: "sqlite",
                status: Status::Warn,
                detail: "skipped: MOAGAN_HOME not resolvable".into(),
            };
        }
    };
    match Db::open(&home.meta_db_path()) {
        Ok(_db) => Check {
            name: "sqlite",
            status: Status::Ok,
            detail: format!("opened at {}", home.meta_db_path().display()),
        },
        Err(e) => Check {
            name: "sqlite",
            status: Status::Fail,
            detail: format!("open failed: {e}"),
        },
    }
}

fn check_provider_config(cfg: &Config) -> Check {
    if cfg.providers.is_empty() {
        return Check {
            name: "providers",
            status: Status::Warn,
            detail: "no providers registered".into(),
        };
    }
    Check {
        name: "providers",
        status: Status::Ok,
        detail: format!("{} provider(s) configured", cfg.providers.len()),
    }
}

/// Run every check and return 0 if everything is OK, 1 otherwise.
pub fn run() -> Result<i32> {
    let cfg = Config::load()?;
    let mut any_fail = false;
    let mut any_warn = false;
    emit(check_provider_config(&cfg), &mut any_fail, &mut any_warn);
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

    #[test]
    fn status_tags_are_stable() {
        assert_eq!(Status::Ok.tag(), "[OK]");
        assert_eq!(Status::Warn.tag(), "[WARN]");
        assert_eq!(Status::Fail.tag(), "[FAIL]");
    }
}
