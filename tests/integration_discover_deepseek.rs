//! End-to-end discovery validation against the native `deepseek`
//! provider (PR #462; companion to the opencode_go close-out in
//! `tests/integration_discover_opencode_go.rs`).
//!
//! `#[ignore]`d by default; only runs locally / in `e2e-network`
//! when the operator's `DEEPSEEK_API_KEY` is exported. With no
//! key the test returns `Ok(())` immediately so a CI matrix without
//! the secret stays green. Run with:
//!
//! ```bash
//! DEEPSEEK_API_KEY=sk-... cargo test --test integration_discover_deepseek -- --ignored
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
#[ignore = "requires DEEPSEEK_API_KEY; run with --ignored"]
fn discover_deepseek_writes_four_subdirs() {
    if std::env::var_os("DEEPSEEK_API_KEY").is_none() {
        eprintln!("skipping: DEEPSEEK_API_KEY not set");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(binary())
        // Disable the per-provider `max_tokens_auto` probe so the
        // 19-step exponential search (up to 2^19 = 524_288 against
        // DEEPSEEK_MAX_TOKENS_CAP = 393_216) does not race the
        // 80-sketch matrix fan-out. The probe is background-only by
        // design, but on a fresh CI runner with no cached
        // `max_tokens_auto.toml` the upstream probe timeouts
        // (5 s × ~19 steps) compound with the matrix + post-matrix
        // LLM calls (Tagger + Cluster + FacetDeriver + Extractor +
        // Integrator) and push the run past the 15-min
        // `test-ignored` job ceiling (PR #473 §14). The wire body
        // still clamps to `DEEPSEEK_MAX_TOKENS_CAP` via
        // `DeepSeekProvider::effective_max_tokens`, so skipping the
        // probe does not regress the HTTP-400 fix from commit
        // `c3dd03e`.
        .env("MOAGAN_MAX_TOKEN_AUTO", "0")
        .args([
            "discover",
            "--provider",
            "deepseek",
            "--prompt",
            PROMPT,
            "--cardinality",
            "80",
            "--dimensions",
            "2",
            "--facets-per-dimension",
            "2",
            "--max-parallelism",
            "2",
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
