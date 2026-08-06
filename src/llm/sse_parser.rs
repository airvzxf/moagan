//! SSE (Server-Sent Events) streaming parser for LLM responses.
//!
//! Parses `data: <json>\n\n` events from an OpenAI-format SSE stream
//! and yields each complete JSON payload. Handles the
//! `[DONE]` sentinel that OpenAI uses to terminate the stream.

use serde::de::DeserializeOwned;
use std::io::BufRead;

/// Streaming parser for OpenAI-format Server-Sent Events.
///
/// Reads `data: <payload>` lines from a `BufRead` source and yields
/// each JSON payload as it arrives. The `[DONE]` sentinel is
/// signalled by returning `Ok(None)`.
pub struct SseParser<R: BufRead> {
    reader: R,
    buf: String,
}

impl<R: BufRead> SseParser<R> {
    /// Build a new parser over the given buffered reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: String::new(),
        }
    }

    /// Read the next `data: <payload>` line. Returns `Ok(None)` on
    /// EOF or `[DONE]` sentinel. Returns `Err` on protocol violation.
    pub fn next_data<T: DeserializeOwned>(&mut self) -> Result<Option<T>, SseError> {
        loop {
            self.buf.clear();
            let bytes = self.reader.read_line(&mut self.buf).map_err(SseError::Io)?;
            if bytes == 0 {
                return Ok(None);
            }
            let line = self.buf.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                let payload = rest.trim();
                if payload == "[DONE]" {
                    return Ok(None);
                }
                let parsed: T = serde_json::from_str(payload).map_err(SseError::Parse)?;
                return Ok(Some(parsed));
            }
        }
    }
}

#[derive(Debug)]
/// Errors returned by [`SseParser::next_data`].
pub enum SseError {
    /// Underlying I/O failure from the reader.
    Io(std::io::Error),
    /// JSON parse failure on a `data:` payload.
    Parse(serde_json::Error),
}

impl std::fmt::Display for SseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Parse(e) => write!(f, "parse: {e}"),
        }
    }
}

impl std::error::Error for SseError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::Cursor;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Payload {
        text: String,
    }

    #[test]
    fn sse_parser_reads_single_data_line() {
        let input = "data: {\"text\":\"hello\"}\n\n";
        let mut p = SseParser::new(Cursor::new(input.as_bytes()));
        let out: Payload = p.next_data().unwrap().unwrap();
        assert_eq!(
            out,
            Payload {
                text: "hello".to_string()
            }
        );
        assert!(p.next_data::<Payload>().unwrap().is_none());
    }

    #[test]
    fn sse_parser_handles_done_sentinel() {
        let input = "data: {\"text\":\"a\"}\n\ndata: [DONE]\n\n";
        let mut p = SseParser::new(Cursor::new(input.as_bytes()));
        let first: Payload = p.next_data().unwrap().unwrap();
        assert_eq!(
            first,
            Payload {
                text: "a".to_string()
            }
        );
        let second: Option<Payload> = p.next_data().unwrap();
        assert!(second.is_none());
    }

    #[test]
    fn sse_parser_skips_empty_lines() {
        let input = "\n\ndata: {\"text\":\"x\"}\n\n\n\n";
        let mut p = SseParser::new(Cursor::new(input.as_bytes()));
        let out: Payload = p.next_data().unwrap().unwrap();
        assert_eq!(
            out,
            Payload {
                text: "x".to_string()
            }
        );
    }

    #[test]
    fn sse_parser_returns_error_on_invalid_json() {
        let input = "data: {not json}\n\n";
        let mut p = SseParser::new(Cursor::new(input.as_bytes()));
        let err = p.next_data::<Payload>().unwrap_err();
        assert!(matches!(err, SseError::Parse(_)));
    }
}
