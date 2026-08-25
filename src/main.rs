use anyhow::Result;

fn main() -> Result<()> {
    // Best-effort .env autoload. dotenvy silently does nothing if no
    // .env is found, and never overrides env vars that are already set
    // (12-factor compatible: explicit env wins over .env). This makes
    // `moagan doctor` and friends work out-of-the-box when the operator
    // keeps their secrets in `.env` in the current directory.
    if let Ok(path) = dotenvy::dotenv()
        && std::env::var_os("MOAGAN_QUIET").is_none()
    {
        eprintln!("[moagan] loaded .env from {}", path.display());
    }
    init_tracing();
    warn_runtime_coverage_unbounded_growth();
    install_panic_hook();
    #[cfg(debug_assertions)]
    trigger_phase_l_test_panic();
    moagan::run_blocking()
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

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,moagan=debug"));
    // Layer B of ADR-0002 (runtime coverage): every event carries the
    // source call site (`file`, `line`, `column`) and the active
    // span. The cost is essentially zero — the metadata is filled in
    // by the `tracing` macros themselves, we just ask the JSON
    // formatter to surface it. The JSON formatter emits the current
    // span under the `span` key by default (and skips the full span
    // list to keep the JSONL compact on deeply nested pipeline
    // runs), so we do not need `with_current_span` / `with_span_list`
    // — those flags only exist for the text formats.
    //
    // Two writers: stderr (colored for interactive terminals) and
    // a lazy file writer (`FileLogWriter`). The file writer is
    // controlled by the top-level `--logs` flag and the
    // `MOAGAN_RUN_LOGS` env var; the path is plumbed in by
    // `dispatch_inner` after clap parses (the subscriber cannot be
    // re-initialised, so the path goes through a process-global
    // `OnceLock`). The file layer always disables ANSI so the
    // output is grep-friendly. Both writers are wrapped in the
    // redaction `ReportingLayer` so secrets never leak to either
    // destination.
    let stderr_layer = fmt::layer()
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(moagan::telemetry::redact::ReportingLayer::new(
            std::io::stderr,
        ));
    let file_layer = fmt::layer()
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .with_writer(moagan::telemetry::redact::ReportingLayer::new(
            moagan::telemetry::file_log::FileLogWriter,
        ));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();
}

fn install_panic_hook() {
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
