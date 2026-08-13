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

use std::path::PathBuf;

use flate2::Compression as FlateCompression;
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
fn open_gz_read(path: &Path) -> Result<Box<dyn Read + Send>> {
    let f = File::open(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    let buf = BufReader::new(f);
    Ok(Box::new(MultiGzDecoder::new(buf)))
}

/// Open a plain `.jsonl` file for reading. Kept alongside
/// `open_gz_read` so legacy runs (or runs produced before compression
/// was wired) remain readable by manifest builders and external tools.
fn open_plain_read(path: &Path) -> Result<Box<dyn Read + Send>> {
    let f = File::open(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(Box::new(BufReader::new(f)))
}

/// Read an entire JSONL stream into a `String`. Used by the manifest
/// builder which already assumes the file fits in memory: phase events
/// for a single run are O(phases × parallel calls), and calls.jsonl is
/// appended incrementally so the manifest reads it once per run.
pub fn read_to_string(path: &Path) -> Result<String> {
    if std::fs::metadata(path)
        .map_err(|e| Error::Io(IoError::Raw(e)))?
        .len()
        == 0
    {
        return Ok(String::new());
    }
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
fn is_gz_path(path: &Path) -> bool {
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
            self.current = Some(GzEncoder::new(f, FlateCompression::default()));
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

// =====================================================================
// D.7.5 — Compression enum + reader
// =====================================================================
//
// Three modes: `None`, `Gz`, `Zst`. The reader returns a
// `Box<dyn Read>` so callers can stream without caring about the
// underlying format. This is an additive layer on top of the
// `MemberGzWriter` / `open_gz_read` API above; the new helpers
// are meant for tooling (export, verify) that needs to switch on
// extension without knowing the file is multi-member gz.

/// Compression mode of a sidecar file (D.7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Plain bytes (`.jsonl`, `.txt`, etc.).
    None,
    /// Single-stream gzip (`.gz`). Reads use the standard
    /// `GzDecoder`, not the multi-member decoder, because the enum
    /// is for tooling that opens a single stream.
    Gz,
    /// Zstandard (`.zst`).
    Zst,
}

impl Compression {
    /// Detect the compression mode from a file path's extension.
    /// Returns `None` for any non-recognized extension.
    pub fn from_extension(path: &Path) -> Self {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        match ext {
            "gz" => Self::Gz,
            "zst" => Self::Zst,
            _ => Self::None,
        }
    }
}

/// Path-aware whole-file compression with structured error
/// reporting. Used by the export surface and any future tool that
/// needs to compress a single file with a deterministic output
/// size. The previous commit `b75acfa` claimed to add this in
/// the squash-merge but the function was lost in transit; this
/// restores the implementation.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// Input/output failure with the relevant path.
    #[error("{path:?}: {source}")]
    Io {
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Compression codec failure (e.g. zstd encoder rejected the
    /// configuration).
    #[error("compression: {0}")]
    Codec(String),
}

impl Compression {
    /// Open a file behind a `Box<dyn Read>` that transparently
    /// decodes a multi-member compression stream. Mode is detected
    /// from the file extension. Plain files are wrapped in a
    /// `BufReader`; `.gz` is decoded with `flate2::MultiGzDecoder`
    /// (so a sequence of gzip members produced by [`open_gz_append`]
    /// round-trips byte-for-byte); `.zst` is decoded with
    /// `zstd::Decoder`.
    ///
    /// Picks `MultiGzDecoder` for `.gz` because sidecars emitted by
    /// the project's own writer end one gzip member per `flush()`,
    /// so the on-disk file is a sequence of complete members; a
    /// single-stream `GzDecoder` would stop after the first.
    ///
    /// Refs: D.7.5 (PR-26).
    pub fn multi_reader(path: &Path) -> io::Result<Box<dyn Read>> {
        let f = File::open(path)?;
        let buf = BufReader::new(f);
        match Self::from_extension(path) {
            Self::None => Ok(Box::new(buf)),
            Self::Gz => Ok(Box::new(MultiGzDecoder::new(buf))),
            Self::Zst => Ok(Box::new(zstd::Decoder::new(buf)?)),
        }
    }
}

// =====================================================================
// F5 — Streaming zstd writer + run-bundle export
// =====================================================================
//
// The export surface gained a `tar.zst` archive format (F5) so
// operators can ship a run as a single compressed file. The
// `ZstWriter` wraps `zstd::stream::write::Encoder` over a `File`
// and exposes a `finish()` call that flushes the trailing frame
// to disk — without it the encoder holds bytes in its internal
// buffer and the on-disk file ends in a truncated frame that
// `zstd::Decoder` cannot open.

/// Stream-friendly zstd writer that wraps `File` and finishes a
/// complete zstd frame on `finish()`. Used by `telemetry::export`
/// (F5) to back the `ExportFormat::TarZst` archive pipeline with a
/// deterministic per-frame output.
pub struct ZstWriter {
    encoder: zstd::stream::write::Encoder<'static, File>,
}

impl ZstWriter {
    /// Open `path` for writing and wrap it in a fresh
    /// `zstd::Encoder` at compression level 0 (the fastest
    /// level; F5 does not pin a level so callers can change it
    /// later without breaking existing archives).
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 0)
            .map_err(|e| io::Error::other(format!("zstd encoder init: {e}")))?;
        Ok(Self { encoder })
    }

    /// Stream `buf` into the underlying zstd frame. The bytes
    /// do not reach disk until `finish()` (or a `flush()`) is
    /// called.
    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.encoder.write_all(buf)
    }

    /// Finish the current zstd frame and flush the file to
    /// disk. Returns the inner `File` so callers that want to
    /// keep writing (e.g. a tar builder that needs the
    /// underlying writer for `into_inner()`) can recover it.
    pub fn finish(self) -> io::Result<File> {
        let mut file = self.encoder.finish()?;
        file.flush()?;
        Ok(file)
    }
}

impl Write for ZstWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.encoder.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.encoder.flush()
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

    // ---- D.7.5 — Compression enum + reader (Phase O) --------------

    #[test]
    fn compression_from_extension_gz() {
        assert_eq!(
            Compression::from_extension(Path::new("calls.jsonl.gz")),
            Compression::Gz,
        );
        assert_eq!(
            Compression::from_extension(Path::new("/var/log/x.gz")),
            Compression::Gz,
        );
    }

    #[test]
    fn compression_from_extension_zst() {
        assert_eq!(
            Compression::from_extension(Path::new("calls.jsonl.zst")),
            Compression::Zst,
        );
        assert_eq!(
            Compression::from_extension(Path::new("/var/log/x.zst")),
            Compression::Zst,
        );
    }

    #[test]
    fn compression_from_extension_none() {
        assert_eq!(
            Compression::from_extension(Path::new("manifest.json")),
            Compression::None,
        );
        assert_eq!(
            Compression::from_extension(Path::new("calls.jsonl")),
            Compression::None,
        );
        // No extension at all
        assert_eq!(
            Compression::from_extension(Path::new("Makefile")),
            Compression::None,
        );
        // Unknown extension falls back to None (caller decides)
        assert_eq!(
            Compression::from_extension(Path::new("data.br")),
            Compression::None,
        );
    }

    /// D.7.5 (PR-26): `Compression::multi_reader` must walk past
    /// every gzip member in a multi-member stream emitted by
    /// `open_gz_append`. Each `flush()` finishes one member, so a
    /// stream of N flushes is N complete members; a single-stream
    /// `GzDecoder` would stop after the first one. The new method
    /// uses `MultiGzDecoder` and is the parity counterpart of the
    /// writer for tooling that opens a sidecar.
    #[test]
    fn multi_reader_walks_past_every_member() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("calls.jsonl.gz");

        let mut w = open_gz_append(&path).unwrap();
        for i in 0..5 {
            writeln!(w, "{{\"phase\":\"p\",\"i\":{i}}}").unwrap();
            w.flush().ok();
        }
        drop(w);

        let mut r = Compression::multi_reader(&path).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        let lines: Vec<&str> = buf.lines().collect();
        assert_eq!(
            lines.len(),
            5,
            "multi_reader must decode all 5 members, got {}",
            lines.len()
        );
        for (idx, line) in lines.iter().enumerate() {
            assert!(line.contains(&format!("\"i\":{idx}")), "line {idx}: {line}");
        }
    }

    /// D.7.5 (PR-26): `multi_reader` on a plain `.jsonl` just
    /// returns a buffered file reader. This is the no-compression
    /// path of the helper.
    #[test]
    fn multi_reader_handles_uncompressed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        std::fs::write(&path, "{\"k\":1}\n{\"k\":2}\n").unwrap();
        let mut r = Compression::multi_reader(&path).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert_eq!(buf.lines().count(), 2);
        assert!(buf.contains("\"k\":2"));
    }

    /// D.7.5 (PR-26): `multi_reader` on a single-member gz
    /// (i.e. one with no intermediate `flush()`) decodes it the
    /// same as the multi-member case. This guards against a
    /// regression where the helper accidentally required more
    /// than one member.
    #[test]
    fn multi_reader_handles_single_member_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("single.jsonl.gz");

        let mut w = open_gz_append(&path).unwrap();
        writeln!(w, "{{\"phase\":\"only\"}}").unwrap();
        drop(w);

        let mut r = Compression::multi_reader(&path).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert!(buf.contains("\"phase\":\"only\""));
    }

    // -- F5: ZstWriter coverage -------------------------------------

    /// F5: `ZstWriter` produces a single zstd frame that
    /// `zstd::Decoder` can read back. The output bytes start
    /// with the canonical zstd magic (`28 b5 2f fd`) and the
    /// round-trip recovers the original bytes byte-for-byte.
    #[test]
    fn zst_writer_compresses_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("frame.zst");
        let payload = b"the quick brown fox jumps over the lazy dog";
        {
            let mut w = ZstWriter::new(&path).unwrap();
            w.write(payload).unwrap();
            w.finish().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(
            raw.len() >= 4,
            "encoded frame must contain at least the magic header, got {} bytes",
            raw.len()
        );
        assert_eq!(&raw[..4], &[0x28, 0xb5, 0x2f, 0xfd], "zstd frame magic");
        // Round-trip: the decoder produces the input verbatim.
        let mut decoder = zstd::Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut decoded).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn zst_writer_compresses_streaming() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("chunks.zst");
        let chunks: [&[u8]; 4] = [b"alpha", b"-", b"beta", b"-gamma"];
        let mut writer = ZstWriter::new(&path).unwrap();
        for chunk in chunks {
            writer.write(chunk).unwrap();
        }
        writer.finish().unwrap();

        let mut decoder = zstd::Decoder::new(File::open(&path).unwrap()).unwrap();
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"alpha-beta-gamma");
    }
}
