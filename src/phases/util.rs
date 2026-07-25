//! Shared helpers for phases that need to read JSON from disk, write
//! JSON to disk atomically, and parse LLM responses.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::atomic::writer::AtomicWriter;
use crate::error::Result;

/// Read a JSON file and deserialize it.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(crate::Error::from)
}

/// Write `value` as JSON to `path` atomically.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(crate::Error::from)?;
    AtomicWriter::new().write(path, &bytes)?;
    Ok(())
}

/// Strip a leading/trailing markdown fence from the model output, then
/// parse the inner JSON.
pub fn parse_model_json<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let trimmed = strip_code_fence(raw);
    serde_json::from_str::<T>(&trimmed).map_err(|e| {
        crate::Error::SchemaViolation(format!("model output is not valid JSON: {e}; raw={raw}"))
    })
}

/// Remove leading ```json and trailing ``` markers from a model output.
pub fn strip_code_fence(raw: &str) -> String {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("```json")
        && let Some(end) = rest.rfind("```")
    {
        return rest[..end].trim().to_owned();
    }
    if let Some(rest) = s.strip_prefix("```")
        && let Some(end) = rest.rfind("```")
    {
        return rest[..end].trim().to_owned();
    }
    s.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Sample {
        a: u32,
        b: String,
    }

    #[test]
    fn json_roundtrip_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.json");
        let v = Sample {
            a: 7,
            b: "ok".into(),
        };
        write_json(&p, &v).unwrap();
        let back: Sample = read_json(&p).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn strip_fence_with_json_tag() {
        let s = "```json\n{\"a\":1,\"b\":\"x\"}\n```";
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 1);
    }

    #[test]
    fn strip_fence_without_tag() {
        let s = "```\n{\"a\":2,\"b\":\"x\"}\n```";
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 2);
    }

    #[test]
    fn strip_fence_passthrough() {
        let s = "{\"a\":3,\"b\":\"x\"}";
        let v: Sample = parse_model_json(s).unwrap();
        assert_eq!(v.a, 3);
    }

    #[test]
    fn invalid_json_errors() {
        let s = "not json";
        let r: std::result::Result<Sample, _> = parse_model_json(s);
        assert!(r.is_err());
    }
}
