//! Integration test for `phases::util::parse_json_with_recovery` tracing
//! instrumentation. Lives in its own binary (rather than in
//! `src/phases/util::tests`) so it gets a fresh `tracing-core` runtime —
//! its own `LevelFilter::current()` atomic and its own callsite
//! interest registry — and avoids the race with
//! `src/sandbox/process.rs:2525`'s `tracing_subscriber::fmt::try_init()`
//! clobbering `LevelFilter::current()` to `LevelFilter::ERROR` for the
//! whole test binary.
//!
//! Pins the `tracing::debug!` event that documents the tolerant
//! extraction byte range: if a future refactor drops the
//! `tracing::debug!` call, this test fails.

use std::io;
use std::sync::{Arc, Mutex};

use moagan::phases::util::parse_json_with_recovery;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::prelude::*;

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedWriter {
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

impl<'a> MakeWriter<'a> for SharedBuf {
    type Writer = SharedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriter(self.0.clone())
    }
}

#[test]
fn parse_json_with_recovery_preserves_extraction_metadata_via_tracing() {
    // Set up a tracing subscriber that captures every event into
    // an in-memory buffer. Run the wrapper on a payload that
    // requires the tolerant extraction step, then assert that
    // the recovery succeeded AND that the wrapper emitted the
    // `tracing::debug!` event that documents the extraction
    // byte range.
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_writer(buf.clone().with_max_level(tracing::Level::TRACE)),
    );

    tracing::subscriber::with_default(subscriber, || {
        let input = "noise prefix {\"answer\": 42} noise suffix";
        let v = parse_json_with_recovery(input).unwrap();
        assert_eq!(v["answer"], serde_json::json!(42));
    });

    // Recover from a poisoned Mutex instead of silently dropping the
    // captured bytes via `unwrap_or_default()`. A poisoned Mutex still
    // holds the bytes that were already written; only the panic flag
    // is set.
    let captured = buf
        .0
        .lock()
        .map(|b| b.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
    let captured = String::from_utf8(captured).unwrap();
    assert!(
        captured.contains("parse_json_with_recovery"),
        "tracing log not captured: {captured}"
    );
    assert!(
        captured.contains("tolerant extraction"),
        "tolerant extraction event missing: {captured}"
    );
}
