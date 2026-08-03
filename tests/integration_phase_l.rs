use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use moagan::domain::PauseReason;
use moagan::error::{Error, ExitCode};
use moagan::redact::patterns::PATTERNS;
use moagan::telemetry::redact::ReportingLayer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("shared tracing buffer poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedBuffer {
    type Writer = SharedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriter(self.0.clone())
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

#[test]
fn panic_message_through_main_binary_is_redacted() {
    let secret = "sk-ant-abcdefghijklmnopqrst";
    let output = Command::new(binary())
        .env(
            "MOAGAN_PHASE_L_TEST_PANIC",
            format!("provider key {secret}"),
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("[REDACTED:anthropic_key]"), "{stderr}");
    assert!(!stderr.contains(secret), "{stderr}");
}

#[test]
fn tracing_event_with_secret_message_is_redacted() {
    let secret = "sk-ant-abcdefghijklmnopqrst";
    let buffer = SharedBuffer::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(ReportingLayer::new(buffer.clone())),
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("provider key {secret} failed");
    });
    let bytes = buffer.0.lock().map(|bytes| bytes.clone()).unwrap();
    let output = String::from_utf8(bytes).unwrap();
    assert!(output.contains("[REDACTED:anthropic_key]"));
    assert!(!output.contains(secret));
}

#[test]
fn anthropic_api_key_pattern_matches() {
    let pattern = PATTERNS
        .iter()
        .find(|pattern| pattern.id == "anthropic_key")
        .unwrap();
    assert!(pattern.re.is_match("sk-ant-abcdefghijklmnopqrst"));
    assert!(!pattern.re.is_match("anthropic-key"));
}

#[test]
fn pause_reason_serializes_to_snake_case() {
    let value = serde_json::to_string(&PauseReason::TimeoutPhase).unwrap();
    assert_eq!(value, "\"timeout_phase\"");
}

#[test]
fn error_io_returns_exit_code_8() {
    let error = Error::from(io::Error::other("disk failure"));
    assert_eq!(error.exit_code(), ExitCode::IoError);
    assert_eq!(error.exit_code() as i32, 8);
}

#[test]
fn error_invalid_args_returns_exit_code_2() {
    let error = Error::InvalidArgs("bad flag".into());
    assert_eq!(error.exit_code(), ExitCode::InvalidArgs);
    assert_eq!(error.exit_code() as i32, 2);
}
