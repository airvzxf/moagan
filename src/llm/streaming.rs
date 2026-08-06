//! D.19.7: streaming + TTFT skeleton.
//!
//! The actual wire formats already emit `stream: true`; this
//! module provides the time-to-first-token recorder and a
//! placeholder for streaming response metrics.

use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct TtftMeasurement {
    pub first_token_at_ms: u128,
    pub total_tokens: u32,
}

pub struct StreamRecorder {
    started_at: Option<Instant>,
    first_token_at: Option<Instant>,
    tokens: u32,
}

impl StreamRecorder {
    pub fn new() -> Self {
        Self { started_at: None, first_token_at: None, tokens: 0 }
    }
    pub fn start(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }
    pub fn record_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
        self.tokens = self.tokens.saturating_add(1);
    }
    pub fn finish(&self) -> Option<TtftMeasurement> {
        let start = self.started_at?;
        let first = self.first_token_at?;
        Some(TtftMeasurement {
            first_token_at_ms: first.duration_since(start).as_millis(),
            total_tokens: self.tokens,
        })
    }
}

impl Default for StreamRecorder {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn stream_recorder_returns_none_before_start() {
        let r = StreamRecorder::new();
        assert!(r.finish().is_none());
    }

    #[test]
    fn stream_recorder_measures_ttft() {
        let mut r = StreamRecorder::new();
        r.start();
        sleep(Duration::from_millis(5));
        r.record_token();
        let m = r.finish().unwrap();
        assert!(m.first_token_at_ms >= 5);
        assert_eq!(m.total_tokens, 1);
    }

    #[test]
    fn stream_recorder_returns_none_when_no_tokens() {
        let mut r = StreamRecorder::new();
        r.start();
        assert!(r.finish().is_none());
    }

    #[test]
    fn stream_recorder_start_is_idempotent() {
        let mut r = StreamRecorder::new();
        r.start();
        let first = r.started_at.unwrap();
        sleep(Duration::from_millis(2));
        r.start();
        assert_eq!(r.started_at.unwrap(), first);
    }

    #[test]
    fn stream_recorder_tokens_saturate_not_overflow() {
        let mut r = StreamRecorder::new();
        r.start();
        r.tokens = u32::MAX;
        r.record_token();
        assert_eq!(r.tokens, u32::MAX);
    }
}
