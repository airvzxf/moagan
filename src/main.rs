use anyhow::Result;
use clap::Parser;
use std::io::IsTerminal;

fn main() -> Result<()> {
    // Best-effort .env autoload. dotenvy silently does nothing if no
    // .env is found, and never overrides env vars that are already set
    // (12-factor compatible: explicit env wins over .env). This makes
    // `moagan doctor` and friends work out-of-the-box when the operator
    // keeps their secrets in `.env` in the current directory.
    if let Ok(path) = dotenvy::dotenv()
        && std::env::var_os("MOAGAN_QUIET").is_none()
    {
        // No subscriber yet; print to stderr directly so the
        // operator sees the .env load even when init_tracing
        // decides to emit JSON instead of text.
        eprintln!("[moagan] loaded .env from {}", path.display());
    }

    // The phase-L panic test must run BEFORE clap parsing so
    // `moagan --help` still panics when `MOAGAN_PHASE_L_TEST_PANIC`
    // is set; otherwise clap exits early and the integration test
    // `panic_message_through_main_binary_is_redacted` sees only the
    // help text. `cfg(debug_assertions)` so release builds skip it.
    // The panic hook is installed FIRST so the panic message goes
    // through redact() (per the test contract).
    install_panic_hook();
    #[cfg(debug_assertions)]
    trigger_phase_l_test_panic();

    // Parse CLI flags before `init_tracing` so that
    // `--log-format <text|json>` and `--event-format <jsonl|off>`
    // take effect on the very first emitted event. Clap does not
    // auto-write to the env var named by `env =`, so we propagate
    // the explicit flag value into `MOAGAN_LOG_FORMAT` /
    // `MOAGAN_EVENT_FORMAT` if the operator passed a non-`Auto`
    // value; `Auto` (default) leaves the env var unset and the
    // init_tracing auto-detect decides.
    let cli = moagan::cli::Cli::parse();
    let format_override: Option<&str> = match cli.log_format {
        moagan::cli::LogFormatArg::Text => Some("text"),
        moagan::cli::LogFormatArg::Json => Some("json"),
        moagan::cli::LogFormatArg::Auto => None,
    };
    if let Some(v) = format_override {
        // SAFETY: `set_var` is `unsafe` because it can race with
        // concurrent readers in other threads. The tracing
        // subscriber is not initialised yet and `run_blocking_with_cli`
        // starts the multi-threaded runtime only AFTER we return, so
        // no other thread can observe the env var between this write
        // and the subscriber's read in `resolve_log_format`.
        unsafe { std::env::set_var("MOAGAN_LOG_FORMAT", v) };
    }
    let event_override: Option<&str> = match cli.event_format {
        moagan::cli::EventFormatArg::Jsonl => Some("jsonl"),
        moagan::cli::EventFormatArg::Off => Some("off"),
    };
    if let Some(v) = event_override {
        // SAFETY: same rationale as the MOAGAN_LOG_FORMAT set above.
        // No concurrent reader exists for stdout events yet — the
        // multi-threaded runtime starts after we return.
        unsafe { std::env::set_var("MOAGAN_EVENT_FORMAT", v) };
    }

    init_tracing();
    tracing::info!("moagan: starting");
    warn_runtime_coverage_unbounded_growth();
    let res = moagan::run_blocking_with_cli(cli);
    if let Err(ref e) = res {
        tracing::error!(error = %e, "moagan: dispatcher failed");
    }
    tracing::info!("moagan: exit");
    res
}

/// Emit a one-shot `tracing::warn!` at startup when the binary was
/// built with SanCov runtime coverage. The SanCov runtime writes a
/// `*.profraw` file at the path pointed at by `LLVM_PROFILE_FILE`,
/// and the file grows unbounded with no internal cap — verified
/// empirically on 2026-08-19 when a 5h 40m run produced a 66 GB
/// `active.profraw` that filled `/home` to 96 %. The fix is a
/// runtime rotation mechanism (see `src/coverage/`), but until that
/// ships the operator needs to know to either rotate the file
/// manually or use `target/debug/moagan` (no SanCov) for long
/// runs.
///
/// The detection is conservative: warn whenever the `LLVM_PROFILE_FILE`
/// env var is set AND the `coverage` Cargo feature is on. The
/// feature flag governs whether the SanCov runtime symbols are
/// linked; the env var governs whether the runtime writes
/// anywhere. The combination is necessary for an actual `profraw`
/// to appear.
fn warn_runtime_coverage_unbounded_growth() {
    #[cfg(feature = "coverage")]
    {
        if let Ok(path) = std::env::var("LLVM_PROFILE_FILE") {
            tracing::warn!(
                target: "moagan::coverage",
                profraw_path = %path,
                "runtime coverage: the SanCov `*.profraw` file grows unbounded; \
                 long-running tests (>= 1 h) may exhaust disk. \
                 Use `target/debug/moagan` (no SanCov) or rotate the file manually. \
                 See ADR-0002 and scripts/coverage-wrap.sh for the rotation workflow."
            );
        }
    }
}

/// stderr format selected by `init_tracing`.
///
/// Per **ADR-0002 §B** the project committed to JSON-formatted
/// tracing events; the implementation lived in `src/main.rs` as text
/// until v0.11 switched it to honour this enum. Text remains the
/// default for interactive terminals (TTY); JSON is the default for
/// redirected / piped stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogFormat {
    Text,
    Json,
}

/// Resolve the stderr format. Honours `MOAGAN_LOG_FORMAT` if the
/// operator sets it explicitly; otherwise picks text when stderr is a
/// TTY and JSON otherwise. Called *before* the tracing subscriber is
/// initialised so the format decision shows up in the very first
/// emitted event (the `format decision` debug event below).
fn resolve_log_format() -> LogFormat {
    // 1. Explicit override wins.
    if let Ok(s) = std::env::var("MOAGAN_LOG_FORMAT") {
        return match s.to_ascii_lowercase().as_str() {
            "text" | "pretty" => LogFormat::Text,
            // Default to JSON for anything unrecognised: JSON is the
            // safe format for downstream tooling, and a typo should
            // not silently degrade to coloured text.
            _ => LogFormat::Json,
        };
    }
    // 2. Auto-detect based on stderr TTY.
    if std::io::stderr().is_terminal() {
        LogFormat::Text
    } else {
        LogFormat::Json
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let format = resolve_log_format();
    // The format decision is reported via the very first subscriber
    // event (`init_tracing: subscriber initialised format=...`).
    // An `eprintln!` BEFORE the subscriber would corrupt NDJSON
    // consumers that pipe stderr into `jq`, so we do NOT print
    // anything here.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,moagan=debug"));
    let stderr_layer = match format {
        LogFormat::Text => fmt::layer()
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(moagan::telemetry::redact::ReportingLayer::new(
                std::io::stderr,
            ))
            .boxed(),
        LogFormat::Json => fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(moagan::telemetry::redact::ReportingLayer::new(
                std::io::stderr,
            ))
            .boxed(),
    };
    let res = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .try_init();
    match res {
        Ok(()) => tracing::debug!(?format, "init_tracing: subscriber initialised"),
        Err(e) => eprintln!("init_tracing: try_init failed: {e}"),
    }
}

fn install_panic_hook() {
    tracing::debug!("install_panic_hook: installing custom panic hook");
    std::panic::set_hook(Box::new(|info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "<non-string panic>".to_string(),
            },
        };
        let redacted = redact_panic_message(&msg);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        eprintln!("panicked at {location}: {redacted}");
    }));
}

#[cfg(debug_assertions)]
fn trigger_phase_l_test_panic() {
    if let Ok(message) = std::env::var("MOAGAN_PHASE_L_TEST_PANIC") {
        tracing::debug!(
            message_len = message.len(),
            "trigger_phase_l_test_panic: panicking"
        );
        panic!("{message}");
    }
}

fn redact_panic_message(message: &str) -> String {
    moagan::redact::apply(
        &moagan::redact::RedactPolicy::default(),
        moagan::redact::Surface::Telemetry,
        message,
    )
    .map(std::borrow::Cow::into_owned)
    .unwrap_or_else(|_| message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::redact_panic_message;

    #[test]
    fn panic_message_redacts_anthropic_key() {
        let message = "panic payload sk-ant-abcdefghijklmnopqrst";
        let redacted = redact_panic_message(message);
        assert!(redacted.contains("[REDACTED:anthropic_key]"));
        assert!(!redacted.contains("sk-ant-abcdefghijklmnopqrst"));
    }
}
