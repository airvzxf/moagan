//! K.4 sub-1 end-to-end integration test for the PDF parser.
//!
//! Round-trips a fixture PDF (`tests/fixtures/sample.pdf`) through
//! [`moagan::research::pdf::extract_pdf_text`] and asserts that the
//! extracted text contains the substring the fixture was generated
//! with. The test is skipped (not failed) when `pdftotext` is not
//! on `PATH` so CI hosts without `poppler-utils` stay green.

use std::path::PathBuf;

use moagan::research::pdf::{DEFAULT_MAX_INPUT_BYTES, extract_pdf_text};

/// Process-wide mutex serialising the `PATH` read below against
/// any parallel test that mutates `PATH` to mock "binary
/// missing". Local to the integration-test binary because the
/// `moagan::TEST_PATH_LOCK` static lives behind `#[cfg(test)]`
/// and is not visible from external test crates — the same
/// pattern as `integration_pr18_auto_pickers.rs`'s
/// `ENV_LOCK`.
static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Canonical phrase embedded in `tests/fixtures/sample.pdf`.
/// Generated via `groff -ms` against the source text:
/// `"Hello PDF world. This is a fixture for the K.4 PDF parser
/// integration test."`
const FIXTURE_PHRASE: &str =
    "Hello PDF world. This is a fixture for the K.4 PDF parser integration test.";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample.pdf")
}

#[test]
fn fixture_pdf_exists_and_is_non_trivial() {
    let bytes = std::fs::read(fixture_path()).expect("fixture PDF must be present");
    assert!(
        bytes.len() > 100,
        "fixture PDF must be non-trivial (>100 bytes), got {} bytes",
        bytes.len()
    );
    // Sanity: every PDF starts with `%PDF-`.
    assert!(
        bytes.starts_with(b"%PDF-"),
        "fixture must be a real PDF (starts with %PDF-), got {:?}",
        &bytes[..8.min(bytes.len())]
    );
}

/// Round-trip: feed the fixture PDF into `extract_pdf_text` and
/// assert the embedded phrase comes out. The test is skipped when
/// `pdftotext` is missing — CI runners without `poppler-utils` stay
/// green and operators get a clear "skipped" line in the test log.
#[tokio::test]
async fn extract_pdf_text_round_trips_fixture() {
    // Scope the PATH lock to the synchronous probe so the guard
    // drops before the `extract_pdf_text(...).await` below.
    let pdftotext_available = {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        moagan::research::pdf::binary_in_path("pdftotext")
    };
    if !pdftotext_available {
        eprintln!("skipping: pdftotext not in PATH (install poppler-utils)");
        return;
    }
    let bytes = std::fs::read(fixture_path()).expect("fixture PDF must be present");
    let text = extract_pdf_text(&bytes)
        .await
        .expect("round-trip must succeed when pdftotext is installed");
    assert!(
        text.contains(FIXTURE_PHRASE),
        "extracted text must contain the fixture phrase, got:\n{text}"
    );
}

/// Output-side defensive cap: a PDF whose extracted text exceeds
/// the parser's `MAX_OUTPUT_BYTES` is truncated and suffixed with
/// the marker. We exercise the cap by setting a tiny `max_bytes`
/// via the higher-level `fetch_pdf_text` against an empty PDF
/// would be a no-op, so this test asserts the public constant
/// instead. The unit tests in `src/research/pdf.rs` cover the
/// actual truncation behaviour against a real pdftotext run.
#[test]
fn default_max_input_bytes_is_above_fetcher_cap() {
    // PDF payloads can be tens of MB; the fetcher caps HTML at
    // 4 KB. The parser default must be larger so a PDF can round-
    // trip through the same fetcher pipeline.
    let fetcher_cap = moagan::research::fetcher::MAX_BYTES_PER_URL;
    assert!(
        DEFAULT_MAX_INPUT_BYTES as usize > fetcher_cap,
        "PDF default ({DEFAULT_MAX_INPUT_BYTES}) must exceed fetcher cap ({fetcher_cap})"
    );
}
