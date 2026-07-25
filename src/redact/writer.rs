//! `RedactWriter` — any `io::Write` that redacts before persisting.
//!
//! Used by the telemetry layer and the manifest writer so all on-disk
//! artefacts pass through the same policy.

use std::io::{self, Write};

use crate::error::Result;
use crate::redact::apply::{RedactPolicy, Surface, apply};

/// Wraps an inner writer and applies the redaction policy to every
/// chunk before it reaches the inner writer.
pub struct RedactWriter<W: Write> {
    inner: Option<W>,
    policy: RedactPolicy,
    surface: Surface,
    buffer: Vec<u8>,
}

impl<W: Write> RedactWriter<W> {
    /// Build a new `RedactWriter` over `inner`, applying `policy` to
    /// every flush.
    pub fn new(inner: W, policy: RedactPolicy, surface: Surface) -> Self {
        Self {
            inner: Some(inner),
            policy,
            surface,
            buffer: Vec::new(),
        }
    }

    /// Consume the wrapper and return the inner writer. Any buffered
    /// bytes are flushed first.
    pub fn into_inner(mut self) -> io::Result<W> {
        self.flush()?;
        let inner = self
            .inner
            .take()
            .ok_or_else(|| io::Error::other("RedactWriter already consumed"))?;
        std::mem::forget(self);
        Ok(inner)
    }

    fn flush_buffer(&mut self) -> io::Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        if self.buffer.is_empty() {
            return Ok(());
        }
        let text = match std::str::from_utf8(&self.buffer) {
            Ok(s) => s,
            Err(_) => {
                // Non-UTF8 bytes pass through verbatim (binary data).
                inner.write_all(&self.buffer)?;
                self.buffer.clear();
                return Ok(());
            }
        };
        let redacted = apply(&self.policy, self.surface, text)
            .unwrap_or(std::borrow::Cow::Borrowed(text));
        inner.write_all(redacted.as_bytes())?;
        self.buffer.clear();
        Ok(())
    }
}

impl<W: Write> Write for RedactWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = buf.len();
        self.buffer.extend_from_slice(buf);
        // Flush on newlines so line-oriented sinks (jsonl) see one
        // record at a time and redaction is applied per line.
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            let text = std::str::from_utf8(&line).unwrap_or("");
            let redacted = apply(&self.policy, self.surface, text)
                .unwrap_or(std::borrow::Cow::Borrowed(text));
            if let Some(inner) = self.inner.as_mut() {
                inner.write_all(redacted.as_bytes())?;
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()?;
        if let Some(inner) = self.inner.as_mut() {
            inner.flush()?;
        }
        Ok(())
    }
}

impl<W: Write> Drop for RedactWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Convenience: redact a single chunk of text and return the result.
pub fn redact_text(policy: &RedactPolicy, surface: Surface, text: &str) -> Result<String> {
    Ok(apply(policy, surface, text)?.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_redacts_on_write() {
        let mut buf = Vec::new();
        {
            let mut w = RedactWriter::new(&mut buf, RedactPolicy::default(), Surface::Telemetry);
            writeln!(w, "key=sk-cp-aaaaaaaaaaaaaaaaaaaa").unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[REDACTED:minimax_sk_cp]"));
    }

    #[test]
    fn writer_passes_through_when_policy_off() {
        let mut buf = Vec::new();
        {
            let mut w = RedactWriter::new(&mut buf, RedactPolicy::allow_all(), Surface::Telemetry);
            writeln!(w, "key=sk-cp-aaaaaaaaaaaaaaaaaaaa").unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("sk-cp-aaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn writer_buffered_until_newline() {
        let mut buf = Vec::new();
        {
            let mut w = RedactWriter::new(&mut buf, RedactPolicy::default(), Surface::Telemetry);
            // Split the secret across two writes without a newline.
            write!(w, "key=sk-cp-").unwrap();
            write!(w, "aaaaaaaaaaaaaaaaaaaa tail").unwrap();
            // Flush to force the buffer out.
            w.flush().unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("[REDACTED:minimax_sk_cp]"));
    }

    #[test]
    fn into_inner_reclaims_writer() {
        let buf: Vec<u8> = Vec::new();
        let w = RedactWriter::new(buf, RedactPolicy::default(), Surface::Telemetry);
        let _inner: Vec<u8> = w.into_inner().unwrap();
    }
}
