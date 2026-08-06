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

/// Compress `src` into `dst` using `c` as the compression format.
/// Returns the output byte count on success.
///
/// `Compression::None` is a straight copy (useful as a no-op when
/// the caller doesn't know the target format up front).
pub fn compress_or_report(
    src: &Path,
    dst: &Path,
    c: Compression,
) -> std::result::Result<u64, CompressionError> {
    let input = std::fs::read(src).map_err(|source| CompressionError::Io {
        path: src.to_path_buf(),
        source,
    })?;
    let out = match c {
        Compression::None => input,
        Compression::Gz => {
            let mut encoder = GzEncoder::new(Vec::new(), FlateCompression::default());
            encoder
                .write_all(&input)
                .map_err(|source| CompressionError::Io {
                    path: dst.to_path_buf(),
                    source,
                })?;
            encoder.finish().map_err(|source| CompressionError::Io {
                path: dst.to_path_buf(),
                source,
            })?
        }
        Compression::Zst => {
            let mut encoder = zstd::Encoder::new(Vec::new(), 0)
                .map_err(|source| CompressionError::Codec(source.to_string()))?;
            encoder
                .write_all(&input)
                .map_err(|source| CompressionError::Io {
                    path: dst.to_path_buf(),
                    source,
                })?;
            encoder.finish().map_err(|source| CompressionError::Io {
                path: dst.to_path_buf(),
                source,
            })?
        }
    };
    let count = out.len() as u64;
    std::fs::write(dst, &out).map_err(|source| CompressionError::Io {
        path: dst.to_path_buf(),
        source,
    })?;
    Ok(count)
}

/// Open a file behind a `Box<dyn Read>` that transparently decodes
/// the selected compression format. Plain files are wrapped in a
/// `BufReader`; `.gz` is decoded with `flate2::GzDecoder`; `.zst`
/// is decoded with `zstd::Decoder`.
///
/// Refs: D.7.5, T16-06 §5.5, T11-04 §D2.
pub fn reader(path: &Path, c: Compression) -> io::Result<Box<dyn Read>> {
    let f = File::open(path)?;
    let buf = BufReader::new(f);
    Ok(match c {
        Compression::None => Box::new(buf),
        Compression::Gz => Box::new(flate2::read::GzDecoder::new(buf)),
        Compression::Zst => Box::new(zstd::Decoder::new(buf)?),
    })
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
/// complete zstd frame on `finish()`. Used by `export_run_tar_zst`
/// (F5) to back the `ExportFormat::TarZst` archive pipeline with a
/// deterministic per-frame output.
pub struct ZstWriter {
    inner: zstd::stream::write::Encoder<'static, File>,
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
        Ok(Self { inner: encoder })
    }

    /// Borrow the inner `File` so a caller (e.g. a tar builder
    /// that wants to flush the file before `finish()`) can
    /// reach the underlying writer. Bytes written through the
    /// returned reference are NOT compressed by the encoder —
    /// callers that need compression should route through
    /// [`Self::as_write_mut`] or [`Self::write`].
    pub fn inner_mut(&mut self) -> &mut File {
        self.inner.get_mut()
    }

    /// Borrow the encoder as a `dyn Write` so a caller (e.g.
    /// `tar::Builder`) can stream data through it and have
    /// every byte compressed by the active zstd frame. The
    /// returned reference is bound to the encoder's lifetime,
    /// so it stays valid until `finish()` consumes `self`.
    pub fn as_write_mut(&mut self) -> &mut dyn Write {
        &mut self.inner
    }

    /// Stream `buf` into the underlying zstd frame. The bytes
    /// do not reach disk until `finish()` (or a `flush()`) is
    /// called.
    pub fn write(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)
    }

    /// Finish the current zstd frame and flush the file to
    /// disk. Returns the inner `File` so callers that want to
    /// keep writing (e.g. a tar builder that needs the
    /// underlying writer for `into_inner()`) can recover it.
    pub fn finish(self) -> io::Result<File> {
        self.inner.finish()
    }
}

/// Build a single `run_<id>.tar.zst` archive containing every
/// file under `run_dir`. The on-disk layout mirrors `tar`/`zstd`
/// defaults: an uncompressed tar stream (no compression between
/// entries) compressed by a single zstd frame around the whole
/// archive. Used by F5's `ExportFormat::TarZst` and by the
/// ad-hoc `moagan telemetry export --format tar.zst` CLI path.
pub fn export_run_tar_zst(run_dir: &Path, out_path: &Path) -> Result<()> {
    if !run_dir.exists() {
        return Err(Error::InvalidState(format!(
            "run dir not found at {}",
            run_dir.display()
        )));
    }
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Io(IoError::CreateDir {
                path: parent.to_path_buf(),
                source: e,
            })
        })?;
    }
    let mut zst = ZstWriter::new(out_path).map_err(|e| Error::Io(IoError::Raw(e)))?;
    {
        let mut builder = tar::Builder::new(zst.as_write_mut());
        append_run_dir(&mut builder, run_dir, run_dir)?;
        builder
            .into_inner()
            .map_err(|e| Error::Io(IoError::Raw(e)))?
            .flush()
            .map_err(|e| {
                Error::Io(IoError::Write {
                    path: out_path.to_path_buf(),
                    source: e,
                })
            })?;
    }
    let mut file = zst.finish().map_err(|e| Error::Io(IoError::Raw(e)))?;
    file.flush().map_err(|e| {
        Error::Io(IoError::Write {
            path: out_path.to_path_buf(),
            source: e,
        })
    })?;
    Ok(())
}

/// Recursively walk `dir` and append every file to the tar
/// builder with a path relative to `root`. Mirrors the helper in
/// `telemetry::export::append_dir` but is kept local so the
/// compression module does not need to depend on the export
/// module (and vice-versa).
fn append_run_dir<W: Write>(builder: &mut tar::Builder<W>, root: &Path, dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| {
        Error::Io(IoError::Read {
            path: dir.to_path_buf(),
            source: e,
        })
    })? {
        let entry = entry.map_err(|e| Error::Io(IoError::Raw(e)))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        let meta = std::fs::metadata(&path).map_err(|e| {
            Error::Io(IoError::Read {
                path: path.clone(),
                source: e,
            })
        })?;
        if meta.is_dir() {
            builder
                .append_dir(rel.to_string_lossy().replace('\\', "/"), &path)
                .map_err(|e| Error::Io(IoError::Raw(e)))?;
            append_run_dir(builder, root, &path)?;
        } else if meta.is_file() {
            let mut f = File::open(&path).map_err(|e| {
                Error::Io(IoError::Read {
                    path: path.clone(),
                    source: e,
                })
            })?;
            builder
                .append_file(rel.to_string_lossy().replace('\\', "/"), &mut f)
                .map_err(|e| Error::Io(IoError::Raw(e)))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn compress_or_report_none() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("source");
        let dst = tmp.path().join("output");
        std::fs::write(&src, b"plain").unwrap();
        assert_eq!(
            compress_or_report(&src, &dst, Compression::None).unwrap(),
            5
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"plain");
    }

    #[test]
    fn compress_or_report_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("source");
        let dst = tmp.path().join("output.gz");
        std::fs::write(&src, b"gzip payload").unwrap();
        let count = compress_or_report(&src, &dst, Compression::Gz).unwrap();
        assert_eq!(count, std::fs::metadata(&dst).unwrap().len());
        // The output must be a valid gzip stream — its first two
        // bytes are the gzip magic 1f 8b.
        let head = std::fs::read(&dst).unwrap();
        assert_eq!(&head[..2], &[0x1f, 0x8b]);
    }

    #[test]
    fn compress_or_report_zst() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("source");
        let dst = tmp.path().join("output.zst");
        std::fs::write(&src, b"zstd payload").unwrap();
        let count = compress_or_report(&src, &dst, Compression::Zst).unwrap();
        assert_eq!(count, std::fs::metadata(&dst).unwrap().len());
        // zstd frames start with magic 28 b5 2f fd.
        let head = std::fs::read(&dst).unwrap();
        assert_eq!(&head[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    }

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

    #[test]
    fn reader_returns_none_for_uncompressed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"a\":2}\n").unwrap();
        let mut r = reader(&path, Compression::None).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert_eq!(buf.lines().count(), 2);
        assert!(buf.contains("\"a\":2"));
    }

    #[test]
    fn reader_returns_gunzip_for_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.jsonl.gz");
        // Encode the same content using the existing gz append path
        // so we exercise the same writer callers will use elsewhere.
        let mut w = open_gz_append(&path).unwrap();
        writeln!(w, "{{\"phase\":\"gzip\"}}").unwrap();
        w.flush().ok();
        drop(w);
        // The new reader must decode it transparently.
        let mut r = reader(&path, Compression::Gz).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert!(buf.contains("\"phase\":\"gzip\""));
    }

    // -- F5: ZstWriter + tar.zst export ------------------------------

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

    /// F5: `export_run_tar_zst` walks `run_dir`, archives each
    /// file via `tar::Builder`, compresses the resulting tar
    /// stream with zstd, and writes the bundle to `out_path`.
    /// Decompressing the bundle and extracting the tar returns
    /// the original files with byte-for-byte fidelity.
    #[test]
    fn tar_zst_roundtrip_extracts_files() {
        let src = tempfile::tempdir().unwrap();
        let src_dir = src.path().to_path_buf();
        std::fs::write(src_dir.join("manifest.json"), b"{\"run\":1}").unwrap();
        std::fs::create_dir_all(src_dir.join("final").join("rankings")).unwrap();
        std::fs::write(src_dir.join("final").join("portfolio.md"), b"# portfolio").unwrap();
        std::fs::write(
            src_dir.join("final").join("rankings").join("ranking.json"),
            b"[]",
        )
        .unwrap();

        let bundle = tempfile::tempdir().unwrap();
        let out_path = bundle.path().join("run.tar.zst");
        export_run_tar_zst(&src_dir, &out_path).expect("tar.zst export succeeds");

        // Decode the zstd frame and stream the tar.
        let f = std::fs::File::open(&out_path).unwrap();
        let zst = zstd::Decoder::new(f).unwrap();
        let mut tar = tar::Archive::new(zst);
        let extract = tempfile::tempdir().unwrap();
        tar.unpack(extract.path()).expect("tar unpacks");

        assert_eq!(
            std::fs::read(extract.path().join("manifest.json")).unwrap(),
            b"{\"run\":1}"
        );
        assert_eq!(
            std::fs::read(extract.path().join("final").join("portfolio.md"),).unwrap(),
            b"# portfolio"
        );
        assert_eq!(
            std::fs::read(
                extract
                    .path()
                    .join("final")
                    .join("rankings")
                    .join("ranking.json"),
            )
            .unwrap(),
            b"[]"
        );
    }

    /// F5: the bundled tar includes the run's `manifest.json`
    /// sidecar. This is the operator-facing guarantee: opening
    /// the archive without re-running the pipeline surfaces the
    /// canonical run metadata. The test seeds a known manifest
    /// payload and confirms it survives the round-trip
    /// verbatim.
    #[test]
    fn tar_zst_export_emits_manifest() {
        let src = tempfile::tempdir().unwrap();
        let src_dir = src.path().to_path_buf();
        let manifest_body =
            b"{\"schema_version\":\"v2\",\"run_id\":\"019f0000-0000-7000-8000-000000000001\"}";
        std::fs::write(src_dir.join("manifest.json"), manifest_body).unwrap();
        std::fs::write(src_dir.join("brief.json"), b"{}").unwrap();

        let bundle = tempfile::tempdir().unwrap();
        let out_path = bundle.path().join("run.tar.zst");
        export_run_tar_zst(&src_dir, &out_path).expect("export succeeds");

        let f = std::fs::File::open(&out_path).unwrap();
        let zst = zstd::Decoder::new(f).unwrap();
        let mut tar = tar::Archive::new(zst);
        let extract = tempfile::tempdir().unwrap();
        tar.unpack(extract.path()).unwrap();

        let archived = std::fs::read(extract.path().join("manifest.json")).unwrap();
        assert_eq!(archived, manifest_body, "manifest.json preserved verbatim");
    }
}
