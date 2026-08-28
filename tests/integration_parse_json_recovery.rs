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
//!
//! **Invariant**: do not add additional `#[test]` functions to this
//! binary. A second test in this file would share the `tracing-core`
//! runtime and re-introduce the §2.2 flake that this binary was created
//! to isolate. New tracing-dependent tests belong in their own
//! integration binary (e.g. `integration_*_tracing.rs`).

use std::io;
use std::sync::{Arc, Mutex};

use moagan::phases::util::parse_json_with_recovery;
use tracing_subscriber::fmt::MakeWriter;
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
            .with_writer(buf.clone()),
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
    // The tolerant-extraction `tracing::debug!` carries structured
    // `start` / `end` byte-range fields. The test name promises this
    // metadata is preserved; assert it explicitly so a future refactor
    // that drops the structured fields (and keeps only the message)
    // cannot pass silently.
    assert!(
        captured.contains("start="),
        "extraction start field missing: {captured}"
    );
    assert!(
        captured.contains("end="),
        "extraction end field missing: {captured}"
    );
}
