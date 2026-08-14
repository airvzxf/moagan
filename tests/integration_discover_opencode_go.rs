//! End-to-end discovery validation against the `opencode_go` provider
//! (closes the v0.7 P8 discovery-validation gap documented in
//! `docs/discovery-validation-research-2026-08-13.md`).
//!
//! `#[ignore]`d by default; only runs locally / in `e2e-network`
//! when the operator's `OPENCODE_GO_API_KEY` is exported. With no
//! key the test returns `Ok(())` immediately so a CI matrix without
//! the secret stays green. Run with:
//!
//! ```bash
//! OPENCODE_GO_API_KEY=sk-... cargo test --test integration_discover_opencode_go -- --ignored
//! ```
//!
//! The validation asserts the four sub-directories produced by the
//! distinct discover_* LLM roles (V4 §6.5–§6.10) are non-empty:
//! `tags/` (Tagger), `facets/` (FacetDeriver),
//! `extractions/cat_*` (Extractor), `drafts/` (Integrator). The
//! 2×2 matrix keeps fan-out small (~80 sketches) so the run stays
//! under the 600 s default test timeout.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PROMPT: &str = "Compare three Rust HTTP clients for binary streaming";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_moagan"))
}

#[test]
#[ignore = "requires OPENCODE_GO_API_KEY; run with --ignored"]
fn discover_opencode_go_writes_four_subdirs() {
    if std::env::var_os("OPENCODE_GO_API_KEY").is_none() {
        eprintln!("skipping: OPENCODE_GO_API_KEY not set");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(binary())
        // Disable the per-provider `max_tokens_auto` probe so the
        // 14-step exponential search (up to 2^14 = 16_384 against
        // OPENCODE_GO_MAX_TOKENS_CAP = 16_384) does not race the
        // 80-sketch matrix fan-out. Same rationale as
        // `tests/integration_discover_deepseek.rs`: the probe is
        // background-only by design, but the upstream probe
        // timeouts (5 s × ~14 steps) compound with the matrix +
        // post-matrix LLM calls and push the run past the 15-min
        // `test-ignored` job ceiling (PR #473 §14). The wire body
        // still clamps to `OPENCODE_GO_MAX_TOKENS_CAP` via the
        // routed provider's `effective_max_tokens`, so skipping the
        // probe does not regress the HTTP-400 fix from commit
        // `c3dd03e`.
        .env("MOAGAN_MAX_TOKEN_AUTO", "0")
        // `--max-parallelism 8` (was 2): same reasoning as the
        // deepseek sibling test. The 80-sketch + 7-phase post-
        // matrix workload is identical and at parallelism=2 each
        // sequential round-trip sums past 15 min on a CI runner.
        .args([
            "discover",
            "--provider",
            "opencode_go",
            "--prompt",
            PROMPT,
            "--cardinality",
            "80",
            "--dimensions",
            "2",
            "--facets-per-dimension",
            "2",
            "--max-parallelism",
            "8",
            "--non-interactive",
            "--runs-dir",
        ])
        .arg(tmp.path())
        .output()
        .expect("spawn moagan discover");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run_id = fs::read_dir(tmp.path().join(".runs"))
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .expect("at least one run dir");
    let run_dir = run_id.path();
    for sub in ["tags", "facets", "drafts"] {
        let count = fs::read_dir(run_dir.join(sub))
            .map(|d| d.count())
            .unwrap_or(0);
        assert!(count >= 1, "{sub}/ should have ≥1 entry, got {count}");
    }
    let cats: usize = fs::read_dir(run_dir.join("extractions"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("cat_"))
                .count()
        })
        .unwrap_or(0);
    assert!(
        cats >= 1,
        "extractions/cat_* should have ≥1 entry, got {cats}"
    );
}
