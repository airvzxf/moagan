//! Self-healing param rejection: auto-detect, cache, persist.
//!
//! When the upstream returns HTTP 4xx with a body that names a
//! rejected wire parameter (e.g. `Unknown parameter: 'max_tokens'`
//! or `{"error":{"param":"temperature must be within [0, 1.5]"}}`),
//! the runtime omits that parameter on the retry, caches the
//! rejection per `(provider, model)`, and persists it to
//! `<MOAGAN_HOME>/param_rejections.toml` so future runs skip the
//! failing round-trip entirely.
//!
//! ## Why this module
//!
//! Provider APIs disagree on which wire fields they accept:
//! Anthropic-compat endpoints reject `top_p`, OpenAI-compat typically
//! accepts it, the OpenCode Go `kimi-k3` route rejects `temperature`
//! outright, and DeepSeek-direct rejects both `temperature` and
//! `top_p` outside their declared ranges. Hardcoding a global map is
//! the same brittleness the `max_tokens` / temperature auto-probes
//! remove: a relay can change its accepted set without warning and
//! the next run breaks.
//!
//! The auto-detect is intentionally conservative: 5-7 patterns cover
//! every rejection signature observed in the spike (Anthropic,
//! OpenAI-compat, OpenCode Go, DeepSeek, MiniMax Anthropic-direct).
//! Up to ~40% of upstream rejections are silent (HTTP 200 with the
//! parameter dropped) and the auto-detect does NOT catch those —
//! callers can opt into a `WARN`-level diagnostic for silent
//! acceptance via [`crate::llm::param_rejections::audit_unknown_fields`].
//!
//! ## What is cached
//!
//! Only the wire fields the runtime controls today — `temperature`,
//! `top_p`, and `max_tokens` (limited to what the dispatch path can
//! actually omit). The auto-detect does NOT extend to model-specific
//! fields like `response_format` or `tool_choice`; those go through
//! the existing capability resolver and modality gate.
//!
//! ## Threading
//!
//! `ParamRejectionsTable` is wrapped in an `Arc<RwLock<...>>` and
//! travels via `ProviderRegistry` → `RunContext` → every LLM call
//! site. Cloning the `Arc` is cheap; the lock is held only on
//! lookups and the rare persistence path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock as ParkingRwLock;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;

/// Wire field names that the auto-detect knows how to omit on the
/// retry path. `temperature`, `top_p`, and `max_tokens` are the three
/// optional fields on [`crate::llm::wire::Request`]; setting any of
/// them to `None` makes the wire builder drop the field (via
/// `#[serde(skip_serializing_if = "Option::is_none")]`).
///
/// `max_tokens` is now in scope: the field is `Option<u32>` so the
/// dispatch path can drop it without restructuring the request
/// shape. `omit_param(req, "max_tokens")` clears the field; the
/// Anthropic / OpenAI / Responses wire builders emit field-absent;
/// and upstreams that reject the *presence* of `max_tokens` (e.g.
/// `gpt-5.6-luna`) accept the retry. The auto-detect closes the
/// loop end-to-end.
pub const PARAM_NAMES: &[&str] = &["temperature", "top_p", "max_tokens"];

/// Serde shape persisted at `<MOAGAN_HOME>/param_rejections.toml`.
/// Schema version 1.
///
/// ```toml
/// schema_version = 1
///
/// [providers."opencode"."gpt-5.6-luna"]
/// rejects = ["temperature"]
///
/// [providers."opencode"."grok-4.5"]
/// rejects = ["max_tokens", "top_p"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParamRejectionsFile {
    /// Bumped on incompatible shape changes so a future binary
    /// refuses to read a stale file instead of silently
    /// misinterpreting it.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// `provider -> model -> set_of_rejected_param_names`. The
    /// nested `BTreeMap` gives deterministic on-disk ordering so a
    /// manual diff after a fresh rejection is meaningful.
    #[serde(default)]
    pub providers: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
}

fn default_schema_version() -> u32 {
    1
}

impl ParamRejectionsFile {
    /// Schema version this binary knows how to read.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Build an empty table. Useful for tests that bypass the
    /// on-disk file.
    pub fn new_empty() -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    }

    /// Read from a TOML file. Missing file is `Ok(new_empty())`;
    /// malformed file is `Err(Error::Provider(...))` so a typo in
    /// operator-land cannot silently break startup.
    pub fn load(path: &Path) -> Result<Self> {
        tracing::trace!(path = %path.display(), "ParamRejectionsFile::load");
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let parsed: Self = toml::from_str(&s).map_err(|e| {
                    tracing::warn!(error = %e, path = %path.display(), "param_rejections.toml malformed");
                    Error::Provider {
                        message: format!(
                            "param_rejections.toml at {} is malformed: {e}",
                            path.display()
                        ),
                        http_status: None,
                    }
                })?;
                if parsed.schema_version > Self::CURRENT_SCHEMA_VERSION {
                    tracing::warn!(
                        file_version = parsed.schema_version,
                        max_supported = Self::CURRENT_SCHEMA_VERSION,
                        "param_rejections.toml schema_version too new"
                    );
                    return Err(Error::Provider {
                        message: format!(
                            "param_rejections.toml at {} has schema_version={}, this binary only knows up to {}",
                            path.display(),
                            parsed.schema_version,
                            Self::CURRENT_SCHEMA_VERSION
                        ),
                        http_status: None,
                    });
                }
                tracing::debug!(
                    path = %path.display(),
                    providers = parsed.providers.len(),
                    "ParamRejectionsFile::load: ok"
                );
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::trace!(path = %path.display(), "ParamRejectionsFile::load: missing, returning empty");
                Ok(Self::new_empty())
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "ParamRejectionsFile::load: io error");
                Err(Error::Io(crate::error::IoError::Raw(e)))
            }
        }
    }

    /// Persist to disk. Writes via `tempfile` then renames so a crash
    /// mid-write cannot leave a truncated file. The same write-then-
    /// rename pattern is used by [`crate::llm::probe::MaxTokensTableFile`]
    /// and the temperature sidecar.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Provider {
                message: format!(
                    "create dir for param_rejections.toml at {}: {e}",
                    parent.display()
                ),
                http_status: None,
            })?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| Error::Provider {
            message: format!("encode param_rejections.toml: {e}"),
            http_status: None,
        })?;
        let tmp = tempfile::Builder::new()
            .suffix(".toml.tmp")
            .tempfile_in(path.parent().unwrap_or(Path::new(".")))
            .map_err(|e| Error::Provider {
                message: format!("tempfile for param_rejections.toml: {e}"),
                http_status: None,
            })?;
        std::fs::write(tmp.path(), body).map_err(|e| Error::Provider {
            message: format!("write param_rejections.toml: {e}"),
            http_status: None,
        })?;
        tmp.persist(path).map_err(|e| Error::Provider {
            message: format!("rename param_rejections.toml into place: {e}"),
            http_status: None,
        })?;
        Ok(())
    }
}

/// In-memory cache of `(provider, model) -> set_of_rejected_params`,
/// backed by the on-disk [`ParamRejectionsFile`] for cross-run
/// persistence. Wrapped in an `Arc` so the same handle can travel
/// through `ProviderRegistry` → `RunContext` → every LLM call site.
///
/// Concurrency model: `parking_lot::RwLock` over the inner file. The
/// hot path (`should_omit`) only takes a read lock; the rare
/// persistence path (`record`) takes a write lock and writes
/// atomically via tempfile + rename.
#[derive(Clone)]
pub struct ParamRejectionsTable {
    inner: Arc<ParkingRwLock<ParamRejectionsFile>>,
    /// Path to the on-disk TOML file. `None` when persistence is
    /// disabled (the operator opted out or the home could not be
    /// resolved). Reads still work; writes are silently skipped.
    persist_path: Option<PathBuf>,
}

impl std::fmt::Debug for ParamRejectionsTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read();
        f.debug_struct("ParamRejectionsTable")
            .field("entries", &inner.providers)
            .finish()
    }
}

impl ParamRejectionsTable {
    /// Build a table from the on-disk file at
    /// `<MOAGAN_HOME>/param_rejections.toml`.
    pub fn from_home(home: &MoaganHome) -> Result<Self> {
        let path = home.param_rejections_path();
        Self::from_path(&path)
    }

    /// Build a table from an explicit path. Used by tests and by
    /// [`Self::from_home`]. Persistence is enabled whenever the path
    /// is reachable; the file is created on the first write so a
    /// missing on-disk file is fine.
    pub fn from_path(path: &Path) -> Result<Self> {
        let file = ParamRejectionsFile::load(path)?;
        Ok(Self {
            inner: Arc::new(ParkingRwLock::new(file)),
            persist_path: Some(path.to_path_buf()),
        })
    }

    /// Build a fresh table with no on-disk backing. Persistence is
    /// disabled; reads work as for any other table.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(ParkingRwLock::new(ParamRejectionsFile::new_empty())),
            persist_path: None,
        }
    }

    /// Returns true if `(provider, model)` is known to reject `param`.
    /// The hot path: every LLM call site calls this once per
    /// `PARAM_NAMES` entry before serialising the wire body.
    pub fn should_omit(&self, provider: &str, model: &str, param: &str) -> bool {
        let omit = {
            let inner = self.inner.read();
            inner
                .providers
                .get(provider)
                .and_then(|by_model| by_model.get(model))
                .is_some_and(|rejects| rejects.contains(param))
        };
        tracing::trace!(
            provider,
            model,
            param,
            omit,
            "ParamRejectionsTable::should_omit"
        );
        omit
    }

    /// Record a rejection. Persists to TOML best-effort so a disk
    /// error does not abort the call. Already-known rejections are
    /// idempotent (the underlying `BTreeSet` dedupes), so the same
    /// upstream firing the same rejection repeatedly does not produce
    /// a write storm — the underlying file is rewritten only when the
    /// set actually changes.
    pub fn record(&self, provider: &str, model: &str, param: &str) -> Result<()> {
        let mut inserted = false;
        {
            let mut inner = self.inner.write();
            let entry = inner
                .providers
                .entry(provider.to_owned())
                .or_default()
                .entry(model.to_owned())
                .or_default();
            if entry.insert(param.to_owned()) {
                inserted = true;
            }
        }
        if inserted {
            tracing::info!(
                provider,
                model,
                param,
                "ParamRejectionsTable::record: new rejection recorded"
            );
            if let Some(path) = self.persist_path.as_ref() {
                self.persist_to(path)?;
            }
        } else {
            tracing::trace!(
                provider,
                model,
                param,
                "ParamRejectionsTable::record: already known, noop"
            );
        }
        Ok(())
    }

    /// Persist the current in-memory state to disk. Best-effort:
    /// callers wrap in `if let Err(_)` because losing a rejection is
    /// preferable to aborting the run. Re-reads the file before
    /// writing so a separate process that wrote the same file in
    /// parallel does not get clobbered.
    fn persist_to(&self, path: &Path) -> Result<()> {
        // Merge with whatever the on-disk sidecar already carries
        // — a separate process (or a previous invocation) may have
        // written its own entries for a different provider/model.
        let mut file = ParamRejectionsFile::load(path)?;
        {
            let inner = self.inner.read();
            for (provider, by_model) in &inner.providers {
                let dest_provider = file.providers.entry(provider.clone()).or_default();
                for (model, rejects) in by_model {
                    let dest_set = dest_provider.entry(model.clone()).or_default();
                    for r in rejects {
                        dest_set.insert(r.clone());
                    }
                }
            }
        }
        file.save(path)
    }

    /// Snapshot the current rejections (for diagnostics / tests).
    pub fn snapshot(&self) -> ParamRejectionsFile {
        self.inner.read().clone()
    }

    /// Number of `(provider, model)` pairs in the cache.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.read().providers.values().map(|m| m.len()).sum()
    }

    /// True when no rejections are cached.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.read().providers.is_empty()
    }
}

/// Auto-detect the parameter name rejected by the upstream.
///
/// Returns `Some(param_name)` when the HTTP status is 4xx AND the
/// body matches one of the known rejection signatures; `None`
/// otherwise. Convenience wrapper around [`detect_all_rejections`]
/// that returns the first match — equivalent to the legacy
/// single-shot contract the rest of the codebase relied on before
/// the cascade-recovery loop. New code should prefer
/// [`detect_all_rejections`] directly so a single upstream response
/// can seed every rejected name into the table at once.
///
/// The seven patterns cover every rejection observed in the spike
/// (Anthropic, OpenAI-compat, OpenCode Go, DeepSeek, MiniMax
/// Anthropic-direct). All 7 patterns are regex-driven; `regex` is
/// already a regular dependency so no new crates are pulled in.
///
/// Pattern catalogue:
///
/// 1. `[unknown_parameter] Unknown parameter: '<param>'` (Anthropic)
/// 2. `[invalid_request_error] Unsupported parameter: '<param>'`
///    (OpenAI Responses)
/// 3. `invalid params, param '<param>' should be ...`
///    (MiniMax Anthropic-direct)
/// 4. `error.param` structured field (deterministic — the
///    upstream's API actually hands us the offending field name)
/// 5. `invalid <param>: ...` (OpenCode Go kimi-k3)
/// 6. `Invalid <param> value, the valid range ...` (DeepSeek)
/// 7. `<param>: invalid value` (DeepSeek deserialization)
/// 8. Legacy freeform: `<param> is too large: N`
pub fn detect_rejection(status: u16, body: &str) -> Option<String> {
    let found = detect_all_rejections(status, body).into_iter().next();
    tracing::trace!(status, present = found.is_some(), param = ?found, "detect_rejection");
    found
}

/// Cascade-recovery counterpart of [`detect_rejection`]. Returns
/// **every** wire parameter name mentioned in the upstream's 4xx
/// body, deduplicated (via `BTreeSet` so the output is in canonical
/// order across runs) and filtered against [`PARAM_NAMES`] — the
/// whitelist of names the dispatcher actually knows how to omit.
/// Returns an empty `Vec` for non-4xx status codes, malformed JSON,
/// or 4xx bodies that don't match any rejection signature.
///
/// The dispatch loop consults this helper once per retry and omits
/// every detected name in the same iteration: a single upstream
/// response that lists `"Unknown parameters: 'temperature',
/// 'max_tokens', 'top_p'"` (the canonical `gpt-5.6-luna` cascade)
/// seeds all three into the table in one round-trip instead of
/// the legacy single-shot behaviour that only recorded the first.
pub fn detect_all_rejections(status: u16, body: &str) -> Vec<String> {
    if !(400..500).contains(&status) {
        return Vec::new();
    }
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut found: BTreeSet<String> = BTreeSet::new();

    // Pattern #4B (deterministic): the upstream's own structured
    // field. Handled first because it carries the field name
    // verbatim — no regex needed. Whitelisted against
    // `PARAM_NAMES` so a freeform `error.param` field that names
    // something the dispatcher cannot omit (e.g. `input`) does not
    // poison the cascade with an un-droppable name.
    if let Some(param) = v
        .get("error")
        .and_then(|e| e.get("param"))
        .and_then(|p| p.as_str())
        && let Some(capture) = parse_error_param_start(param)
        && PARAM_NAMES.contains(&capture.as_str())
    {
        tracing::trace!(param = %capture, "detect_all_rejections: structured error.param hit");
        found.insert(capture);
    }

    // The rest of the patterns are freeform `error.message`
    // strings. Each upstream uses its own phrasing; we run them in
    // reliability order so the most specific pattern wins.
    if let Some(msg) = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        for cap in run_message_patterns(msg) {
            // Whitelist: only emit names the dispatch path can
            // actually omit. A message like "invalid input: ..."
            // captures "input" under pattern #5, but `input` is the
            // user prompt — dropping it breaks the wire contract.
            if PARAM_NAMES.contains(&cap.as_str()) {
                found.insert(cap);
            }
        }
    }

    found.into_iter().collect()
}

/// Test-only escape hatch for the regex set so unit tests can drive
/// the message branch without an HTTP-shaped body. Iterates over
/// every regex in the catalogue and emits all matches; the order is
/// the same as the legacy single-shot chain so a regression points
/// the operator at the right pattern to fix. Returns the captures
/// as a `Vec<String>` rather than `Option<String>` so a single
/// message like `"Unknown parameters: 'a', 'b', 'c'"` (handled by
/// the dedicated plural extractor at the top of the function) yields
/// every name.
fn run_message_patterns(msg: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Pattern: plural-form "Unknown parameters: '<a>', '<b>', '<c>'"
    // (Anthropic-compat relay + several OpenCode Go routes emit
    // this shape). Must run before the singular #1+#2 extractor so
    // the three quoted names are captured in one pass.
    for cap in captures_iter(msg, r"'([A-Za-z_][A-Za-z0-9_]*)'") {
        if !out.contains(&cap) {
            out.push(cap);
        }
    }
    // The plural-form extractor above is permissive — it accepts
    // any quoted identifier — so we additionally run the singular
    // patterns to seed names like `top_p` that may appear
    // without quotes. The two extractors together close the
    // cascade gap on `gpt-5.6-luna` (plural) and the canonical
    // Anthropic "Unknown parameter: 'max_tokens'" (singular).

    // Pattern #4A (MiniMax Anthropic-direct).
    if let Some(cap) = capture(msg, r"invalid params, param '([A-Za-z_][A-Za-z0-9_]*)'") {
        push_unique(&mut out, cap);
    }

    // Patterns #1 + #2: case-insensitive, with optional
    // `[bracket]` prefix. Raw string with `r#"..."#` so the
    // embedded `"` and `<` inside the character class do not
    // terminate the literal.
    if let Some(cap) = capture(
        msg,
        r#"(?i)(?:\[(?:unknown_parameter|invalid_request_error)\]\s*)?(?:unknown|unsupported) parameter[:\s]+['"<]?([A-Za-z_][A-Za-z0-9_]*)"#,
    ) {
        push_unique(&mut out, cap);
    }

    // Pattern #7: DeepSeek deserialization
    // "Failed to deserialize the JSON body: max_tokens: invalid value".
    // Run BEFORE #5 because the message also contains "invalid
    // value:" (where "value" is a generic noun, not a parameter
    // name) — pattern #5 would capture "value" if it ran first.
    if let Some(cap) = capture(msg, r"([A-Za-z_][A-Za-z0-9_]*):\s+invalid value") {
        push_unique(&mut out, cap);
    }

    // Pattern #6: DeepSeek "Invalid temperature value, the valid
    // range of temperature is [0, 2]".
    if let Some(cap) = capture(
        msg,
        r"(?i)invalid ([A-Za-z_][A-Za-z0-9_]*) value,\s+the valid range",
    ) {
        push_unique(&mut out, cap);
    }

    // Pattern #5: OpenCode Go kimi-k3 "invalid temperature: only 1".
    // Anchored on `invalid <param>:` so the noun after "invalid"
    // is the parameter name (not a generic word like "value").
    if let Some(cap) = capture(msg, r"(?i)invalid ([A-Za-z_][A-Za-z0-9_]*):\s+[a-zA-Z]") {
        push_unique(&mut out, cap);
    }

    // Pattern #4B legacy: "<param> is too large: N" without leading
    // "must be|is" (some upstreams emit it directly).
    if let Some(cap) = capture(
        msg,
        r"(?i)([A-Za-z_][A-Za-z0-9_]*) is (?:too|invalid) (?:large|small)",
    ) {
        push_unique(&mut out, cap);
    }

    out
}

/// Push `cap` into `out` only when absent — the regex catalogue
/// captures the same identifier from multiple patterns (e.g. a
/// message like `"Failed to deserialize ... temperature: invalid
// value"` triggers both #7 and the generic `'<param>'` plural
/// extractor). Without dedup, the cascade would persist the same
/// name twice and burn a retry budget entry for nothing.
fn push_unique(out: &mut Vec<String>, cap: String) {
    if !out.contains(&cap) {
        out.push(cap);
    }
}

/// Run a single regex against `haystack` and return the first
/// capture group as a `String` (empty string when the regex has no
/// captures). Returns `None` when the regex does not match. The
/// compile cost is paid on every call — fine because
/// `detect_rejection` only runs on HTTP 4xx responses (the rare
/// path); adding a global regex cache would not pay for itself
/// unless the dispatch path started logging every 200 OK through
/// this helper.
fn capture(haystack: &str, pattern: &str) -> Option<String> {
    let re = regex::Regex::new(pattern).ok()?;
    let caps = re.captures(haystack)?;
    let m = caps.get(1)?;
    Some(m.as_str().to_owned())
}

/// Multi-match counterpart of [`capture`]. Returns every captured
/// group-1 across all matches in the haystack (in left-to-right
/// order). The same compile-cost caveat as [`capture`] applies —
/// used by [`run_message_patterns`] on the rare 4xx path, not on
/// every dispatch.
fn captures_iter(haystack: &str, pattern: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(pattern) else {
        return Vec::new();
    };
    re.captures_iter(haystack)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_owned()))
        .collect()
}

/// The `error.param` field is a freeform string the upstream uses
/// to communicate which wire field was rejected. We extract the
/// first contiguous run of identifier characters — the upstream may
/// continue with `is too large: N` or `must be within [0, 1.5]`, all
/// of which start with the field name. Returns `None` when the
/// string does not start with a valid Rust-style identifier (the
/// upstream emits an empty string on rare occasions).
fn parse_error_param_start(s: &str) -> Option<String> {
    let s = s.trim();
    let end = s
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let candidate = &s[..end];
    let first = candidate.chars().next()?;
    if (first.is_ascii_alphabetic() || first == '_')
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Some(candidate.to_owned())
    } else {
        None
    }
}

/// Whitelist of wire field names the dispatch path is allowed to
/// emit. Any field outside this list on a serialised request body is
/// a candidate for "silent acceptance" — some upstreams swallow
/// unknown fields and behave inconsistently on the next call. The
/// helper emits a `WARN` per unknown field so the operator can see
/// the audit hint in the run logs.
const KNOWN_WIRE_FIELDS: &[&str] = &[
    // Core Request struct fields that every wire builder maps to
    // the upstream's per-protocol name.
    "role",
    "model",
    "messages",
    "system",
    "user",
    "max_tokens",
    "temperature",
    "top_p",
    "stream",
    "stop",
    "stop_sequences",
    "tools",
    "tool_choice",
    "response_format",
    "text",
    "input",
    "instructions",
    // Moagan-specific fields that are intentional and
    // expected by the pipeline (PromptPrefill, attachments, JSON
    // schema, tool selection).
    "extra_messages",
    "attachments",
    "response_schema",
    // Anthropic-only fields.
    "thinking",
    "metadata",
];

/// Audit the serialised wire body for unknown fields. Emits a
/// `tracing::warn!` per non-standard field so the operator can spot
/// silent acceptance in the run logs. Safe to call on every
/// dispatch — the whitelist is small and `serde_json::Value` lookups
/// are O(n) over the field set.
pub fn audit_unknown_fields(body: &serde_json::Value) {
    let Some(obj) = body.as_object() else {
        return;
    };
    for key in obj.keys() {
        if !KNOWN_WIRE_FIELDS.contains(&key.as_str()) {
            tracing::warn!(
                field = %key,
                "wire body contains non-standard field; some upstreams silently accept \
                 these and may break reproducibility — set it via ProviderConfig or \
                 add it to the whitelist"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- ParamRejectionsFile --------------------------------------------------

    #[test]
    fn file_new_empty_has_current_schema() {
        let f = ParamRejectionsFile::new_empty();
        assert_eq!(
            f.schema_version,
            ParamRejectionsFile::CURRENT_SCHEMA_VERSION
        );
        assert!(f.providers.is_empty());
    }

    #[test]
    fn file_load_missing_returns_empty() {
        let path = Path::new("/nonexistent/param_rejections.toml");
        let f = ParamRejectionsFile::load(path).unwrap();
        assert!(f.providers.is_empty());
        assert_eq!(f.schema_version, 1);
    }

    #[test]
    fn file_load_rejects_future_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("param_rejections.toml");
        std::fs::write(&path, "schema_version = 999\n[providers]\n").unwrap();
        let err = ParamRejectionsFile::load(&path).expect_err("future schema must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("schema_version")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[test]
    fn file_load_rejects_malformed_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("param_rejections.toml");
        std::fs::write(&path, "this is = not valid toml = at all").unwrap();
        let err = ParamRejectionsFile::load(&path).expect_err("malformed must error");
        match err {
            Error::Provider { message, .. } => assert!(message.contains("malformed")),
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[test]
    fn file_round_trip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("param_rejections.toml");
        let mut f = ParamRejectionsFile::new_empty();
        let set = f
            .providers
            .entry("opencode".to_owned())
            .or_default()
            .entry("gpt-5.6-luna".to_owned())
            .or_default();
        set.insert("temperature".to_owned());
        f.save(&path).unwrap();
        let back = ParamRejectionsFile::load(&path).unwrap();
        assert_eq!(back, f);
    }

    // ----- ParamRejectionsTable ------------------------------------------------

    #[test]
    fn table_empty_has_no_rejections() {
        let t = ParamRejectionsTable::empty();
        assert!(!t.should_omit("opencode", "gpt-5.6-luna", "temperature"));
        assert!(t.is_empty());
    }

    #[test]
    fn table_record_marks_rejection() {
        let t = ParamRejectionsTable::empty();
        t.record("opencode", "gpt-5.6-luna", "temperature").unwrap();
        assert!(t.should_omit("opencode", "gpt-5.6-luna", "temperature"));
        assert!(!t.should_omit("opencode", "gpt-5.6-luna", "top_p"));
    }

    #[test]
    fn table_record_is_idempotent_per_param() {
        let t = ParamRejectionsTable::empty();
        t.record("p", "m", "temperature").unwrap();
        t.record("p", "m", "temperature").unwrap();
        let snap = t.snapshot();
        let set = snap.providers.get("p").unwrap().get("m").unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains("temperature"));
    }

    #[test]
    fn table_record_persists_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("param_rejections.toml");
        let t = ParamRejectionsTable::from_path(&path).unwrap();
        t.record("opencode", "gpt-5.6-luna", "temperature").unwrap();
        assert!(path.exists(), "TOML must be written on first record");
        let back = ParamRejectionsTable::from_path(&path).unwrap();
        assert!(back.should_omit("opencode", "gpt-5.6-luna", "temperature"));
    }

    #[test]
    fn table_persist_merges_with_on_disk() {
        // Simulate a separate process that already wrote the file
        // with a different (provider, model) entry.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("param_rejections.toml");
        let t1 = ParamRejectionsTable::from_path(&path).unwrap();
        t1.record("p1", "m1", "top_p").unwrap();
        // A second table instance observes the same on-disk file
        // and writes its own entry.
        let t2 = ParamRejectionsTable::from_path(&path).unwrap();
        t2.record("p2", "m2", "temperature").unwrap();
        // Both entries survive.
        let merged = ParamRejectionsFile::load(&path).unwrap();
        assert!(
            merged
                .providers
                .get("p1")
                .and_then(|m| m.get("m1"))
                .map(|s| s.contains("top_p"))
                .unwrap_or(false)
        );
        assert!(
            merged
                .providers
                .get("p2")
                .and_then(|m| m.get("m2"))
                .map(|s| s.contains("temperature"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn table_from_home_loads_under_temp_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(dir.path().to_path_buf());
        let path = home.param_rejections_path();
        assert!(path.ends_with("param_rejections.toml"));
        let t = ParamRejectionsTable::from_home(&home).unwrap();
        assert!(!t.should_omit("any", "thing", "temperature"));
    }

    // ----- detect_rejection: pattern coverage ─────────

    fn body_json(s: &str) -> String {
        serde_json::json!({"error": {"message": s}}).to_string()
    }

    #[test]
    fn detect_pattern_1_unknown_parameter_anthropic() {
        let body = r#"{"error":{"type":"unknown_parameter","message":"[unknown_parameter] Unknown parameter: 'max_tokens'"}}"#;
        assert_eq!(detect_rejection(400, body).as_deref(), Some("max_tokens"));
    }

    #[test]
    fn detect_pattern_1_unknown_parameter_lowercase() {
        let body = body_json("unknown parameter: 'temperature'");
        assert_eq!(detect_rejection(400, &body).as_deref(), Some("temperature"));
    }

    #[test]
    fn detect_pattern_2_unsupported_parameter_responses() {
        let body = body_json(
            "[invalid_request_error] Unsupported parameter: 'top_p' is not supported with this model.",
        );
        assert_eq!(detect_rejection(400, &body).as_deref(), Some("top_p"));
    }

    #[test]
    fn detect_pattern_3_minimax_anthropic() {
        let body = body_json("invalid params, param 'top_p' should be in (0,1] (2013)");
        assert_eq!(detect_rejection(400, &body).as_deref(), Some("top_p"));
    }

    #[test]
    fn detect_pattern_4b_structured_error_param() {
        let body = r#"{"error":{"param":"temperature must be within [0, 1.5]","type":"server_error","message":"..."}}"#;
        assert_eq!(detect_rejection(400, body).as_deref(), Some("temperature"));
    }

    #[test]
    fn detect_pattern_4b_structured_error_param_too_large() {
        let body = r#"{"error":{"param":"max_tokens is too large: 999999. This model supports at most 131072 completion tokens, whereas you provided 999999.","type":"server_error","message":"..."}}"#;
        // The dispatch path can't drop `max_tokens` (it's not an
        // Option), but the detector must still extract the field
        // name for the diagnostic.
        assert_eq!(detect_rejection(400, body).as_deref(), Some("max_tokens"));
    }

    #[test]
    fn detect_pattern_5_kimi_k3_invalid() {
        let body = body_json("invalid temperature: only 1 is allowed for this model");
        assert_eq!(detect_rejection(400, &body).as_deref(), Some("temperature"));
    }

    #[test]
    fn detect_pattern_6_deepseek_invalid_value() {
        let body = body_json("Invalid temperature value, the valid range of temperature is [0, 2]");
        assert_eq!(detect_rejection(400, &body).as_deref(), Some("temperature"));
    }

    #[test]
    fn detect_pattern_7_deepseek_deserialization() {
        let body = body_json(
            "Failed to deserialize the JSON body into the target type: max_tokens: invalid value: integer `-1`",
        );
        assert_eq!(detect_rejection(400, &body).as_deref(), Some("max_tokens"));
    }

    #[test]
    fn detect_returns_none_for_2xx() {
        let body = r#"{"error":{"param":"temperature","message":"..."}}"#;
        assert_eq!(detect_rejection(200, body), None);
    }

    #[test]
    fn detect_returns_none_for_5xx() {
        let body = body_json("internal server error");
        assert_eq!(detect_rejection(500, &body), None);
    }

    #[test]
    fn detect_returns_none_for_unrelated_4xx() {
        let body = r#"{"error":"model not found"}"#;
        assert_eq!(detect_rejection(404, body), None);
    }

    #[test]
    fn detect_returns_none_for_malformed_body() {
        assert_eq!(detect_rejection(400, "not json"), None);
    }

    #[test]
    fn detect_returns_none_when_message_field_missing() {
        let body = r#"{"error":{"type":"invalid_request_error"}}"#;
        assert_eq!(detect_rejection(400, body), None);
    }

    // ----- parse_error_param_start ------------------------------------------------

    #[test]
    fn parse_error_param_extracts_simple_identifier() {
        assert_eq!(
            parse_error_param_start("temperature must be within [0, 1.5]"),
            Some("temperature".to_owned())
        );
    }

    #[test]
    fn parse_error_param_handles_underscore_start() {
        assert_eq!(
            parse_error_param_start("_internal_field is too large"),
            Some("_internal_field".to_owned())
        );
    }

    #[test]
    fn parse_error_param_returns_none_for_empty() {
        assert_eq!(parse_error_param_start(""), None);
    }

    #[test]
    fn parse_error_param_returns_none_for_digit_start() {
        // Identifiers cannot start with a digit in Rust / most
        // JSON conventions.
        assert_eq!(parse_error_param_start("1abc is too large"), None);
    }

    // ----- audit_unknown_fields --------------------------------------------------

    #[test]
    fn audit_emits_no_warn_for_known_fields() {
        // Direct call to the underlying helper (rather than capturing
        // tracing output) — we just want to know that the function
        // does NOT panic on a known-only body. The actual WARN emission
        // is covered by the integration test.
        let body = serde_json::json!({
            "model": "MiniMax-M3",
            "messages": [],
            "temperature": 0.6,
            "max_tokens": 1024,
        });
        audit_unknown_fields(&body);
    }

    #[test]
    fn audit_handles_non_object_body_without_panic() {
        let body = serde_json::json!(["a", "b"]);
        audit_unknown_fields(&body);
    }

    // ----- detect_all_rejections: cascade-recovery surface -----

    /// The canonical `gpt-5.6-luna` cascade: a single upstream
    /// response that lists every forbidden wire field in one
    /// sentence. The detector must surface all three so the
    /// dispatcher's retry loop can omit them in a single iteration
    /// instead of the legacy single-shot behaviour that would only
    /// record the first match and burn two more round-trips.
    #[test]
    fn detect_all_returns_three_names_from_unknown_parameters_list() {
        let body = body_json("Unknown parameters: 'temperature', 'max_tokens', 'top_p'");
        let detected = detect_all_rejections(400, &body);
        assert!(
            detected.iter().any(|s| s == "temperature"),
            "temperature must be detected; got {detected:?}"
        );
        assert!(
            detected.iter().any(|s| s == "max_tokens"),
            "max_tokens must be detected; got {detected:?}"
        );
        assert!(
            detected.iter().any(|s| s == "top_p"),
            "top_p must be detected; got {detected:?}"
        );
    }

    /// When the upstream mentions the same name across multiple
    /// patterns (plural extractor + singular extractor), the output
    /// must dedupe so the cascade retry budget isn't burned by
    /// phantom duplicates.
    #[test]
    fn detect_all_dedupes_repeated_names() {
        // `temperature` appears quoted (caught by the plural
        // extractor) and again under "invalid temperature: ..."
        // (caught by pattern #5). Without dedup we'd see two
        // entries; the table's `BTreeSet` would also dedup at
        // persist time, so the test pins the contract at the
        // detector boundary instead of relying on the persistence
        // path to swallow duplicates.
        let body = body_json("Unknown parameters: 'temperature', 'temperature'");
        let detected = detect_all_rejections(400, &body);
        let temp_count = detected.iter().filter(|s| *s == "temperature").count();
        assert_eq!(
            temp_count, 1,
            "temperature must appear once; got {detected:?}"
        );
    }

    /// A 4xx that has nothing to do with param rejection (404,
    /// 401, plain text) must yield an empty `Vec` so the cascade
    /// loop's `while detected.is_empty()` branch fires and aborts
    /// cleanly instead of recording noise.
    #[test]
    fn detect_all_returns_empty_for_unrelated_4xx() {
        assert_eq!(
            detect_all_rejections(404, r#"{"error":"model not found"}"#).len(),
            0
        );
        assert_eq!(
            detect_all_rejections(401, r#"{"error":"invalid api key"}"#).len(),
            0
        );
        assert_eq!(detect_all_rejections(400, "not json").len(), 0);
        assert_eq!(
            detect_all_rejections(200, &body_json("Unknown parameters: 'temperature'")).len(),
            0
        );
        assert_eq!(
            detect_all_rejections(500, &body_json("internal server error")).len(),
            0
        );
    }

    /// `detect_rejection` (the legacy single-shot surface) must
    /// remain a strict wrapper over `detect_all_rejections` so the
    /// 12 existing integration tests keep their contract: returns
    /// the FIRST detected name in canonical `BTreeSet` order.
    #[test]
    fn detect_rejection_wrapper_returns_first_from_detect_all() {
        let body = body_json("Unknown parameters: 'temperature', 'max_tokens', 'top_p'");
        let first = detect_rejection(400, &body);
        let all = detect_all_rejections(400, &body);
        assert_eq!(
            first.as_deref(),
            all.first().map(String::as_str),
            "detect_rejection must return detect_all_rejections's first element; first={first:?}, all={all:?}"
        );
        assert!(first.is_some(), "cascade body must yield at least one name");
    }

    /// Pattern #4B whitelisting: when the upstream's structured
    /// `error.param` field names a token that is NOT in
    /// `PARAM_NAMES` (e.g. `input` — the user prompt, which the
    /// dispatch path cannot omit), the detector must ignore it
    /// instead of seeding the cascade with an un-droppable name.
    #[test]
    fn detect_rejection_ignores_param_field_not_in_param_names() {
        // `input` is the user prompt — dropping it would break the
        // wire contract. The detector must surface an empty result.
        let body = r#"{"error":{"param":"input must be a non-empty array","type":"invalid_request_error","message":"..."}}"#;
        assert_eq!(detect_rejection(400, body), None);
        let all = detect_all_rejections(400, body);
        assert!(
            all.is_empty(),
            "non-whitelisted error.param must not leak into the cascade; got {all:?}"
        );
    }

    /// Pattern #4B positive case: when `error.param` names a known
    /// wire field, the detector must surface it so the cascade
    /// table can record it and the retry omits it.
    #[test]
    fn detect_rejection_accepts_param_field_in_param_names() {
        let body = r#"{"error":{"param":"max_tokens is too large: 999999","type":"server_error","message":"..."}}"#;
        assert_eq!(detect_rejection(400, body).as_deref(), Some("max_tokens"));
    }

    /// When `error.param` names a non-whitelisted token, the
    /// detector must fall through to the message branch and pick
    /// up any other field name the message body happens to list.
    /// Pins the cascade contract that "the table can grow even
    /// when one of the two extractor arms is dead".
    #[test]
    fn detect_rejection_falls_back_to_message_when_param_ignored() {
        // `error.param` is `input` (ignored), but the message
        // body still names `temperature` under pattern #7.
        let body = r#"{"error":{"param":"input must be a non-empty array","message":"Failed to deserialize: temperature: invalid value","type":"invalid_request_error"}}"#;
        assert_eq!(
            detect_rejection(400, body).as_deref(),
            Some("temperature"),
            "message branch must win when error.param is non-whitelisted"
        );
    }

    /// `model` is NOT in `PARAM_NAMES` today, so a structured
    /// `error.param = "model"` must be ignored by the detector —
    /// the cascade table would not know how to omit it. The test
    /// pins the whitelist boundary so a future contributor who
    /// expands `PARAM_NAMES` to include `model` sees the detector
    /// surface it (and updates this test in the same commit).
    #[test]
    fn detect_rejection_handles_model_param_field_correctly() {
        let body = r#"{"error":{"param":"model not found","type":"invalid_request_error","message":"..."}}"#;
        assert_eq!(
            detect_rejection(400, body),
            None,
            "non-whitelisted error.param must be ignored"
        );
    }
}
