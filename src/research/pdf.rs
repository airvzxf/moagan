//! K.4 sub-1: PDF text extraction via `pdftotext` shelling out
//! ([`docs/proposal-04-cuarta-etapa.md`](../docs/proposal-04-cuarta-etapa.md) §4).
//!
//! External dependency: the `pdftotext` binary from the
//! `poppler-utils` system package (Arch: `pacman -S poppler`;
//! Debian: `apt install poppler-utils`). The binary is Linux-only
//! per [`docs/deferred-v0.9-2026-08-16.md`](../docs/deferred-v0.9-2026-08-16.md)
//! §1.2 — no macOS / WSL story.
//!
//! Why shell out instead of pulling in a pure-Rust PDF crate like
//! `lopdf`:
//!
//! 1. **Zero new dependencies** — the no-go list
//!    ([`docs/adr/0001-no-go-list-policy.md`](../docs/adr/0001-no-go-list-policy.md))
//!    treats every new crate as overhead, and `pdftotext` is
//!    already on every Arch host the operator targets.
//! 2. **Battle-tested extraction** — poppler's text layout
//!    heuristics are the reference implementation; a pure-Rust
//!    port would re-derive years of edge-case fixes for forms,
//!    CIDFonts, and embedded fonts.
//! 3. **Tight error surface** — when the binary is missing the
//!    caller gets [`Error::ResearchUnavailable`] with a hint to
//!    install `poppler-utils`. When the PDF is malformed poppler
//!    returns a non-zero exit code that surfaces as a generic
//!    [`Error::Provider`].
//!
//! The module exposes a single public function,
//! [`fetch_pdf_text`], that downloads a PDF from an allowlisted
//! URL and returns the extracted UTF-8 text. The allowlist
//! pre-filter is the caller's responsibility (so the function
//! plays well with the rest of [`crate::research`]) but the
//! module re-checks defensively so a future caller that skips
//! the upstream filter cannot accidentally exfiltrate to a
//! non-allowlisted host.

use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::research::allowlist;
use crate::research::fetcher::REQUEST_TIMEOUT;

/// Default cap on input bytes fed to `pdftotext`. Set deliberately
/// larger than [`crate::research::fetcher::MAX_BYTES_PER_URL`]
/// (4 KB) because the fetcher cap applies to the *HTML* snippet
/// path; a PDF can be tens of MB on disk while still yielding
/// <100 KB of extracted text. Operators tune this per call site.
pub const DEFAULT_MAX_INPUT_BYTES: u32 = 4 * 1024 * 1024;

/// Defensive cap on the extracted text returned to the caller.
/// Anything larger is truncated and suffixed with `(truncated)`
/// so the Sketch phase cannot accidentally pull a megabyte-class
/// string into context. Matches the order of magnitude of one
/// `mode=deep` research snippet.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Per-call stderr cap. We only need stderr for the failure path;
/// the success path discards it. 4 KB is plenty for the poppler
/// error format.
const STDERR_CAP: u64 = 4 * 1024;

/// Heuristic: does this URL look like a PDF link? Returns `true`
/// when the path component (ignoring query string and fragment)
/// ends with `.pdf` (case insensitive). The allowlist filter still
/// runs separately via [`allowlist::is_allowed`] — this helper
/// only picks the parser path.
pub fn looks_like_pdf_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let result = path.to_ascii_lowercase().ends_with(".pdf");
    tracing::trace!(
        url,
        path,
        looks_like_pdf = result,
        "research::pdf::looks_like_pdf_url"
    );
    result
}

/// Returns `true` when `binary_name` resolves to an executable
/// file via the current `PATH`. Implemented without an extra
/// dependency so the no-go list stays clean (a `which` crate
/// would be one row but adds compile time + maintenance).
///
/// `pub` so the integration test in `tests/integration_pdf.rs`
/// can skip when `poppler-utils` is not installed without
/// shelling out a second `Command::new` probe.
pub fn binary_in_path(binary_name: &str) -> bool {
    tracing::trace!(binary_name, "research::pdf::binary_in_path: probing");
    let Some(paths) = std::env::var_os("PATH") else {
        tracing::trace!("research::pdf::binary_in_path: PATH unset");
        return false;
    };
    for path in std::env::split_paths(&paths) {
        // PATH lookups honour the platform's executable
        // semantics implicitly: `is_file()` returns false for
        // directories and for symlink targets that don't exist.
        // On Linux a non-`x` file also returns false here, which
        // is the right call — `Command::new` would fail to spawn
        // it anyway.
        if path.join(binary_name).is_file() {
            tracing::debug!(
                binary_name,
                path = %path.display(),
                "research::pdf::binary_in_path: found"
            );
            return true;
        }
    }
    tracing::trace!(binary_name, "research::pdf::binary_in_path: not found");
    false
}

/// Public entry point: download `url` (allowlist-vetted by the
/// caller; the module re-checks defensively), pipe the bytes
/// through `pdftotext`, and return the extracted UTF-8 text.
///
/// `max_bytes` caps the *input* PDF payload — anything larger is
/// truncated before being fed to `pdftotext`. Output truncation is
/// a fixed [`MAX_OUTPUT_BYTES`] cap that always applies on the
/// success path.
///
/// Failure modes:
///
/// - URL parse / no host / host not in allowlist →
///   [`Error::InvalidArgs`] (caller bug, not a transport
///   problem).
/// - `pdftotext` missing from `PATH` →
///   [`Error::ResearchUnavailable`] with a hint to install
///   `poppler-utils`.
/// - HTTP transport / non-2xx status / `pdftotext` non-zero
///   exit → [`Error::Provider`].
/// - Empty response body → [`Error::ResearchUnavailable`].
pub async fn fetch_pdf_text(url: &str, max_bytes: u32) -> Result<String> {
    tracing::debug!(url, max_bytes, "research::pdf::fetch_pdf_text: enter");
    let parsed =
        reqwest::Url::parse(url).map_err(|e| Error::InvalidArgs(format!("pdf url parse: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::InvalidArgs("pdf url has no host".into()))?;
    if !allowlist::is_allowed(host) {
        tracing::warn!(
            url,
            host,
            "research::pdf::fetch_pdf_text: host not in allowlist"
        );
        return Err(Error::InvalidArgs(format!(
            "pdf host '{host}' not in research allowlist"
        )));
    }

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| Error::Provider {
            message: format!("pdf reqwest client build: {e}"),
            http_status: None,
        })?;
    let resp = client.get(url).send().await.map_err(|e| Error::Provider {
        message: format!("pdf fetch send: {e}"),
        http_status: None,
    })?;
    let status = resp.status();
    if !status.is_success() {
        tracing::warn!(
            url,
            status = status.as_u16(),
            "research::pdf::fetch_pdf_text: non-success status"
        );
        return Err(Error::Provider {
            message: format!("pdf fetch status {status}"),
            http_status: Some(status.as_u16()),
        });
    }
    let bytes = resp.bytes().await.map_err(|e| Error::Provider {
        message: format!("pdf fetch body: {e}"),
        http_status: None,
    })?;
    if bytes.is_empty() {
        tracing::warn!(url, "research::pdf::fetch_pdf_text: empty body");
        return Err(Error::ResearchUnavailable("empty pdf response".into()));
    }

    let cap = max_bytes as usize;
    let input_slice: &[u8] = if bytes.len() > cap {
        // `bytes.len() > cap` is the truncation trigger. `cap`
        // is `u32` and `bytes.len()` is `usize`; on 32-bit
        // targets a `u32`-sized cap can still address the full
        // `usize` range so the slice is well-formed.
        tracing::trace!(
            bytes_len = bytes.len(),
            cap,
            "research::pdf::fetch_pdf_text: truncating input"
        );
        &bytes[..cap]
    } else {
        &bytes[..]
    };

    let mut text = extract_pdf_text(input_slice).await?;
    if text.len() > MAX_OUTPUT_BYTES {
        tracing::trace!(
            text_len = text.len(),
            max_output = MAX_OUTPUT_BYTES,
            "research::pdf::fetch_pdf_text: truncating output"
        );
        text.truncate(MAX_OUTPUT_BYTES);
        text.push_str("...(truncated)");
    }
    tracing::debug!(
        url,
        text_len = text.len(),
        "research::pdf::fetch_pdf_text: extracted"
    );
    Ok(text)
}

/// Byte-level entry point. Spawns `pdftotext -q -enc UTF-8 - -`,
/// pipes `bytes` into stdin, captures stdout, and returns the
/// extracted UTF-8 text. No network, no allowlist — a pure
/// subprocess wrapper.
///
/// `pub` (not `pub(crate)`) so the integration test in
/// `tests/integration_pdf.rs` can drive the round-trip end-to-end
/// against a fixture PDF without spinning up a wiremock server
/// that satisfies the allowlist filter. Production callers should
/// use [`fetch_pdf_text`] so the allowlist + byte-cap + transport
/// layers stay in one place.
pub async fn extract_pdf_text(bytes: &[u8]) -> Result<String> {
    extract_pdf_text_with_binary("pdftotext", bytes).await
}

/// Same as [`extract_pdf_text`] but the binary name is supplied by
/// the caller. Private so production code cannot accidentally
/// spawn a different `pdftotext` than the one the system ships;
/// the test suite uses it to exercise the "binary not found"
/// branch with a non-existent name (glibc's `execvp` falls back
/// to `/usr/bin:/bin` when `PATH` is empty, so an empty `PATH`
/// alone does not prevent `Command::new("pdftotext")` from
/// succeeding — only an unknown absolute or relative name does).
async fn extract_pdf_text_with_binary(binary: &str, bytes: &[u8]) -> Result<String> {
    tracing::trace!(
        binary,
        input_bytes = bytes.len(),
        "research::pdf::extract_pdf_text_with_binary: enter"
    );
    let mut child = Command::new(binary)
        .arg("-q")
        .arg("-enc")
        .arg("UTF-8")
        .arg("-")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                tracing::warn!(
                    binary,
                    "research::pdf::extract_pdf_text_with_binary: binary not found"
                );
                Error::ResearchUnavailable(
                    "pdftotext binary not found; install poppler-utils".into(),
                )
            } else {
                tracing::error!(
                    binary,
                    error = %e,
                    "research::pdf::extract_pdf_text_with_binary: spawn failed"
                );
                Error::Provider {
                    message: format!("pdftotext spawn: {e}"),
                    http_status: None,
                }
            }
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| Error::Provider {
        message: "pdftotext stdin missing".into(),
        http_status: None,
    })?;
    stdin.write_all(bytes).await.map_err(|e| Error::Provider {
        message: format!("pdftotext stdin write: {e}"),
        http_status: None,
    })?;
    drop(stdin);

    let stdout = child.stdout.take().ok_or_else(|| Error::Provider {
        message: "pdftotext stdout missing".into(),
        http_status: None,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| Error::Provider {
        message: "pdftotext stderr missing".into(),
        http_status: None,
    })?;

    // Read both pipes concurrently. The read caps are defensive:
    // stdout is bounded by MAX_OUTPUT_BYTES + a small slack for
    // poppler's trailing bytes; stderr is bounded to a single
    // poppler error line (the binary caps at ~512 bytes).
    let read_cap = (MAX_OUTPUT_BYTES as u64) + 16;
    let (stdout_bytes, stderr_bytes) = tokio::try_join!(
        read_capped(stdout, read_cap),
        read_capped(stderr, STDERR_CAP),
    )
    .map_err(|e| Error::Provider {
        message: format!("pdftotext read: {e}"),
        http_status: None,
    })?;

    let status = child.wait().await.map_err(|e| Error::Provider {
        message: format!("pdftotext wait: {e}"),
        http_status: None,
    })?;
    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&stderr_bytes);
        tracing::warn!(
            binary,
            code = status.code().unwrap_or(-1),
            stderr = %stderr_str,
            "research::pdf::extract_pdf_text_with_binary: pdftotext exited non-zero"
        );
        return Err(Error::Provider {
            message: format!(
                "pdftotext exit {}: {}",
                status.code().unwrap_or(-1),
                stderr_str
            ),
            http_status: None,
        });
    }

    let text = String::from_utf8_lossy(&stdout_bytes).into_owned();
    tracing::trace!(
        binary,
        text_len = text.len(),
        "research::pdf::extract_pdf_text_with_binary: extracted"
    );
    Ok(text)
}

/// Read up to `cap` bytes from `reader` into a `Vec`. Helper for
/// the [`tokio::try_join!`] in [`extract_pdf_text`].
async fn read_capped<R>(mut reader: R, cap: u64) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut limited = (&mut reader).take(cap);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `looks_like_pdf_url` is the gate the fetcher uses to
    /// decide whether to route a URL through the PDF parser.
    /// Pin every branch so a refactor that drops a case fails
    /// here before it lands in production.
    #[test]
    fn looks_like_pdf_url_matches_suffix() {
        assert!(looks_like_pdf_url("https://docs.rs/crate/crate-1.0.pdf"));
        assert!(looks_like_pdf_url("HTTP://EXAMPLE.COM/Whitepaper.PDF"));
        // Query string and fragment are stripped before the
        // suffix check.
        assert!(looks_like_pdf_url(
            "https://docs.rs/foo.pdf?download=true#page=2"
        ));
        // Negative paths.
        assert!(!looks_like_pdf_url("https://docs.rs/index.html"));
        assert!(!looks_like_pdf_url("https://docs.rs/"));
        assert!(!looks_like_pdf_url(""));
        // Suffix is the path's last segment, not a substring
        // anywhere in the URL.
        assert!(!looks_like_pdf_url("https://docs.rs/page.html?ref=.pdf"));
    }

    /// `binary_in_path` is the cheap pre-flight check used by
    /// the unit test for the "missing binary" path. With an
    /// empty PATH the result must be `false` regardless of the
    /// binary name — the test exercises the early-return branch
    /// and pins the PATH-iteration semantics at the same time.
    ///
    /// PATH is process-wide so the test serialises on
    /// [`crate::TEST_PATH_LOCK`] (declared below alongside the
    /// lock) — touching PATH concurrently with another test
    /// that depends on `Command::new` resolution would race and
    /// flake.
    #[test]
    fn binary_in_path_is_false_with_empty_path() {
        let _guard = crate::TEST_PATH_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os("PATH");
        // SAFETY: the lock above serialises against any other
        // test that touches PATH. The body sets, asserts, then
        // restores within the same critical section so no
        // observer sees a half-mutated environment.
        unsafe {
            std::env::set_var("PATH", "");
        }
        assert!(
            !binary_in_path("pdftotext"),
            "empty PATH must yield false for any binary"
        );
        // PATH is unset vs empty-set vs non-existent — both
        // should yield `false` because `var_os` returns `None`
        // only on a true unset, and `split_paths("")` is empty.
        unsafe {
            std::env::remove_var("PATH");
        }
        assert!(
            !binary_in_path("pdftotext"),
            "unset PATH must yield false (var_os returns None)"
        );
        match saved {
            Some(v) => unsafe {
                std::env::set_var("PATH", v);
            },
            None => unsafe {
                std::env::remove_var("PATH");
            },
        }
    }

    /// When the binary is on PATH and `Command::new` resolves
    /// it, `extract_pdf_text` returns the extracted text. The
    /// test feeds a deliberately malformed PDF and asserts on
    /// the *failure* mode instead — poppler exits non-zero on
    /// garbage input and the helper surfaces that as
    /// [`Error::Provider`]. The point of the test is to pin the
    /// spawn + wait plumbing end-to-end so a regression that
    /// drops the `stdin.take()` or the `wait()` call fails
    /// here, not in production.
    #[tokio::test]
    async fn extract_pdf_text_propagates_pdftotext_exit_code() {
        if !binary_in_path("pdftotext") {
            // Skip on hosts where poppler is not installed —
            // the test environment may not have it, but the
            // production target (Arch) always does.
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        // Garbage bytes — poppler will fail to parse and exit
        // non-zero. The fixture is irrelevant; the contract is
        // "non-zero exit → Provider error with stderr in the
        // message".
        let garbage: &[u8] = b"this is not a pdf at all";
        let err = extract_pdf_text(garbage)
            .await
            .expect_err("garbage input must surface as Provider error");
        assert!(
            matches!(err, Error::Provider { .. }),
            "non-zero pdftotext exit must classify as Provider, got {err:?}"
        );
        assert!(
            format!("{err}").contains("pdftotext"),
            "error message must mention the binary name, got {err:?}"
        );
    }

    /// When `Command::new(...)` cannot resolve the binary (name
    /// not on PATH and no absolute match), the spawn call fails
    /// with `io::ErrorKind::NotFound` and the helper maps that to
    /// [`Error::ResearchUnavailable`] with the install hint. The
    /// unit test for the missing-binary contract.
    ///
    /// We invoke the binary-name-injecting helper with a name
    /// that cannot possibly resolve — glibc's `execvp` falls back
    /// to `/usr/bin:/bin` when `PATH` is empty, so an empty `PATH`
    /// alone is insufficient. A clearly-fake name like
    /// `moagan_pdf_test_no_such_binary_xyz` guarantees `NotFound`
    /// on every host.
    #[tokio::test]
    async fn extract_pdf_text_reports_missing_binary_as_research_unavailable() {
        let result =
            extract_pdf_text_with_binary("moagan_pdf_test_no_such_binary_xyz", b"%PDF-1.4\n").await;
        let err = result.expect_err("unknown binary must surface as ResearchUnavailable");
        match &err {
            Error::ResearchUnavailable(msg) => {
                assert!(
                    msg.contains("pdftotext"),
                    "missing-binary message must name the binary, got {msg:?}"
                );
                assert!(
                    msg.contains("poppler-utils"),
                    "missing-binary message must hint at poppler-utils, got {msg:?}"
                );
            }
            other => panic!("expected ResearchUnavailable, got {other:?}"),
        }
    }

    /// The `fetch_pdf_text` URL pre-flight must reject a
    /// non-allowlisted host with [`Error::InvalidArgs`] before
    /// any HTTP traffic. This is the defense-in-depth check —
    /// the upstream `fetcher` already filters, but the pdf
    /// module re-checks so a future caller that bypasses the
    /// fetcher cannot exfiltrate to a random host.
    #[tokio::test]
    async fn fetch_pdf_text_rejects_non_allowlisted_host() {
        let err = fetch_pdf_text(
            "https://evil.example.com/whitepaper.pdf",
            DEFAULT_MAX_INPUT_BYTES,
        )
        .await
        .expect_err("non-allowlisted host must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "non-allowlisted host must classify as InvalidArgs, got {err:?}"
        );
    }

    /// The URL parser must surface a malformed URL as
    /// [`Error::InvalidArgs`] rather than panic. Symmetric with
    /// the existing `fetch_one_rejects_malformed_url` test in
    /// the fetcher so the contract is consistent across both
    /// entry points.
    #[tokio::test]
    async fn fetch_pdf_text_rejects_malformed_url() {
        let err = fetch_pdf_text("not a url at all", DEFAULT_MAX_INPUT_BYTES)
            .await
            .expect_err("malformed url must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "malformed url must classify as InvalidArgs, got {err:?}"
        );
    }
}
