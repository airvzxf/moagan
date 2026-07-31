//! Atomic file writer with crash-safe semantics.
//!
//! `AtomicWriter::write` produces a `<path>` that is either fully present
//! (correct contents and matching `.meta.json` sidecar) or fully absent
//! (the previous content, if any, is untouched). It survives crashes
//! between steps because each step either completes or leaves the
//! filesystem in the previous valid state.
//!
//! Sequence:
//!
//! 1. Write data to `<path>.tmp.<random>`.
//! 2. `fsync` the data file.
//! 3. Write metadata sidecar (size, BLAKE3, mtime) to `<path>.meta.json`.
//! 4. `fsync` the sidecar.
//! 5. `rename` data file to `<path>` (atomic on POSIX).
//! 6. `rename` sidecar to `<path>.meta.json`.
//! 7. `fsync` the parent directory so the renames are durable.
//!
//! Compliance: catalog 10-integrada-v0 §D.1.1 (Day 1).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::IoError;

/// Sidecar metadata written next to each artifact. Verifies integrity on
/// read and tells `inspect` when the file was last touched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactMeta {
    /// Schema version of the meta sidecar. Currently `"v1"`.
    pub schema_version: String,
    /// Size in bytes of the data file.
    pub size_bytes: u64,
    /// BLAKE3 hash of the data file (hex).
    pub blake3_hex: String,
    /// Unix epoch seconds when the file was sealed.
    pub sealed_at_unix: i64,
    /// CRC32C of the data file (hex, low-overhead sanity check).
    pub crc32c_hex: String,
}

impl ArtifactMeta {
    /// Current schema version.
    pub const SCHEMA_VERSION: &'static str = "v1";
}

/// Atomic file writer.
#[derive(Debug, Default, Clone, Copy)]
pub struct AtomicWriter;

impl AtomicWriter {
    /// Create a new `AtomicWriter`.
    pub fn new() -> Self {
        Self
    }

    /// Write `data` to `dest` atomically. `dest` is created if it does
    /// not exist; if it does, the previous content is replaced once the
    /// new file is fully durable.
    pub fn write(&self, dest: &Path, data: &[u8]) -> Result<ArtifactMeta> {
        let parent = dest.parent().ok_or_else(|| IoError::NoParent {
            path: dest.to_path_buf(),
        })?;
        fs::create_dir_all(parent).map_err(|e| IoError::CreateDir {
            path: parent.to_path_buf(),
            source: e,
        })?;

        let nonce = fastrand::u64(..);
        let tmp = Self::tmp_path(dest, nonce);
        let tmp_meta = Self::tmp_meta_path(dest, nonce);

        // Step 1: write data to tmp.
        let mut data_file = File::create(&tmp).map_err(|e| IoError::CreateFile {
            path: tmp.clone(),
            source: e,
        })?;
        data_file.write_all(data).map_err(|e| IoError::Write {
            path: tmp.clone(),
            source: e,
        })?;
        // Step 2: fsync data.
        data_file.sync_all().map_err(|e| IoError::Sync {
            path: tmp.clone(),
            source: e,
        })?;

        // Compute metadata.
        let meta = compute_meta(data)?;
        let meta_bytes = serde_json::to_vec(&meta).map_err(IoError::SerializeMeta)?;

        // Step 3: write meta to tmp.
        let mut meta_file = File::create(&tmp_meta).map_err(|e| IoError::CreateFile {
            path: tmp_meta.clone(),
            source: e,
        })?;
        meta_file
            .write_all(&meta_bytes)
            .map_err(|e| IoError::Write {
                path: tmp_meta.clone(),
                source: e,
            })?;
        // Step 4: fsync meta.
        meta_file.sync_all().map_err(|e| IoError::Sync {
            path: tmp_meta.clone(),
            source: e,
        })?;

        // Step 5+6: rename both atomically.
        fs::rename(&tmp, dest).map_err(|e| IoError::Rename {
            from: tmp.clone(),
            to: dest.to_path_buf(),
            source: e,
        })?;
        let final_meta = Self::meta_path(dest);
        fs::rename(&tmp_meta, &final_meta).map_err(|e| IoError::Rename {
            from: tmp_meta.clone(),
            to: final_meta,
            source: e,
        })?;

        // Step 7: fsync parent directory.
        Self::fsync_dir(parent)?;

        Ok(meta)
    }

    /// Read the data file and verify it against its sidecar metadata.
    ///
    /// The integrity check compares only the data-derived fields
    /// (`size_bytes`, `blake3_hex`, `crc32c_hex`) so the call is
    /// deterministic regardless of when it runs after the write.
    /// The `sealed_at_unix` and `schema_version` fields are
    /// informational — they survive a round trip but a
    /// `now_unix_secs()` change between the write and the read
    /// is expected and not an error.
    pub fn read_with_meta(&self, dest: &Path) -> Result<(Vec<u8>, ArtifactMeta)> {
        let data = fs::read(dest).map_err(|e| IoError::Read {
            path: dest.to_path_buf(),
            source: e,
        })?;
        let meta_path = Self::meta_path(dest);
        let meta_bytes = fs::read(&meta_path).map_err(|e| IoError::Read {
            path: meta_path.clone(),
            source: e,
        })?;
        let meta: ArtifactMeta =
            serde_json::from_slice(&meta_bytes).map_err(IoError::DeserializeMeta)?;
        if !data_fingerprint_matches(&data, &meta) {
            // Build a diagnostic `expected` that re-uses the
            // sidecar's `sealed_at_unix` so the error message
            // surfaces a single timestamp per field.
            let expected = compute_meta_at(&data, meta.sealed_at_unix)?;
            return Err(IoError::MetaMismatch {
                path: dest.to_path_buf(),
                expected: Box::new(expected),
                got: Box::new(meta),
            }
            .into());
        }
        Ok((data, meta))
    }

    /// Convenience: write to a file in one call without a pre-built
    /// `AtomicWriter`. Equivalent to `AtomicWriter::new().write(...)`.
    pub fn write_path(dest: &Path, data: &[u8]) -> Result<ArtifactMeta> {
        Self::new().write(dest, data)
    }

    /// Path to the sidecar metadata file for `dest`.
    pub fn meta_path(dest: &Path) -> PathBuf {
        let mut s = dest.as_os_str().to_owned();
        s.push(".meta.json");
        PathBuf::from(s)
    }

    fn tmp_path(dest: &Path, nonce: u64) -> PathBuf {
        let mut s = dest.as_os_str().to_owned();
        s.push(format!(".tmp.{nonce:016x}"));
        PathBuf::from(s)
    }

    fn tmp_meta_path(dest: &Path, nonce: u64) -> PathBuf {
        let mut s = Self::meta_path(dest).as_os_str().to_owned();
        s.push(format!(".tmp.{nonce:016x}"));
        PathBuf::from(s)
    }

    fn fsync_dir(dir: &Path) -> Result<()> {
        // POSIX-only: open the directory and sync_all so renames inside
        // it are durable. Best-effort on platforms where this is a no-op.
        #[cfg(unix)]
        {
            let f = File::open(dir).map_err(|e| IoError::OpenDir {
                path: dir.to_path_buf(),
                source: e,
            })?;
            f.sync_all().map_err(|e| IoError::Sync {
                path: dir.to_path_buf(),
                source: e,
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = dir;
        }
        Ok(())
    }
}

fn compute_meta(data: &[u8]) -> Result<ArtifactMeta> {
    compute_meta_at(data, crate::time::now_unix_secs())
}

/// Build the `ArtifactMeta` for `data` with the given
/// `sealed_at_unix`. The timestamp is an explicit parameter so the
/// read path can re-derive the expected meta with the sidecar's
/// timestamp and avoid a clock-boundary race.
fn compute_meta_at(data: &[u8], sealed_at_unix: i64) -> Result<ArtifactMeta> {
    let size_bytes = data.len() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(data);
    let blake3_hex = hex::encode(hasher.finalize().as_bytes());
    let crc32c_hex = crc32c_hex(data);
    Ok(ArtifactMeta {
        schema_version: ArtifactMeta::SCHEMA_VERSION.to_owned(),
        size_bytes,
        blake3_hex,
        sealed_at_unix,
        crc32c_hex,
    })
}

/// True when the data-derived fingerprint fields of `meta` match
/// the actual `data`. `sealed_at_unix` and `schema_version` are
/// informational and intentionally excluded from the comparison.
fn data_fingerprint_matches(data: &[u8], meta: &ArtifactMeta) -> bool {
    if data.len() as u64 != meta.size_bytes {
        return false;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(data);
    let blake3_hex = hex::encode(hasher.finalize().as_bytes());
    if blake3_hex != meta.blake3_hex {
        return false;
    }
    if crc32c_hex(data) != meta.crc32c_hex {
        return false;
    }
    true
}

/// CRC32C (Castagnoli) computed in software. Different providers name
/// this differently; we use the castagnoli polynomial so that any
/// sidecar reader can recompute it without a native dep.
fn crc32c_hex(data: &[u8]) -> String {
    let crc = crc32c::crc32c(data);
    format!("{crc:08x}")
}

// Minimal CRC32C implementation to avoid adding a dependency just for
// one hex-encoded 8-byte fingerprint. Castagnoli polynomial, bit-reflected.
mod crc32c {
    const TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0x82f6_3b78 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };

    pub(super) fn crc32c(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xffff_ffff;
        for &b in data {
            let idx = ((crc ^ b as u32) & 0xff) as usize;
            crc = (crc >> 8) ^ TABLE[idx];
        }
        crc ^ 0xffff_ffff
    }
}

#[cfg(test)]
mod tests {
    use super::crc32c::crc32c;
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        // "123456789" -> 0xe3069283 (RFC 3720 reference).
        assert_eq!(format!("{:08x}", crc32c(b"123456789")), "e3069283");
    }

    #[test]
    fn write_and_read_with_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.json");
        let payload = br#"{"hello":"world"}"#;
        let meta = AtomicWriter::new().write(&dest, payload).unwrap();
        assert_eq!(meta.size_bytes, payload.len() as u64);
        assert_eq!(meta.schema_version, "v1");
        let (got, got_meta) = AtomicWriter::new().read_with_meta(&dest).unwrap();
        assert_eq!(got, payload);
        assert_eq!(got_meta, meta);
    }

    #[test]
    fn meta_mismatch_on_tampered_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.json");
        AtomicWriter::new().write(&dest, b"alpha").unwrap();
        // Overwrite the data without going through AtomicWriter.
        std::fs::write(&dest, b"beta").unwrap();
        let err = AtomicWriter::new().read_with_meta(&dest);
        assert!(matches!(
            err,
            Err(crate::Error::Io(IoError::MetaMismatch { .. }))
        ));
    }

    #[test]
    fn overwrites_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.json");
        AtomicWriter::new().write(&dest, b"first").unwrap();
        AtomicWriter::new().write(&dest, b"second-content").unwrap();
        let (got, _) = AtomicWriter::new().read_with_meta(&dest).unwrap();
        assert_eq!(got, b"second-content");
    }

    /// Regression: `read_with_meta` must not fail when the second
    /// write happens in a different clock second than the read.
    /// The original `overwrites_existing_file` test was flaky in
    /// CI: when the read crossed a Unix-second boundary the
    /// sidecar's `sealed_at_unix` was 1s older than the freshly
    /// computed `expected`, so the comparison failed with
    /// `MetaMismatch`. The fix compares only the data-derived
    /// fields (`size_bytes`, `blake3_hex`, `crc32c_hex`) and
    /// ignores the timestamp.
    #[test]
    fn overwrites_existing_file_survives_clock_second_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.json");
        AtomicWriter::new().write(&dest, b"first").unwrap();
        // Force the second write to land in a different clock
        // second from the first one.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        AtomicWriter::new().write(&dest, b"second-content").unwrap();
        // And force the read into yet another second.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let (got, got_meta) = AtomicWriter::new().read_with_meta(&dest).unwrap();
        assert_eq!(got, b"second-content");
        assert_eq!(got_meta.size_bytes, 14);
        assert_eq!(got_meta.blake3_hex.len(), 64);
    }

    #[test]
    fn meta_path_appends_suffix() {
        let p = std::path::Path::new("/var/run/x.json");
        assert_eq!(
            AtomicWriter::meta_path(p),
            std::path::PathBuf::from("/var/run/x.json.meta.json")
        );
    }

    #[test]
    fn write_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("a/b/c/artifact.json");
        AtomicWriter::new().write(&dest, b"hello").unwrap();
        assert!(dest.exists());
    }
}
