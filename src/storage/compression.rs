//! Transparent gzip wrapper for JSONL streams.
//!
//! The MVP spec (`docs/proposal-02-rust.md` §1.5) declares the default
//! telemetry compression as `gz` for `phases.jsonl` and `calls.jsonl`,
//! and `none` for `manifest.json`. AGENTS.md's smoke gate #2 then
//! literally checks that the file emitted is `telemetry/calls.jsonl.gz`.
//!
//! The write path uses a `MemberGzWriter` that finishes each gzip
//! member on `flush()` and starts a fresh member on the next `write`.
//! This is what makes the on-disk file a sequence of well-formed
//! gzip members, decodable with `MultiGzDecoder`; without it, the
//! trailing member would be incomplete (`GzDecoder` returns
//! `UnexpectedEof`).
//!
//! Plain `.jsonl` files (legacy runs) remain readable through
//! `read_to_string`, which auto-detects the `.gz` suffix.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

use crate::error::{Error, IoError, Result};

/// Open a file in append mode behind `MemberGzWriter`. Each call to
/// `flush()` completes the current gzip member (header + deflate
/// blocks + CRC32 + length trailer) so the on-disk file is always a
/// valid sequence of gzip members readable by `MultiGzDecoder`.
///
/// The next `write()` after a `flush()` opens a fresh member. If the
/// process crashes between writes, `MultiGzDecoder` reads whatever
/// was already persisted and stops at the truncated trailing member.
pub fn open_gz_append(path: &Path) -> Result<Box<dyn Write + Send>> {
    let f = File::options()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(Box::new(MemberGzWriter::new(f)))
}

/// Open a `.gz` file for reading, behind a `BufReader` and
/// `MultiGzDecoder`. Multi-decoder is the right choice because the
/// write path produces a sequence of complete gzip members rather
/// than a single member spanning the whole file.
pub fn open_gz_read(path: &Path) -> Result<Box<dyn Read + Send>> {
    let f = File::open(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    let buf = BufReader::new(f);
    Ok(Box::new(MultiGzDecoder::new(buf)))
}

/// Open a plain `.jsonl` file for reading. Kept alongside
/// `open_gz_read` so legacy runs (or runs produced before compression
/// was wired) remain readable by manifest builders and external tools.
pub fn open_plain_read(path: &Path) -> Result<Box<dyn Read + Send>> {
    let f = File::open(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(Box::new(BufReader::new(f)))
}

/// Read an entire JSONL stream into a `String`. Used by the manifest
/// builder which already assumes the file fits in memory: phase events
/// for a single run are O(phases × parallel calls), and calls.jsonl is
/// appended incrementally so the manifest reads it once per run.
pub fn read_to_string(path: &Path) -> Result<String> {
    let mut buf = String::new();
    let mut reader: Box<dyn Read> = if is_gz_path(path) {
        open_gz_read(path)?
    } else {
        open_plain_read(path)?
    };
    reader
        .read_to_string(&mut buf)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(buf)
}

/// True when `path` ends in `.gz`. Used by the manifest builder to
/// decide which opener to use.
pub fn is_gz_path(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("gz")
}

/// Append-mode gzip writer that emits a complete gzip member on every
/// `flush()`. Lazily creates the inner `GzEncoder` on the first
/// `write()` and recycles it after each `flush()`.
///
/// Why this exists: a plain `GzEncoder<File>` keeps the gzip member
/// open across writes, so the on-disk file ends in an incomplete
/// member that `GzDecoder` (and even `MultiGzDecoder`) cannot
/// decode. Closing the member on flush turns each flush into a
/// self-contained gzip chunk that a `MultiGzDecoder` walks past.
struct MemberGzWriter {
    file: File,
    current: Option<GzEncoder<File>>,
}

impl MemberGzWriter {
    fn new(file: File) -> Self {
        Self {
            file,
            current: None,
        }
    }

    fn encoder(&mut self) -> io::Result<&mut GzEncoder<File>> {
        if self.current.is_none() {
            let f = self.file.try_clone()?;
            self.current = Some(GzEncoder::new(f, Compression::default()));
        }
        Ok(self.current.as_mut().expect("just initialised"))
    }
}

impl Write for MemberGzWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.encoder()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Take the encoder out, finalize it (writes gzip trailer and
        // returns the File), then flush the file so bytes really hit
        // disk. Drop the encoder to release any borrowed state
        // before the next write opens a new member.
        if let Some(enc) = self.current.take() {
            let mut file = enc.finish()?;
            file.flush()?;
        }
        Ok(())
    }
}

impl Drop for MemberGzWriter {
    fn drop(&mut self) {
        // Best-effort: if the caller forgot to flush, finalize the
        // trailing member on the way out so the file is still valid.
        if self.current.is_some()
            && let Err(e) = self.flush()
        {
            eprintln!("warn: MemberGzWriter drop flush failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn round_trip_writes_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("phases.jsonl.gz");

        let mut w = open_gz_append(&path).unwrap();
        for i in 0..50 {
            writeln!(w, "{{\"phase\":\"p\",\"i\":{i}}}").unwrap();
        }
        w.flush().ok();
        drop(w);

        let raw = read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 50);
        assert!(lines[0].contains("\"i\":0"));
        assert!(lines[49].contains("\"i\":49"));
    }

    #[test]
    fn multi_member_stream_decodes() {
        // Each flush should produce a fresh gzip member; the on-disk
        // file is then a multi-member stream that `MultiGzDecoder`
        // walks past. This is the property that makes the writer
        // crash-safe mid-flush.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("calls.jsonl.gz");

        let mut w = open_gz_append(&path).unwrap();
        for i in 0..3 {
            writeln!(w, "{{\"phase\":\"p\",\"i\":{i}}}").unwrap();
            w.flush().ok();
        }
        drop(w);

        let raw = read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("\"i\":2"));
    }

    #[test]
    fn plain_reader_still_works() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("calls.jsonl");
        std::fs::write(&path, "{\"x\":1}\n{\"x\":2}\n").unwrap();
        let raw = read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2);
    }

    #[test]
    fn gz_path_detection() {
        assert!(is_gz_path(Path::new("calls.jsonl.gz")));
        assert!(!is_gz_path(Path::new("calls.jsonl")));
        assert!(!is_gz_path(Path::new("manifest.json")));
    }
}
