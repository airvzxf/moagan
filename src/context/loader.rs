//! Load the on-disk contents behind a `ContextRef` into a
//! `LoadedContext`. The intake phase prepends the `brief_excerpt`
//! to the user prompt and writes the full `context_refs` to the
//! SQLite `run_context_refs` table so the lineage is queryable.
//!
//! `Scope` controls how much we read:
//! - `Summary`: only `final/*.md` (the markdown summary produced by
//!   the deliver phase). Cheap, intended as a one-paragraph hint.
//! - `SummaryFull`: `final/*.md` plus the `sketches/` JSONs
//!   (each rendered to a flat string).
//! - `Full`: every text-like file under the run dir, capped at 4 MiB
//!   to avoid pathological inputs. The cap is enforced **per file**,
//!   not as a global aggregate, so a 100 MiB context with many
//!   small files still loads (each is hashed but only the first
//!   4 MiB feeds into `brief_excerpt`).
//!
//! SHA-256 over the canonical concatenation of the loaded texts is
//! the `shared_brief_hash` we attach to the new run; the loader
//! uses BLAKE3 for the per-file `shasum` column because BLAKE3 is
//! the day-to-day internal hash (catalog 10-integrada-v0 §D.6.1).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::{RunId, blake3_hex};

use super::resolver::ContextRef;

/// How much of a context reference to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    /// `final/*.md` only (default).
    Summary,
    /// `final/*.md` + `sketches/` JSONs.
    SummaryFull,
    /// Every text-like file, capped at 4 MiB per file.
    Full,
}

impl ContextScope {
    /// Default scope used when the user passes `--context` without
    /// `--context-summary` or `--context-full`.
    pub const DEFAULT: Self = Self::Summary;

    /// Stable lowercase label, used in CLI flags and the SQLite
    /// `run_context_refs.context_type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::SummaryFull => "summary_full",
            Self::Full => "full",
        }
    }

    /// Parse from the `--context-{summary,full}` CLI flag.
    pub fn parse(s: &str) -> Result<Self> {
        let out = match s {
            "summary" => Ok(Self::Summary),
            "summary_full" => Ok(Self::SummaryFull),
            "full" => Ok(Self::Full),
            other => {
                tracing::warn!(input = %s, "context::loader::ContextScope::parse: unknown");
                return Err(Error::InvalidArgs(format!(
                    "unknown context scope {other:?} (expected summary | summary_full | full)"
                )));
            }
        };
        tracing::trace!(input = %s, ?out, "context::loader::ContextScope::parse");
        out
    }

    /// Human description for `moagan run --help`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Summary => "only the run's `final/*.md`",
            Self::SummaryFull => "`final/*.md` plus every sketch JSON",
            Self::Full => "every text-like file (capped at 4 MiB per file)",
        }
    }
}

/// One context reference that was loaded. Mirrored to
/// `run_context_refs` so a post-execution inspector can answer
/// "what was the brief fed into this run?".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRefRecord {
    /// Path or id, as it was classified (UUID text for `RunId`,
    /// filesystem path for paths).
    pub source_path: String,
    /// One of `"run_id" | "path" | "dir"`. Mirrors `ContextRef::kind()`.
    pub context_type: String,
    /// BLAKE3 hex digest of the on-disk bytes. For directories the
    /// hash is the concatenation of per-file hashes in sorted order,
    /// which keeps the value deterministic across runs.
    pub shasum: String,
    /// Number of bytes that fed into the hash (sum across files for
    /// dirs; single file size for files).
    pub bytes: u64,
    /// Unix seconds when the file/dir was hashed.
    pub added_unix: i64,
}

/// The result of loading a context reference. The `intake` phase
/// prepends `brief_excerpt` to the user prompt; the manifest
/// persists `parent_run_id` + `shared_brief_hash` + `context_refs`
/// + `lineage_paths`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadedContext {
    /// When the context was a `RunId`, this is that run id. `None`
    /// for path-based contexts.
    pub parent_run_id: Option<RunId>,
    /// SHA-256 hex of the canonical concatenation of every loaded
    /// text. Empty when no texts were loaded.
    pub shared_brief_hash: Option<String>,
    /// First N chars of the joined texts (default N = 4096). This
    /// is what the intake phase prepends to the user prompt.
    pub brief_excerpt: String,
    /// Per-file hashes and byte counts. Empty when the context
    /// resolved to a run id with no artefacts (e.g. an empty run dir).
    pub context_refs: Vec<ContextRefRecord>,
}

/// Load the contents of `cref` from disk. Returns the texts in
/// the order they were discovered (sorted for determinism).
pub fn load(home: &MoaganHome, cref: &ContextRef, scope: ContextScope) -> Result<LoadedContext> {
    tracing::debug!(
        kind = cref.kind(),
        scope = %scope.as_str(),
        "context::loader::load: enter"
    );
    let result = match cref {
        ContextRef::RunId(id) => load_from_run_id(home, *id, scope),
        ContextRef::FilePath(path) => load_from_path(path, scope),
        ContextRef::DirPath(path) => load_from_path(path, scope),
    };
    match &result {
        Ok(loaded) => tracing::debug!(
            excerpt_len = loaded.brief_excerpt.len(),
            refs = loaded.context_refs.len(),
            "context::loader::load: ok"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "context::loader::load: failed"
        ),
    }
    result
}

/// Load a run's text artefacts under `<home>/.runs/<id>/`. The
/// `scope` argument picks which subdirectories are scanned:
///
/// - `Summary`: `final/*.md` only.
/// - `SummaryFull`: `final/*.md` plus `sketches/*.json`.
/// - `Full`: every file under the run dir, capped at 4 MiB per file
///   (each text-like file hashed; non-text extensions are skipped).
///
/// The `parent_run_id` of the returned `LoadedContext` is the run id
/// we just loaded.
pub fn load_from_run_id(
    home: &MoaganHome,
    run_id: RunId,
    scope: ContextScope,
) -> Result<LoadedContext> {
    tracing::debug!(
        run_id = %run_id,
        scope = %scope.as_str(),
        "context::loader::load_from_run_id: enter"
    );
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().exists() {
        tracing::warn!(
            run_id = %run_id,
            "context::loader::load_from_run_id: run dir missing"
        );
        return Err(Error::InvalidArgs(format!(
            "context run id {run_id} not found under {}",
            home.runs_dir().display()
        )));
    }
    let mut candidate_dirs: Vec<PathBuf> = Vec::new();
    match scope {
        ContextScope::Summary => {
            candidate_dirs.push(run_dir.final_dir());
        }
        ContextScope::SummaryFull => {
            candidate_dirs.push(run_dir.final_dir());
            candidate_dirs.push(run_dir.sketches());
        }
        ContextScope::Full => {
            candidate_dirs.push(run_dir.root().to_path_buf());
        }
    }
    let mut texts: Vec<String> = Vec::new();
    let mut records: Vec<ContextRefRecord> = Vec::new();
    let now = crate::time::now_unix_secs();
    let mut scanned = 0usize;
    for dir in &candidate_dirs {
        if !dir.is_dir() {
            continue;
        }
        let before_texts = texts.len();
        collect_text_files(dir, scope, &mut texts, &mut records, now)?;
        scanned += texts.len() - before_texts;
    }
    tracing::trace!(
        run_id = %run_id,
        scanned,
        scope = %scope.as_str(),
        "context::loader::load_from_run_id: collected"
    );
    Ok(finalise_loaded(Some(run_id), texts, records))
}

/// Load a single `.md` file or walk a directory for every `.md`
/// file. The `scope` argument is currently a no-op for files (a
/// single file is always loaded as-is) and only filters
/// extensions when walking a directory (always `.md` for paths).
pub fn load_from_path(path: &Path, scope: ContextScope) -> Result<LoadedContext> {
    tracing::debug!(
        path = %path.display(),
        scope = %scope.as_str(),
        "context::loader::load_from_path: enter"
    );
    let meta = fs::metadata(path).map_err(Error::from)?;
    let mut texts: Vec<String> = Vec::new();
    let mut records: Vec<ContextRefRecord> = Vec::new();
    let now = crate::time::now_unix_secs();
    if meta.is_file() {
        ingest_file(path, &mut texts, &mut records, now, MAX_FILE_BYTES, "path")?;
    } else if meta.is_dir() {
        // Paths always use `.md` extension; the user pointed at
        // a folder of markdown notes.
        let _ = scope; // documented unused; surfaces the param
        walk_dir(path, &["md"], &mut texts, &mut records, now, "dir")?;
    } else {
        tracing::warn!(
            path = %path.display(),
            "context::loader::load_from_path: neither file nor directory"
        );
        return Err(Error::InvalidArgs(format!(
            "context path {path:?} is neither a file nor a directory"
        )));
    }
    Ok(finalise_loaded(None, texts, records))
}

/// Hard cap on a single file's bytes (4 MiB). Anything larger is
/// truncated at this point: the LLM never sees the full blob, but
/// the operator still gets the canonical hash for audit. The cap
/// applies per file so a directory of 1000 × 1 MiB files still
/// loads in full.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn collect_text_files(
    dir: &Path,
    scope: ContextScope,
    texts: &mut Vec<String>,
    records: &mut Vec<ContextRefRecord>,
    now_unix: i64,
) -> Result<()> {
    // `final/*.md` is the canonical surface for every mode;
    // `sketches/*.json` is the per-artefact source for
    // SummaryFull. For Full we accept every file under the run dir.
    let extensions: &[&str] = match scope {
        ContextScope::Summary => &["md"],
        ContextScope::SummaryFull => &["md", "json"],
        ContextScope::Full => &["md", "json", "txt"],
    };
    walk_dir(dir, extensions, texts, records, now_unix, "path")
}

fn walk_dir(
    dir: &Path,
    extensions: &[&str],
    texts: &mut Vec<String>,
    records: &mut Vec<ContextRefRecord>,
    now_unix: i64,
    context_type: &'static str,
) -> Result<()> {
    tracing::trace!(dir = %dir.display(), ?extensions, context_type, "context::loader::walk_dir: enter");
    let mut entries: Vec<PathBuf> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .map(|ext| extensions.contains(&ext))
                .unwrap_or(false)
        })
        .collect();
    // Sorted order so re-runs produce identical `shared_brief_hash`.
    entries.sort();
    let before = texts.len();
    for path in entries {
        ingest_file(
            &path,
            texts,
            records,
            now_unix,
            MAX_FILE_BYTES,
            context_type,
        )?;
    }
    tracing::trace!(
        dir = %dir.display(),
        ingested = texts.len() - before,
        "context::loader::walk_dir: exit"
    );
    Ok(())
}

fn ingest_file(
    path: &Path,
    texts: &mut Vec<String>,
    records: &mut Vec<ContextRefRecord>,
    now_unix: i64,
    cap: u64,
    context_type: &'static str,
) -> Result<()> {
    tracing::trace!(path = %path.display(), cap, context_type, "context::loader::ingest_file: enter");
    let bytes = fs::read(path).map_err(Error::from)?;
    let bytes_len = bytes.len() as u64;
    let truncated = bytes_len > cap;
    let (used_bytes, text) = if truncated {
        let used = &bytes[..cap as usize];
        let text = String::from_utf8_lossy(used).into_owned();
        (cap, text)
    } else {
        (bytes_len, String::from_utf8_lossy(&bytes).into_owned())
    };
    let shasum = blake3_hex(&bytes[..used_bytes as usize]);
    texts.push(text);
    records.push(ContextRefRecord {
        source_path: path.display().to_string(),
        context_type: context_type.into(),
        shasum,
        bytes: used_bytes,
        added_unix: now_unix,
    });
    tracing::trace!(
        path = %path.display(),
        bytes = used_bytes,
        truncated,
        "context::loader::ingest_file: ingested"
    );
    Ok(())
}

fn finalise_loaded(
    parent: Option<RunId>,
    mut texts: Vec<String>,
    mut records: Vec<ContextRefRecord>,
) -> LoadedContext {
    tracing::trace!(
        text_count = texts.len(),
        record_count = records.len(),
        has_parent = parent.is_some(),
        "context::loader::finalise_loaded: enter"
    );
    if texts.is_empty() {
        return LoadedContext {
            parent_run_id: parent,
            shared_brief_hash: None,
            brief_excerpt: String::new(),
            context_refs: records,
        };
    }
    texts.sort();
    let shared_brief_hash = Some(compute_shared_brief_hash(&texts));
    let brief_excerpt = brief_excerpt(&texts, BRIEF_EXCERPT_MAX_CHARS);
    // Records keep file order from the walk; no need to re-sort.
    records.sort_by(|a, b| a.source_path.cmp(&b.source_path));
    // Stamp the parent_run_id as the `context_type` for RunId refs.
    if let Some(pid) = parent {
        records.insert(
            0,
            ContextRefRecord {
                source_path: pid.to_string(),
                context_type: "run_id".into(),
                shasum: shared_brief_hash.clone().unwrap_or_default(),
                bytes: texts.iter().map(|t| t.len() as u64).sum(),
                added_unix: crate::time::now_unix_secs(),
            },
        );
    }
    tracing::trace!(
        excerpt_len = brief_excerpt.len(),
        refs = records.len(),
        "context::loader::finalise_loaded: ok"
    );
    LoadedContext {
        parent_run_id: parent,
        shared_brief_hash,
        brief_excerpt,
        context_refs: records,
    }
}

/// Max chars of the joined texts surfaced as `brief_excerpt`.
/// 4096 is the budget the intake LLM call can absorb without
/// bloating the request past a single round-trip for a 8k-token
/// context model.
pub const BRIEF_EXCERPT_MAX_CHARS: usize = 4096;

/// Compute the SHA-256 over the canonical concatenation of `texts`.
/// Texts are joined with `\x1f` (the ASCII unit separator) so the
/// concatenation is unambiguous even when individual texts end with
/// a newline. The hex output is the `shared_brief_hash` we attach
/// to `manifest.json` and the SQLite `runs.shared_brief_hash` column.
pub fn compute_shared_brief_hash(texts: &[String]) -> String {
    let mut hasher = Sha256::new();
    for (i, t) in texts.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\x1f");
        }
        hasher.update(t.as_bytes());
    }
    let out = hex::encode(hasher.finalize());
    tracing::trace!(
        input_texts = texts.len(),
        "context::loader::compute_shared_brief_hash"
    );
    out
}

/// Build the `brief_excerpt` — the first `max_chars` of the joined
/// texts, with a trailing `…` when truncated. Safe with multi-byte
/// UTF-8: walks to the next char boundary before slicing.
pub fn brief_excerpt(texts: &[String], max_chars: usize) -> String {
    tracing::trace!(
        text_count = texts.len(),
        max_chars,
        "context::loader::brief_excerpt"
    );
    let joined = texts.join("\n\n");
    if joined.chars().count() <= max_chars {
        return joined;
    }
    let mut out = String::with_capacity(max_chars + 4);
    for (i, ch) in joined.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Empty input → empty hash.
    #[test]
    fn compute_shared_brief_hash_deterministic() {
        let a = compute_shared_brief_hash(&["x".into(), "y".into()]);
        let b = compute_shared_brief_hash(&["x".into(), "y".into()]);
        assert_eq!(a, b);
        // 64 hex chars = SHA-256.
        assert_eq!(a.len(), 64);
    }

    /// A change in the input changes the hash.
    #[test]
    fn compute_shared_brief_hash_changes_with_input() {
        let a = compute_shared_brief_hash(&["x".into(), "y".into()]);
        let b = compute_shared_brief_hash(&["x".into(), "z".into()]);
        assert_ne!(a, b);
    }

    /// `brief_excerpt` truncates with an ellipsis when the input is
    /// longer than `max_chars`.
    #[test]
    fn brief_excerpt_truncates_with_ellipsis() {
        let s = "abcdefghij".to_string();
        let out = brief_excerpt(&[s], 5);
        assert_eq!(out, "abcde…");
    }

    /// Short input round-trips verbatim.
    #[test]
    fn brief_excerpt_short_input_unchanged() {
        let out = brief_excerpt(&["abc".into()], 100);
        assert_eq!(out, "abc");
    }

    /// `Summary` scope reads `final/*.md` and only that.
    #[test]
    fn load_from_run_id_summary_reads_final_md() {
        crate::test_support::with_moagan_home("load_from_run_id_summary_reads_final_md", |_home| {
            let home = MoaganHome::resolve().unwrap();
            home.ensure().unwrap();
            let id = RunId::new();
            let run_dir = home.run_dir(id);
            run_dir.ensure().unwrap();
            std::fs::create_dir_all(run_dir.final_dir()).unwrap();
            let mut f = std::fs::File::create(run_dir.final_dir().join("portfolio.md")).unwrap();
            writeln!(f, "# portfolio").unwrap();
            // A sketch file must NOT be picked up under Summary scope.
            std::fs::create_dir_all(run_dir.sketches()).unwrap();
            let mut g = std::fs::File::create(run_dir.sketches().join("sk_001.json")).unwrap();
            writeln!(g, "{{}}").unwrap();

            let loaded = load_from_run_id(&home, id, ContextScope::Summary).unwrap();
            assert_eq!(loaded.parent_run_id, Some(id));
            // 1 parent_run_id stamp + 1 file = 2.
            assert_eq!(loaded.context_refs.len(), 2, "{:?}", loaded.context_refs);
            let file_record = loaded
                .context_refs
                .iter()
                .find(|r| r.source_path.ends_with("portfolio.md"))
                .expect("portfolio.md record missing");
            assert_eq!(file_record.context_type, "path");
            assert!(loaded.brief_excerpt.contains("portfolio"));
            assert!(!loaded.brief_excerpt.contains("sk_001"));
            assert!(loaded.shared_brief_hash.is_some());
        });
    }

    /// `SummaryFull` scope also reads the sketch JSONs.
    #[test]
    fn load_from_run_id_summary_full_reads_sketches() {
        crate::test_support::with_moagan_home(
            "load_from_run_id_summary_full_reads_sketches",
            |_home| {
                let home = MoaganHome::resolve().unwrap();
                home.ensure().unwrap();
                let id = RunId::new();
                let run_dir = home.run_dir(id);
                run_dir.ensure().unwrap();
                std::fs::create_dir_all(run_dir.final_dir()).unwrap();
                std::fs::write(run_dir.final_dir().join("portfolio.md"), "# p").unwrap();
                std::fs::create_dir_all(run_dir.sketches()).unwrap();
                std::fs::write(
                    run_dir.sketches().join("sk_001.json"),
                    "{\"id\":\"sk_001\"}",
                )
                .unwrap();
                std::fs::write(
                    run_dir.sketches().join("sk_002.json"),
                    "{\"id\":\"sk_002\"}",
                )
                .unwrap();

                let loaded = load_from_run_id(&home, id, ContextScope::SummaryFull).unwrap();
                // 1 final + 2 sketches + 1 parent_run_id stamp = 4.
                assert!(loaded.context_refs.len() >= 3, "{:?}", loaded.context_refs);
                assert!(loaded.brief_excerpt.contains("sk_001"));
                assert!(loaded.brief_excerpt.contains("sk_002"));
            },
        );
    }

    /// Loading a single `.md` file returns one record and the
    /// file's text appears in `brief_excerpt`.
    #[test]
    fn load_from_path_md() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notes.md");
        std::fs::write(&path, "# Notes\n\nSome content here.").unwrap();
        let loaded = load_from_path(&path, ContextScope::Summary).unwrap();
        assert!(loaded.parent_run_id.is_none());
        assert_eq!(loaded.context_refs.len(), 1);
        assert!(loaded.brief_excerpt.contains("Notes"));
        assert!(loaded.shared_brief_hash.is_some());
    }

    /// Loading a directory walks every `.md` file.
    #[test]
    fn load_from_dir_recurses() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("ctx").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.md"), "alpha").unwrap();
        std::fs::write(sub.join("b.md"), "beta").unwrap();
        // A non-md file is ignored.
        std::fs::write(sub.join("ignored.txt"), "ignore me").unwrap();
        let loaded = load_from_path(&tmp.path().join("ctx"), ContextScope::Summary).unwrap();
        assert_eq!(loaded.context_refs.len(), 2, "{:?}", loaded.context_refs);
        assert!(loaded.brief_excerpt.contains("alpha"));
        assert!(loaded.brief_excerpt.contains("beta"));
        assert!(!loaded.brief_excerpt.contains("ignore me"));
    }

    /// Loading an empty directory returns an empty `LoadedContext`
    /// (no `shared_brief_hash`, empty excerpt, no records).
    #[test]
    fn load_from_empty_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("empty");
        std::fs::create_dir_all(&dir).unwrap();
        let loaded = load_from_path(&dir, ContextScope::Summary).unwrap();
        assert!(loaded.brief_excerpt.is_empty());
        assert!(loaded.shared_brief_hash.is_none());
        assert!(loaded.context_refs.is_empty());
    }

    /// `ContextScope::parse` accepts the three documented values and
    /// rejects anything else.
    #[test]
    fn context_scope_parse_round_trip() {
        assert_eq!(
            ContextScope::parse("summary").unwrap(),
            ContextScope::Summary
        );
        assert_eq!(
            ContextScope::parse("summary_full").unwrap(),
            ContextScope::SummaryFull
        );
        assert_eq!(ContextScope::parse("full").unwrap(), ContextScope::Full);
        assert!(ContextScope::parse("nope").is_err());
    }

    /// `ContextRefRecord` round-trips through JSON so the SQLite
    /// mirror (`run_context_refs`) can parse records back.
    #[test]
    fn context_ref_record_round_trips() {
        let r = ContextRefRecord {
            source_path: "/tmp/x.md".into(),
            context_type: "path".into(),
            shasum: "deadbeef".into(),
            bytes: 42,
            added_unix: 1_700_000_000,
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: ContextRefRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }
}
