//! End-to-end integration tests for Phase O (v0.3 sub-fase O —
//! rubric anchoring + compression enum).
//!
//! Phase O closes when:
//!
//! 1. `Rubric::default()` returns the same anchor string for a
//!    `(Criterion, level)` pair on every call (idempotent — useful
//!    for the LLM prompt that interpolates the anchors).
//! 2. `Compression::multi_reader` round-trips a file in each of the
//!    three modes (None, Gz, Zst) so a downstream tool that picks
//!    the reader by extension can stream without caring about the
//!    on-disk format.
//!
//! These tests complement the unit tests in `src/ranking/rubric.rs`
//! and `src/storage/compression.rs` (which pin individual anchors
//! and individual extensions) by exercising the integration points
//! — the public API the rest of the project will import.

#![allow(clippy::await_holding_lock)]

use std::io::{Read, Write};

use flate2::Compression as FlateCompression;
use flate2::write::GzEncoder;

use moagan::ranking::{Criterion, Rubric};
use moagan::storage::compression::Compression;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// The same `(Criterion, level)` pair must return the same anchor
/// string on every call. The LLM prompt interpolates the anchor
/// into the prompt template, so a flaky or mutating implementation
/// would produce prompts that drift across calls.
#[test]
fn rubric_anchors_are_stable_across_calls() {
    let _g = env_lock();
    let r = Rubric::default();
    for c in [
        Criterion::Correctness,
        Criterion::Completeness,
        Criterion::Fit,
        Criterion::Evidence,
        Criterion::Clarity,
        Criterion::Overall,
    ] {
        // Build a fresh Rubric every iteration so we are testing
        // the seeded defaults rather than the same HashMap twice.
        let one = Rubric::default().anchored_1(c).to_string();
        let three = Rubric::default().anchored_3(c).to_string();
        let five = Rubric::default().anchored_5(c).to_string();
        assert_eq!(r.anchored_1(c), one);
        assert_eq!(r.anchored_3(c), three);
        assert_eq!(r.anchored_5(c), five);
        // Anchors are non-empty.
        assert!(!one.is_empty());
        assert!(!three.is_empty());
        assert!(!five.is_empty());
        // 1 != 3 != 5 (sanity: the seed isn't accidentally constant).
        assert_ne!(one, three);
        assert_ne!(three, five);
        assert_ne!(one, five);
    }
}

/// Round-trip a `.gz` file through `Compression::multi_reader`.
/// Uses the standard `flate2::GzEncoder` so the test is independent
/// of the project's own writer (the writer's own round-trip is
/// covered by `open_gz_append` + `read_to_string`).
#[test]
fn compression_reader_handles_gz_file() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("calls.jsonl.gz");
    let original = "{\"phase\":\"p\",\"i\":1}\n{\"phase\":\"p\",\"i\":2}\n";
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut enc = GzEncoder::new(f, FlateCompression::default());
        enc.write_all(original.as_bytes()).unwrap();
        enc.finish().unwrap();
    }
    let mut r = Compression::multi_reader(&path).unwrap();
    let mut buf = String::new();
    r.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, original);
    assert_eq!(Compression::from_extension(&path), Compression::Gz);
}

/// Round-trip a `.zst` file through `Compression::multi_reader`.
/// `zstd` does not have a single-call encoder API in the version
/// pinned by `Cargo.toml` (`0.13`), so we use the simple
/// `zstd::stream::encode_all` helper that ships with the crate.
#[test]
fn compression_reader_handles_zst_file() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("calls.jsonl.zst");
    let original =
        "{\"phase\":\"z\",\"i\":1}\n{\"phase\":\"z\",\"i\":2}\n{\"phase\":\"z\",\"i\":3}\n";
    let encoded = zstd::stream::encode_all(original.as_bytes(), 3).unwrap();
    std::fs::write(&path, &encoded).unwrap();
    let mut r = Compression::multi_reader(&path).unwrap();
    let mut buf = String::new();
    r.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, original);
    assert_eq!(Compression::from_extension(&path), Compression::Zst);
}

/// Round-trip a plain (uncompressed) file through
/// `Compression::multi_reader`. Useful for tools that receive
/// mixed-format inputs and must pick the reader by extension.
#[test]
fn compression_reader_handles_uncompressed() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("manifest.json");
    let original = "{\"schema_version\":\"v1\",\"ok\":true}\n";
    std::fs::write(&path, original).unwrap();
    let mut r = Compression::multi_reader(&path).unwrap();
    let mut buf = String::new();
    r.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, original);
    assert_eq!(Compression::from_extension(&path), Compression::None);
}
