use anyhow::Result;

fn main() -> Result<()> {
    init_tracing();
    install_panic_hook();
    #[cfg(debug_assertions)]
    trigger_phase_l_test_panic();
    moagan::run_blocking()
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,moagan=debug"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_writer(
            moagan::telemetry::redact::ReportingLayer::new(std::io::stderr),
        ))
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
