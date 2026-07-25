//! Mock provider. Returns canned responses from an in-memory list, or
//! from a JSON file on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::error::{Error, Result};

use super::provider::Provider;
use super::wire::{CallRecord, Request, Response, Usage};

/// A single canned response.
#[derive(Debug, Clone)]
pub struct MockResponse {
    /// Text to return as the LLM output.
    pub text: String,
    /// Optional pre-baked usage; defaults to 0 tokens.
    pub usage: Usage,
    /// Optional finish reason.
    pub finish_reason: Option<String>,
}

impl MockResponse {
    /// Build a response from raw text with zero usage.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            usage: Usage::default(),
            finish_reason: Some("end_turn".into()),
        }
    }

    /// Build a response with explicit usage.
    pub fn with_usage(text: impl Into<String>, input: u64, output: u64) -> Self {
        Self {
            text: text.into(),
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read: 0,
                cache_creation: 0,
            },
            finish_reason: Some("end_turn".into()),
        }
    }

    /// Convert to a `Response` for the [`Provider`] trait.
    pub fn into_response(self) -> Response {
        Response {
            text: self.text,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

/// Provider that hands out `MockResponse` values in order.
#[derive(Debug, Default)]
pub struct MockProvider {
    responses: Vec<MockResponse>,
    index: AtomicUsize,
    name: String,
    model: String,
    endpoint: String,
    calls: parking_lot::Mutex<Vec<CallRecord>>,
}

impl MockProvider {
    /// Build a mock with explicit canned responses.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses,
            index: AtomicUsize::new(0),
            name: "mock".to_owned(),
            model: "mock-model".to_owned(),
            endpoint: "mock://local".to_owned(),
            calls: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Build an empty mock — useful as a placeholder for tests that
    /// will inject responses via [`Self::push`].
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Push a response onto the queue.
    pub fn push(&mut self, response: MockResponse) {
        self.responses.push(response);
    }

    /// Number of remaining (unconsumed) responses.
    pub fn remaining(&self) -> usize {
        self.responses
            .len()
            .saturating_sub(self.index.load(Ordering::SeqCst))
    }

    /// Read all calls recorded so far.
    pub fn calls(&self) -> Vec<CallRecord> {
        self.calls.lock().clone()
    }

    /// Load canned responses from a directory. Each file is a JSON
    /// object with `text` (required), `usage` (optional), `finish_reason`
    /// (optional). Files are read in alphabetical order.
    pub fn from_dir(path: &Path) -> Result<Self> {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .map_err(|e| Error::Provider(format!("mock dir {path:?}: {e}")))?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();
        let mut responses = Vec::new();
        for entry in entries {
            let raw = fs::read_to_string(&entry)
                .map_err(|e| Error::Provider(format!("mock read {entry:?}: {e}")))?;
            let resp: MockResponseJson = serde_json::from_str(&raw)
                .map_err(|e| Error::Provider(format!("mock parse {entry:?}: {e}")))?;
            responses.push(resp.into());
        }
        Ok(Self::new(responses))
    }
}

#[derive(Debug, serde::Deserialize)]
struct MockResponseJson {
    text: String,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    finish_reason: Option<String>,
}

impl From<MockResponseJson> for MockResponse {
    fn from(j: MockResponseJson) -> Self {
        Self {
            text: j.text,
            usage: Usage {
                input_tokens: j.input_tokens.unwrap_or(0),
                output_tokens: j.output_tokens.unwrap_or(0),
                cache_read: 0,
                cache_creation: 0,
            },
            finish_reason: j.finish_reason,
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn send(&self, _req: &Request) -> Result<Response> {
        let i = self.index.fetch_add(1, Ordering::SeqCst);
        let record = CallRecord {
            cache_key: String::new(),
            provider: self.name().to_owned(),
            model: self.model().to_owned(),
            started_unix: crate::time::now_unix_secs(),
            ended_unix: crate::time::now_unix_secs(),
            http_status: Some(200),
            cache_hit: false,
            usage: Usage::default(),
            error: None,
        };
        self.calls.lock().push(record);
        let r = self.responses.get(i).ok_or(Error::MockExhausted)?;
        Ok(r.clone().into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::role::Role;

    #[tokio::test]
    async fn serves_responses_in_order() {
        let p = MockProvider::new(vec![
            MockResponse::plain("first"),
            MockResponse::plain("second"),
        ]);
        let req = || Request {
            role: Role::Intake,
            model: "m".into(),
            system: "s".into(),
            user: "u".into(),
            max_tokens: 16,
            temperature: None,
            top_p: None,
            response_schema: None,
        };
        let r1 = p.send(&req()).await.unwrap();
        let r2 = p.send(&req()).await.unwrap();
        assert_eq!(r1.text, "first");
        assert_eq!(r2.text, "second");
        assert!(p.send(&req()).await.is_err());
    }

    #[test]
    fn from_dir_loads_responses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("01_intake.json"), r#"{"text": "intake-ok"}"#).unwrap();
        fs::write(
            dir.join("02_propose.json"),
            r#"{"text": "propose-ok", "input_tokens": 10, "output_tokens": 5}"#,
        )
        .unwrap();
        let p = MockProvider::from_dir(dir).unwrap();
        assert_eq!(p.remaining(), 2);
    }
}
