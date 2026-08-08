//! `moagan telemetry verify` — re-hash the files in an exported
//! bundle against the embedded SHA256SUMS manifest.
//!
//! Mirrors `proposal-02-rust.md §10.10` + the `sha256sum -c`
//! contract: every line of the manifest names a relative path and
//! its expected digest; the verifier hashes the file again and
//! emits a row per entry with the verdict (OK / MISSING / MISMATCH).
//!
//! Supports both flat directories (the canonical case after
//! `tar -xf <archive>`) and the raw archive path (we extract into
//! a temporary directory first so the SHA256SUMS lines match the
//! relative paths the producer wrote).

use std::fs::File;
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use sha2::{Digest, Sha256};

use crate::error::{Error, IoError, Result};
use crate::telemetry::export::{HashEntry, parse_sha256sums, sha256_file};

/// Verdict for a single manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyVerdict {
    /// Digest matches the on-disk file.
    Ok,
    /// The manifest named a file that does not exist on disk.
    Missing,
    /// The digest does not match the on-disk file.
    Mismatch {
        /// Expected digest from the manifest.
        expected: String,
        /// Computed digest from the re-hash.
        actual: String,
    },
}

impl VerifyVerdict {
    /// Short label suitable for columnar output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Missing => "MISSING",
            Self::Mismatch { .. } => "MISMATCH",
        }
    }
}

/// One row of the verification report.
#[derive(Debug, Clone)]
pub struct VerifyRow {
    /// Path from the manifest.
    pub path: String,
    /// Verdict.
    pub verdict: VerifyVerdict,
}

/// Aggregated verification outcome.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// One row per manifest entry.
    pub rows: Vec<VerifyRow>,
    /// Path to the directory that was verified (post-extraction if
    /// the input was an archive).
    pub root: PathBuf,
}

impl VerifyReport {
    /// Number of files that passed.
    pub fn ok_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.verdict, VerifyVerdict::Ok))
            .count()
    }
    /// Number of files that failed.
    pub fn fail_count(&self) -> usize {
        self.rows.len() - self.ok_count()
    }
}

/// Verify the bundle at `path`. When `path` is a directory it is
/// verified in place; when it is an archive (`tar.gz`, `tar`,
/// `zip`) it is extracted into a temporary directory first.
pub fn verify(path: &Path) -> Result<VerifyReport> {
    if !path.exists() {
        return Err(Error::InvalidArgs(format!(
            "verify path not found: {}",
            path.display()
        )));
    }
    let staging_root = tempfile::tempdir()?;
    let verify_dir = match path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("zip") => {
            let dir = staging_root.path().join("verify");
            std::fs::create_dir_all(&dir)?;
            extract_zip(path, &dir)?;
            dir
        }
        Some("tar") => {
            let dir = staging_root.path().join("verify");
            std::fs::create_dir_all(&dir)?;
            extract_tar(path, &dir)?;
            dir
        }
        Some("gz") => {
            let dir = staging_root.path().join("verify");
            std::fs::create_dir_all(&dir)?;
            extract_tar_gz(path, &dir)?;
            dir
        }
        _ => path.to_path_buf(),
    };

    let sums_path = verify_dir.join("SHA256SUMS");
    if !sums_path.exists() {
        return Err(Error::InvalidState(format!(
            "no SHA256SUMS at {}; cannot verify",
            sums_path.display()
        )));
    }
    let body = std::fs::read_to_string(&sums_path).map_err(|e| {
        Error::Io(IoError::Read {
            path: sums_path.clone(),
            source: e,
        })
    })?;
    let entries = parse_sha256sums(&body)?;
    let rows = check_entries(&entries, &verify_dir)?;
    Ok(VerifyReport {
        rows,
        root: verify_dir,
    })
}

fn check_entries(entries: &[HashEntry], root: &Path) -> Result<Vec<VerifyRow>> {
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = root.join(&entry.path);
        let verdict = if !path.exists() {
            VerifyVerdict::Missing
        } else {
            let actual = sha256_file(&path)?;
            if actual == entry.sha256 {
                VerifyVerdict::Ok
            } else {
                VerifyVerdict::Mismatch {
                    expected: entry.sha256.clone(),
                    actual,
                }
            }
        };
        rows.push(VerifyRow {
            path: entry.path.clone(),
            verdict,
        });
    }
    Ok(rows)
}

fn extract_zip(archive: &Path, out: &Path) -> Result<()> {
    let f = File::open(archive).map_err(|e| {
        Error::Io(IoError::Read {
            path: archive.to_path_buf(),
            source: e,
        })
    })?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| Error::Io(IoError::Raw(zip_err(&e))))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::Io(IoError::Raw(zip_err(&e))))?;
        let Some(name) = entry.enclosed_name().map(|n| n.to_path_buf()) else {
            continue;
        };
        let dest = out.join(&name);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| {
                Error::Io(IoError::CreateDir {
                    path: dest.clone(),
                    source: e,
                })
            })?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Io(IoError::CreateDir {
                    path: parent.to_path_buf(),
                    source: e,
                })
            })?;
        }
        let mut out_file = File::create(&dest).map_err(|e| {
            Error::Io(IoError::CreateFile {
                path: dest.clone(),
                source: e,
            })
        })?;
        std::io::copy(&mut entry, &mut out_file).map_err(|e| {
            Error::Io(IoError::Write {
                path: dest.clone(),
                source: e,
            })
        })?;
    }
    Ok(())
}

fn extract_tar(archive: &Path, out: &Path) -> Result<()> {
    let f = File::open(archive).map_err(|e| {
        Error::Io(IoError::Read {
            path: archive.to_path_buf(),
            source: e,
        })
    })?;
    let mut archive = tar::Archive::new(f);
    archive
        .unpack(out)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(())
}

fn extract_tar_gz(archive: &Path, out: &Path) -> Result<()> {
    let f = File::open(archive).map_err(|e| {
        Error::Io(IoError::Read {
            path: archive.to_path_buf(),
            source: e,
        })
    })?;
    let decoder = MultiGzDecoder::new(f);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(out)
        .map_err(|e| Error::Io(IoError::Raw(e)))?;
    Ok(())
}

fn zip_err(err: &zip::result::ZipError) -> std::io::Error {
    use zip::result::ZipError as Z;
    match err {
        Z::Io(e) => std::io::Error::new(e.kind(), format!("{e}")),
        other => std::io::Error::other(format!("zip: {other}")),
    }
}

/// Re-hash a single file and return its hex SHA-256. Convenience
/// helper exposed to the rest of the crate and to tests; the
/// implementation lives in [`crate::telemetry::export`] so both
/// modules share one definition.
pub fn sha256_hex_of(path: &Path) -> Result<String> {
    sha256_file(path)
}

/// Re-hash a byte slice. Useful for tests.
#[allow(dead_code)]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_ok_matches_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, b"hello world\n").unwrap();
        let entries = vec![crate::telemetry::export::HashEntry {
            sha256: "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447".into(),
            path: "hello.txt".into(),
        }];
        let rows = check_entries(&entries, tmp.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, VerifyVerdict::Ok);
    }

    #[test]
    fn verify_mismatch_when_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, b"different content").unwrap();
        let entries = vec![crate::telemetry::export::HashEntry {
            sha256: "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447".into(),
            path: "hello.txt".into(),
        }];
        let rows = check_entries(&entries, tmp.path()).unwrap();
        assert_eq!(rows[0].verdict.label(), "MISMATCH");
        if let VerifyVerdict::Mismatch { expected, actual } = &rows[0].verdict {
            assert_eq!(
                expected,
                "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
            );
            assert_ne!(actual, expected);
        } else {
            panic!("expected Mismatch");
        }
    }

    #[test]
    fn verify_missing_returns_missing_label() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![crate::telemetry::export::HashEntry {
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            path: "absent.txt".into(),
        }];
        let rows = check_entries(&entries, tmp.path()).unwrap();
        assert_eq!(rows[0].verdict, VerifyVerdict::Missing);
    }

    #[test]
    fn verify_report_counts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), b"a").unwrap();
        std::fs::write(tmp.path().join("b"), b"b").unwrap();
        let entries = vec![
            crate::telemetry::export::HashEntry {
                sha256: sha256_hex(b"a"),
                path: "a".into(),
            },
            crate::telemetry::export::HashEntry {
                sha256: sha256_hex(b"WRONG"),
                path: "b".into(),
            },
            crate::telemetry::export::HashEntry {
                sha256: sha256_hex(b"never written"),
                path: "missing".into(),
            },
        ];
        let rows = check_entries(&entries, tmp.path()).unwrap();
        let report = VerifyReport {
            rows,
            root: tmp.path().to_path_buf(),
        };
        assert_eq!(report.ok_count(), 1);
        assert_eq!(report.fail_count(), 2);
    }

    #[test]
    fn verify_missing_path_returns_invalid_args() {
        let res = verify(Path::new("/nonexistent/path/here"));
        let err = res.unwrap_err();
        assert!(matches!(err, Error::InvalidArgs(_)));
    }

    #[test]
    fn verify_directory_without_shasums_returns_invalid_state() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("x"), b"x").unwrap();
        let res = verify(tmp.path());
        let err = res.unwrap_err();
        assert!(matches!(err, Error::InvalidState(_)));
    }

    #[test]
    fn verify_round_trip_with_export() {
        crate::test_support::with_moagan_home("verify_round_trip_with_export", |_home| {
            // End-to-end: bundle a fake run, then verify it.
            let home = crate::fs_layout::MoaganHome::resolve().unwrap();
            let run_id = crate::ids::RunId::new();
            let run_dir = home.run_dir(run_id);
            run_dir.ensure().unwrap();
            std::fs::write(run_dir.manifest(), b"{}").unwrap();
            std::fs::write(run_dir.brief(), b"{}").unwrap();
            std::fs::write(run_dir.rankings().join("ranking.json"), b"{}").unwrap();
            std::fs::write(run_dir.proposals().join("p_01.json"), b"{}").unwrap();

            // Bundle as tar.gz.
            let archive = _home.join("bundle.tar.gz");
            let _ = crate::telemetry::export::export_run(
                &run_dir,
                run_id,
                crate::cli::telemetry_cmd::ExportLevel::Summary,
                crate::cli::telemetry_cmd::ExportFormat::TarGz,
                &archive,
            )
            .unwrap();

            // Verify — should be all OK.
            let report = verify(&archive).unwrap();
            assert!(report.rows.iter().all(|r| r.verdict.label() == "OK"));
            assert!(report.ok_count() >= 4);
        });
    }
}
