//! `moagan telemetry export` — bundle a run into a portable
//! archive with a SHA256SUMS manifest.
//!
//! Mirrors `proposal-02-rust.md §10.9` and `V4 §9.1-§9.2`. The
//! archive is produced in three steps:
//!
//! 1. Stage the selected artefacts into a temporary directory
//!    named after the run id.
//! 2. Stream-hash every staged file and write
//!    `<staging>/SHA256SUMS` in the canonical
//!    `<sha256>  <relative-path>` shape (same format as
//!    GNU coreutils).
//! 3. Bundle the staged tree into the requested container
//!    (`tar.gz`, `tar`, or `zip`).
//!
//! Compression is delegated to the `tar` and `zip` crates; the
//! `flate2` and `zstd` crates are already direct dependencies.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::Builder as TarBuilder;
use walkdir::WalkDir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::cli::telemetry_cmd::{ExportFormat, ExportLevel};
use crate::error::{Error, IoError, Result};
use crate::fs_layout::RunDir;
use crate::ids::RunId;

/// One entry in a `SHA256SUMS` manifest. Used by `verify` (commit 8)
/// and exposed to the rest of the crate for unit tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashEntry {
    /// SHA-256 hex digest.
    pub sha256: String,
    /// Path relative to the staging root, using forward slashes.
    pub path: String,
}

/// Format a list of `HashEntry` into the canonical SHA256SUMS text
/// (one `<sha256>  <path>` per line, LF terminated, no trailing
/// newline). Mirrors `sha256sum -b` output.
pub fn format_sha256sums(entries: &[HashEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        // Note the two spaces between digest and path (sha256sum
        // binary mode; matches what most tools verify against).
        out.push_str(&format!("{}  {}\n", e.sha256, e.path));
    }
    out
}

/// Parse a `SHA256SUMS` text body into the canonical entry list.
/// Tolerant of CRLF and surrounding whitespace so files produced by
/// different `sha256sum` builds round-trip cleanly.
pub fn parse_sha256sums(body: &str) -> Result<Vec<HashEntry>> {
    let mut out = Vec::new();
    for (idx, line) in body.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next() else {
            return Err(Error::InvalidArgs(format!(
                "SHA256SUMS line {}: missing digest",
                idx + 1
            )));
        };
        let Some(path) = parts.next() else {
            return Err(Error::InvalidArgs(format!(
                "SHA256SUMS line {}: missing path",
                idx + 1
            )));
        };
        // The canonical GNU layout uses two spaces between digest
        // and path; tolerate any whitespace separator.
        let _ = parts.next();
        out.push(HashEntry {
            sha256: sha.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(out)
}

/// Compute the SHA-256 of a single file in streaming fashion. Reads
/// through a 64 KiB buffer so multi-GB files stay bounded in RAM.
pub fn sha256_file(path: &Path) -> Result<String> {
    let f = File::open(path).map_err(|e| {
        Error::Io(IoError::Read {
            path: path.to_path_buf(),
            source: e,
        })
    })?;
    let mut reader = BufReader::with_capacity(64 * 1024, f);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| {
            Error::Io(IoError::Read {
                path: path.to_path_buf(),
                source: e,
            })
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

/// Result of an export run.
#[derive(Debug, Clone)]
pub struct ExportResult {
    /// Path to the produced archive on disk.
    pub archive_path: PathBuf,
    /// Number of files bundled (excluding the `SHA256SUMS` manifest
    /// itself).
    pub file_count: usize,
    /// SHA-256 of the archive (informational; verification happens
    /// through `moagan telemetry verify` on the bundle, not on this
    /// outer envelope).
    pub archive_sha256: String,
    /// Total bytes of the staged payload before archiving.
    pub payload_bytes: u64,
    /// Archive bytes.
    pub archive_bytes: u64,
}

/// Run an export. `run_dir` is the canonical run directory the
/// pipeline produced; `out` is the destination archive path (parent
/// must exist).
pub fn export_run(
    run_dir: &RunDir<'_>,
    run_id: RunId,
    level: ExportLevel,
    format: ExportFormat,
    out: &Path,
) -> Result<ExportResult> {
    if !run_dir.root().exists() {
        return Err(Error::InvalidState(format!(
            "run {run_id} directory not found at {}",
            run_dir.root().display()
        )));
    }

    // Stage 1: copy selected artefacts to a temporary directory.
    let staging_root = tempfile::tempdir()?;
    let staging_dir = staging_root.path().join(format!("run_{run_id}_export"));
    std::fs::create_dir_all(&staging_dir)?;
    let included = collect_files(run_dir, level)?;
    let mut payload_bytes: u64 = 0;
    for (src, rel) in &included {
        let dest = staging_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, &dest).map_err(|e| {
            Error::Io(IoError::Write {
                path: dest.clone(),
                source: e,
            })
        })?;
        payload_bytes += std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    }

    // Stage 2: hash every staged file in deterministic order so the
    // SHA256SUMS line order is stable across exports of the same
    // run.
    let mut entries = Vec::with_capacity(included.len());
    for (_, rel) in &included {
        let path = staging_dir.join(rel);
        let sha = sha256_file(&path)?;
        entries.push(HashEntry {
            sha256: sha,
            path: rel.clone(),
        });
    }
    let sums_path = staging_dir.join("SHA256SUMS");
    std::fs::write(&sums_path, format_sha256sums(&entries))?;

    // Stage 3: bundle the staged tree into the requested container.
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file_count = included.len();
    let archive_bytes = match format {
        ExportFormat::TarGz => write_tar_gz(&staging_dir, out)?,
        ExportFormat::Tar => write_tar(&staging_dir, out)?,
        ExportFormat::Zip => write_zip(&staging_dir, out)?,
        ExportFormat::TarZst => write_tar_zst(&staging_dir, out)?,
    };
    let archive_sha256 = sha256_file(out)?;

    // Drop the tempdir eagerly; it would happen at scope exit anyway
    // but dropping early surfaces errors here instead of at the
    // caller's `?`.
    drop(staging_root);

    Ok(ExportResult {
        archive_path: out.to_path_buf(),
        file_count,
        archive_sha256,
        payload_bytes,
        archive_bytes,
    })
}

/// Decide which files are included in the export at the requested
/// level. Returns `(absolute, relative-to-run-dir)` pairs.
fn collect_files(run_dir: &RunDir<'_>, level: ExportLevel) -> Result<Vec<(PathBuf, String)>> {
    let always = ["manifest.json", "brief.json", "rankings/ranking.json"];
    let level_specific: &[&str] = match level {
        ExportLevel::Summary => &[
            "sketches/",
            "proposals/",
            "critiques/",
            "revisions/",
            "evaluations/",
            "final/",
        ],
        ExportLevel::Full => &[
            "sketches/",
            "proposals/",
            "critiques/",
            "revisions/",
            "validation/",
            "evaluations/",
            "rankings/",
            "final/",
            "synthesized/",
            "cluster_proposals/",
            "adversaries/",
            "checkpoints/",
            "telemetry/calls.jsonl.gz",
            "telemetry/phases.jsonl.gz",
            "telemetry/warnings.jsonl",
            "telemetry/checkpoints.jsonl",
        ],
    };
    let mut out = Vec::new();
    let root = run_dir.root();
    for rel in always {
        let abs = root.join(rel);
        if abs.exists() {
            out.push((abs, (*rel).to_owned()));
        }
    }
    for rel in level_specific {
        let abs = root.join(rel);
        if !abs.exists() {
            continue;
        }
        if abs.is_dir() {
            for entry in WalkDir::new(&abs)
                .follow_links(false)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                if entry.file_type().is_file() {
                    let path = entry.path().to_path_buf();
                    let stripped = path
                        .strip_prefix(root)
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    if !stripped.is_empty() {
                        out.push((path, stripped));
                    }
                }
            }
        } else if abs.is_file() {
            out.push((abs, (*rel).to_owned()));
        }
    }
    // Deterministic ordering so the SHA256SUMS manifest is stable.
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

fn write_tar(staging_dir: &Path, out: &Path) -> Result<u64> {
    let f = File::create(out).map_err(|e| {
        Error::Io(IoError::CreateFile {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    let mut builder = TarBuilder::new(BufWriter::new(f));
    append_dir(&mut builder, staging_dir, staging_dir)?;
    let mut f = builder
        .into_inner()
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    f.flush().map_err(|e| {
        Error::Io(IoError::Write {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

fn write_tar_gz(staging_dir: &Path, out: &Path) -> Result<u64> {
    let f = File::create(out).map_err(|e| {
        Error::Io(IoError::CreateFile {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    let gz = GzEncoder::new(BufWriter::new(f), Compression::default());
    let mut builder = TarBuilder::new(gz);
    append_dir(&mut builder, staging_dir, staging_dir)?;
    let gz = builder
        .into_inner()
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    let mut f = gz.finish().map_err(|e| Error::Io(IoError::Raw(e)))?;
    f.flush().map_err(|e| {
        Error::Io(IoError::Write {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

fn write_zip(staging_dir: &Path, out: &Path) -> Result<u64> {
    let f = File::create(out).map_err(|e| {
        Error::Io(IoError::CreateFile {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    let mut zip = zip::ZipWriter::new(f);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    append_zip_dir(&mut zip, staging_dir, staging_dir, options)?;
    zip.finish()
        .map_err(|e| Error::Io(IoError::Raw(zip_error_to_io(&e))))?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

/// F5: write the staged tree as a `tar.zst` archive. The tar
/// stream is uncompressed between entries; the entire stream
/// is then compressed with a single zstd frame via
/// [`crate::storage::compression::ZstWriter`]. The compression
/// module owns the writer so the export module does not need to
/// learn zstd directly.
fn write_tar_zst(staging_dir: &Path, out: &Path) -> Result<u64> {
    let mut zst = crate::storage::compression::ZstWriter::new(out).map_err(|e| {
        Error::Io(IoError::CreateFile {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    {
        let mut builder = TarBuilder::new(zst.as_write_mut());
        append_dir(&mut builder, staging_dir, staging_dir)?;
        builder
            .into_inner()
            .map_err(|e| Error::Io(IoError::Raw(e)))?
            .flush()
            .map_err(|e| {
                Error::Io(IoError::Write {
                    path: out.to_path_buf(),
                    source: e,
                })
            })?;
    }
    let mut f = zst.finish().map_err(|e| Error::Io(IoError::Raw(e)))?;
    f.flush().map_err(|e| {
        Error::Io(IoError::Write {
            path: out.to_path_buf(),
            source: e,
        })
    })?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

/// Bridge `zip::result::ZipError` into the crate's `io::Error`
/// wrapper. `zip` doesn't implement `Into<io::Error>` because some
/// variants carry an `io::Error` payload and others don't; we
/// unwrap the inner error when present and stringify the rest.
fn zip_error_to_io(err: &zip::result::ZipError) -> std::io::Error {
    use zip::result::ZipError as Z;
    match err {
        Z::Io(e) => std::io::Error::new(e.kind(), format!("{e}")),
        other => std::io::Error::other(format!("zip: {other}")),
    }
}

fn append_dir<W: Write>(builder: &mut TarBuilder<W>, root: &Path, dir: &Path) -> Result<()> {
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
            append_dir(builder, root, &path)?;
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

fn append_zip_dir<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    root: &Path,
    dir: &Path,
    options: SimpleFileOptions,
) -> Result<()> {
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
        let name = rel.to_string_lossy().replace('\\', "/");
        if meta.is_dir() {
            zip.add_directory(name, options)
                .map_err(|e| Error::Io(IoError::Raw(zip_error_to_io(&e))))?;
            append_zip_dir(zip, root, &path, options)?;
        } else if meta.is_file() {
            zip.start_file(name, options)
                .map_err(|e| Error::Io(IoError::Raw(zip_error_to_io(&e))))?;
            let mut f = File::open(&path).map_err(|e| {
                Error::Io(IoError::Read {
                    path: path.clone(),
                    source: e,
                })
            })?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = f.read(&mut buf).map_err(|e| {
                    Error::Io(IoError::Read {
                        path: path.clone(),
                        source: e,
                    })
                })?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buf[..n])
                    .map_err(|e| Error::Io(IoError::Raw(e)))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_then_parse_round_trip() {
        let entries = vec![
            HashEntry {
                sha256: "a".repeat(64),
                path: "manifest.json".into(),
            },
            HashEntry {
                sha256: "b".repeat(64),
                path: "rankings/ranking.json".into(),
            },
        ];
        let body = format_sha256sums(&entries);
        let parsed = parse_sha256sums(&body).unwrap();
        assert_eq!(parsed, entries);
    }

    #[test]
    fn parse_tolerates_crlf_and_extra_whitespace() {
        let body = format!(
            "{}  a.json\r\n{}    b.json\r\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let parsed = parse_sha256sums(&body).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, "a.json");
        assert_eq!(parsed[1].path, "b.json");
    }

    #[test]
    fn parse_rejects_missing_path() {
        let body = format!("{}\n", "a".repeat(64));
        let err = parse_sha256sums(&body).unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn sha256_file_matches_known_value() {
        // The hash of an empty file.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_file_matches_known_hello_world() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hello.txt");
        std::fs::write(&path, b"hello world\n").unwrap();
        // Canonical sha256sum of "hello world\n".
        let digest = sha256_file(&path).unwrap();
        assert_eq!(
            digest,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    /// `collect_files` walks the staging directory and emits every
    /// file with a relative path. We use a hand-rolled fake
    /// `RunDir` via the public helpers on `MoaganHome` to build the
    /// scenario.
    #[test]
    fn collect_files_summary_picks_expected_paths() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = crate::fs_layout::MoaganHome::resolve().unwrap();
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        std::fs::write(run_dir.manifest(), b"{}").unwrap();
        std::fs::write(run_dir.brief(), b"{}").unwrap();
        std::fs::write(run_dir.rankings().join("ranking.json"), b"{}").unwrap();
        std::fs::write(run_dir.proposals().join("p_1.json"), b"{}").unwrap();
        std::fs::create_dir_all(run_dir.telemetry()).unwrap();
        std::fs::write(run_dir.telemetry().join("calls.jsonl.gz"), b"gz").unwrap();

        let summary = collect_files(&run_dir, ExportLevel::Summary).unwrap();
        let summary_paths: Vec<&str> = summary.iter().map(|(_, p)| p.as_str()).collect();
        assert!(summary_paths.contains(&"manifest.json"));
        assert!(summary_paths.contains(&"brief.json"));
        assert!(summary_paths.contains(&"rankings/ranking.json"));
        assert!(summary_paths.contains(&"proposals/p_1.json"));
        // Summary must NOT include the gzip telemetry stream.
        assert!(!summary_paths.contains(&"telemetry/calls.jsonl.gz"));

        let full = collect_files(&run_dir, ExportLevel::Full).unwrap();
        let full_paths: Vec<&str> = full.iter().map(|(_, p)| p.as_str()).collect();
        assert!(full_paths.contains(&"telemetry/calls.jsonl.gz"));
    }
}
