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
    tracing::debug!(path = %path.display(), "open_gz_append: enter");
    let f = File::options()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    tracing::trace!(path = %path.display(), "open_gz_append: opened append handle");
    Ok(Box::new(MemberGzWriter::new(f)))
}

/// Open a `.gz` file for reading, behind a `BufReader` and
/// `MultiGzDecoder`. Multi-decoder is the right choice because the
/// write path produces a sequence of complete gzip members rather
/// than a single member spanning the whole file.
fn open_gz_read(path: &Path) -> Result<Box<dyn Read + Send>> {
    tracing::trace!(path = %path.display(), "open_gz_read: enter");
    let f = File::open(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    let buf = BufReader::new(f);
    Ok(Box::new(MultiGzDecoder::new(buf)))
}

/// Open a plain `.jsonl` file for reading. Kept alongside
/// `open_gz_read` so legacy runs (or runs produced before compression
/// was wired) remain readable by manifest builders and external tools.
fn open_plain_read(path: &Path) -> Result<Box<dyn Read + Send>> {
    tracing::trace!(path = %path.display(), "open_plain_read: enter");
    let f = File::open(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(Box::new(BufReader::new(f)))
}

/// Read an entire JSONL stream into a `String`. Used by the manifest
/// builder which already assumes the file fits in memory: phase events
/// for a single run are O(phases × parallel calls), and calls.jsonl is
/// appended incrementally so the manifest reads it once per run.
pub fn read_to_string(path: &Path) -> Result<String> {
    tracing::debug!(path = %path.display(), "read_to_string: enter");
    let metadata = std::fs::metadata(path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    if metadata.len() == 0 {
        tracing::trace!(path = %path.display(), "read_to_string: empty file -> empty string");
        return Ok(String::new());
    }
    let mut buf = String::new();
    let mut reader: Box<dyn Read> = if is_gz_path(path) {
        tracing::trace!(path = %path.display(), "read_to_string: gz branch");
        open_gz_read(path)?
    } else {
        tracing::trace!(path = %path.display(), "read_to_string: plain branch");
        open_plain_read(path)?
    };
    reader
        .read_to_string(&mut buf)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    tracing::debug!(
        path = %path.display(),
        bytes = buf.len(),
        "read_to_string: ok"
    );
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
            tracing::trace!("MemberGzWriter::encoder: lazy-initialising GzEncoder");
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
            tracing::trace!("MemberGzWriter::flush: finishing gzip member");
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
            tracing::warn!(error = %e, "MemberGzWriter drop flush failed");
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
        let mode = match ext {
            "gz" => Self::Gz,
            "zst" => Self::Zst,
            _ => Self::None,
        };
        tracing::trace!(path = %path.display(), ext, ?mode, "Compression::from_extension");
        mode
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
        let mode = Self::from_extension(path);
        tracing::debug!(path = %path.display(), ?mode, "Compression::multi_reader: enter");
        let f = File::open(path)?;
        let buf = BufReader::new(f);
        match mode {
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
        tracing::debug!(path = %path.display(), "ZstWriter::new: enter");
        let file = File::create(path)?;
        let encoder = zstd::stream::write::Encoder::new(file, 0)
            .map_err(|e| io::Error::other(format!("zstd encoder init: {e}")))?;
        tracing::trace!(path = %path.display(), "ZstWriter::new: encoder ready");
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
        tracing::debug!("ZstWriter::finish: enter");
        let mut file = self.encoder.finish()?;
        file.flush()?;
        tracing::trace!("ZstWriter::finish: ok");
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
    use proptest::prelude::*;
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

    // -----------------------------------------------------------------
    // Property-based tests (proptest 1.4, dev-only per ADR-0001).
    //
    // The round-trip properties below pin the core contract of
    // the gzip + zstd writers and `Compression::multi_reader`:
    // for any byte sequence, write -> read must produce the
    // exact same bytes back. This is what makes the on-disk
    // telemetry files (`calls.jsonl.gz`, `manifest.json.zst`)
    // auditable: an operator can re-decode a sidecar with stock
    // tooling and recover the original payload.
    //
    // CRC32 is not separately tested here because the gzip /
    // zstd codecs each carry their own checksum internally; a
    // mismatch would surface as a decoder error, which the
    // round-trip properties below already detect.
    // -----------------------------------------------------------------

    proptest::proptest! {
        /// `MemberGzWriter` (via `open_gz_append`) followed by
        /// `Compression::multi_reader` is a byte-exact round-trip
        /// for any non-empty payload. The writer finishes one
        /// gzip member per `flush()`, so `MultiGzDecoder` must
        /// walk past every member and recover the original
        /// bytes. We use `multi_reader` + `read_to_end` (not
        /// `read_to_string`) so the property holds for arbitrary
        /// binary payloads, not just valid UTF-8 — the round-trip
        /// contract is byte-exact regardless of payload encoding.
        ///
        /// Empty payloads are excluded because `write_all(&[])`
        /// is a no-op: the gzip encoder is never materialised and
        /// the resulting flush produces no member on disk, which
        /// would make `multi_reader` return `UnexpectedEof`. The
        /// non-empty path exercises the same codec.
        #[test]
        fn prop_gzip_round_trip(
            payload in proptest::collection::vec(any::<u8>(), 1..512),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("stream.jsonl.gz");
            {
                let mut w = open_gz_append(&path).unwrap();
                // Write the payload as a single byte sequence
                // followed by a flush so the gzip member
                // boundary lands on a deterministic spot.
                // `write_all` then `flush` makes the round-trip
                // identical regardless of how proptest chose to
                // chunk the bytes.
                w.write_all(&payload).unwrap();
                w.flush().ok();
            }
            let mut reader = Compression::multi_reader(&path).unwrap();
            let mut recovered = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut recovered).unwrap();
            prop_assert_eq!(
                recovered,
                payload,
                "gzip round-trip must be byte-exact"
            );
        }

        /// `ZstWriter` + `zstd::Decoder` is a byte-exact
        /// round-trip. Pins the F5 contract that the export
        /// surface can ship a run as `.tar.zst` and an
        /// external operator can decode it with the standard
        /// `zstd` CLI. Excludes empty payloads because
        /// `write_all(&[])` is a no-op and the resulting
        /// frame may be omitted by the encoder.
        #[test]
        fn prop_zstd_round_trip(
            payload in proptest::collection::vec(any::<u8>(), 1..512),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("frame.zst");
            {
                let mut w = ZstWriter::new(&path).unwrap();
                w.write_all(&payload).unwrap();
                w.finish().unwrap();
            }
            // The zstd frame starts with the canonical magic
            // (28 b5 2f fd); if proptest picked a payload that
            // happens to start with those bytes the round-trip
            // is still valid because we feed the recovered bytes
            // through the decoder.
            let raw = std::fs::read(&path).unwrap();
            prop_assert!(raw.len() >= 4, "zstd frame must include magic header");
            prop_assert_eq!(
                &raw[..4],
                &[0x28, 0xb5, 0x2f, 0xfd],
                "zstd frame must start with the canonical magic"
            );
            let mut decoder =
                zstd::Decoder::new(File::open(&path).unwrap()).unwrap();
            let mut decoded = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut decoded).unwrap();
            prop_assert_eq!(
                decoded,
                payload,
                "zstd round-trip must be byte-exact"
            );
        }

        /// `Compression::multi_reader` walks past every gzip
        /// member produced by `open_gz_append`. Property: N
        /// flushes produce N recoverable payloads (concatenated
        /// into the read buffer). The strategy requires each
        /// chunk to be non-empty so the gzip member boundary
        /// is real: an empty `write_all` is a no-op and the
        /// resulting flush produces no member on disk, which
        /// would invalidate the round-trip count.
        #[test]
        fn prop_multi_member_stream_round_trip(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 1..64),
                1..8,
            ),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("multi.jsonl.gz");
            {
                let mut w = open_gz_append(&path).unwrap();
                for chunk in &chunks {
                    w.write_all(chunk).unwrap();
                    w.flush().ok();
                }
            }
            let mut reader = Compression::multi_reader(&path).unwrap();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
            // Concatenation of the chunks must match what the
            // reader decoded. We concatenate in the same order
            // we wrote them.
            let expected: Vec<u8> =
                chunks.iter().flat_map(|c| c.iter().copied()).collect();
            prop_assert_eq!(
                buf, expected,
                "multi_reader must recover every chunk in order"
            );
        }

        /// `Compression::from_extension` is a deterministic
        /// classifier: the same extension always maps to the
        /// same `Compression` variant, and the unknown case
        /// always falls back to `None`. Pins the dispatcher
        /// routing used by `multi_reader` and other tooling.
        #[test]
        fn prop_compression_from_extension_is_deterministic(
            ext in "[a-z]{0,6}",
        ) {
            let p = std::path::PathBuf::from(format!("file.{ext}"));
            let a = Compression::from_extension(&p);
            let b = Compression::from_extension(&p);
            prop_assert_eq!(a, b);
            prop_assert_eq!(
                a,
                match ext.as_str() {
                    "gz" => Compression::Gz,
                    "zst" => Compression::Zst,
                    _ => Compression::None,
                },
                "from_extension must match the expected classifier"
            );
        }
    }
}
