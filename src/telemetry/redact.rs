//! Tracing output boundary that redacts complete formatted events before forwarding them.
//! `D.8.3` keeps secrets out of `tracing` output even when middleware logs raw fields.

use tracing::Metadata;
use tracing_subscriber::fmt::MakeWriter;

use crate::redact::{RedactPolicy, RedactWriter, Surface};

/// Writer factory that applies telemetry redaction to every formatted tracing event.
pub struct ReportingLayer<Inner> {
    inner: Inner,
    policy: RedactPolicy,
}

impl<Inner> ReportingLayer<Inner> {
    /// Wrap an existing tracing writer factory with the default redaction policy.
    pub fn new(inner: Inner) -> Self {
        Self {
            inner,
            policy: RedactPolicy::default(),
        }
    }
}

impl<'a, Inner> MakeWriter<'a> for ReportingLayer<Inner>
where
    Inner: MakeWriter<'a>,
{
    type Writer = RedactWriter<Inner::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactWriter::new(
            self.inner.make_writer(),
            self.policy.clone(),
            Surface::Telemetry,
        )
    }

    fn make_writer_for(&'a self, metadata: &Metadata<'_>) -> Self::Writer {
        RedactWriter::new(
            self.inner.make_writer_for(metadata),
            self.policy.clone(),
            Surface::Telemetry,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::prelude::*;

    use super::*;

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

    fn capture(event: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(ReportingLayer::new(buffer.clone())),
        );
        tracing::subscriber::with_default(subscriber, event);
        let bytes = buffer
            .0
            .lock()
            .map(|bytes| bytes.clone())
            .unwrap_or_default();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn redacts_anthropic_key_in_event_message() {
        let output = capture(|| {
            tracing::info!("provider key sk-ant-abcdefghijklmnopqrst failed");
        });
        assert!(output.contains("[REDACTED:anthropic_key]"));
        assert!(!output.contains("sk-ant-abcdefghijklmnopqrst"));
    }

    #[test]
    fn redacts_email_in_event_field() {
        let output = capture(|| {
            tracing::info!(user = "alice@example.com", "request failed");
        });
        assert!(output.contains("[REDACTED:email]"));
        assert!(!output.contains("alice@example.com"));
    }
}
