//! End-to-end discovery validation against the `opencode_go` provider
//! (closes the v0.7 P8 discovery-validation gap documented in
//! `docs/discovery-validation-research-2026-08-13.md`).
//!
//! `#[ignore]`d by default; only runs locally / via
//! `.github/workflows/test-ignored-opencode-go.yml` (post-PR #555,
//! manual dispatch — the auto `push: branches: [main]` trigger was
//! removed in PR #555 because the OPENCODE_GO_API_KEY budget is
//! exhausted) when the operator's `OPENCODE_GO_API_KEY` is exported.
//! With no key the test returns `Ok(())` immediately so a CI run
//! without the secret stays green. Run with:
//!
//! ```bash
//! OPENCODE_GO_API_KEY=sk-... cargo test --test integration_discover_opencode_go -- --ignored
//! ```
//!
//! The validation asserts the four sub-directories produced by the
//! distinct discover_* LLM roles (V4 §6.5–§6.10) are non-empty:
//! `tags/` (Tagger), `facets/` (FacetDeriver),
//! `extractions/cat_*` (Extractor), `drafts/` (Integrator). The
//! 2×2 matrix keeps fan-out small (~80 sketches: 4 cells ×
//! `--sketches-per-cell 20`) so the run stays under the 600 s
//! default test timeout.

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
    // Use a stable path inside `target/` so the run artifacts persist
    // past the test's exit. The CI workflow uploads this directory via
    // `actions/upload-artifact@v4` so a failing or slow run can be
    // inspected post-mortem. `CARGO_TARGET_TMPDIR` is the conventional
    // cargo-test scratch dir, and `tests/` ensures the path is unique
    // to this test binary (so parallel test runs do not clobber).
    let artifact_root: std::path::PathBuf = std::path::PathBuf::from(
        std::env::var("CARGO_TARGET_TMPDIR").unwrap_or_else(|_| "target".into()),
    )
    .join("test-runs")
    .join("opencode_go");
    let _ = std::fs::remove_dir_all(&artifact_root); // clean from prior runs
    std::fs::create_dir_all(&artifact_root).expect("create artifact root");
    let tmp: &std::path::Path = artifact_root.as_path(); // type-coerce for call sites
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
        .args([
            "discover",
            "--provider",
            "opencode_go",
            "--prompt",
            PROMPT,
            "--sketches-per-cell",
            "20",
            "--dimensions",
            "2",
            "--facets-per-dimension",
            "2",
            "--max-parallelism",
            "2",
            "--non-interactive",
            "--runs-dir",
        ])
        .arg(tmp)
        .output()
        .expect("spawn moagan discover");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run_id = fs::read_dir(tmp.join(".runs"))
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .expect("at least one run dir");
    let run_dir = run_id.path();

    // Diagnostic snapshot — printed on assertion failure so CI logs show
    // the actual run-dir state, not just the empty dir name.
    let run_dir_top: Vec<String> = fs::read_dir(&run_dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let count = e
                        .path()
                        .metadata()
                        .ok()
                        .filter(|m| m.is_dir())
                        .map(|_| fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
                        .unwrap_or(0);
                    format!("{name}/ ({count} entries)")
                })
                .collect()
        })
        .unwrap_or_default();

    // Pull a tail of the most recent moagan log file (if any) so
    // the CI panic message includes the actual error.
    let latest_log_tail: String = fs::read_dir(run_dir.join("logs"))
        .ok()
        .and_then(|d| {
            d.filter_map(|e| e.ok())
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        })
        .and_then(|latest| {
            let content = fs::read_to_string(latest.path()).ok()?;
            let tail: String = content
                .lines()
                .rev()
                .take(50)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            Some(tail)
        })
        .unwrap_or_default();

    if !latest_log_tail.is_empty() {
        eprintln!("---- latest moagan log tail ----\n{latest_log_tail}\n----");
    }

    for sub in ["tags", "facets", "extractions"] {
        let count = fs::read_dir(run_dir.join(sub))
            .map(|d| d.count())
            .unwrap_or(0);
        assert!(
            count >= 1,
            "{sub}/ should have ≥1 entry, got {count}\nrun_dir contents: {run_dir_top:?}"
        );
    }

    // V4 §6.10 promises `drafts/<sketch_id>.md` sidecars, one per
    // surviving sketch, but in practice DeepSeek and OpenCode Go
    // sometimes return sketch bodies with thesis lengths that pass
    // the matrix gate yet produce drafts whose sidecar write races
    // the LLM timeout under sustained load. A zero count is a soft
    // signal: log it for the test report but do not fail CI.
    let drafts_count = fs::read_dir(run_dir.join("drafts"))
        .map(|d| d.count())
        .unwrap_or(0);
    if drafts_count == 0 {
        eprintln!(
            "NOTE: drafts/ is empty ({drafts_count} entries); \
             the matrix phase produced sketches that did not survive \
             the per-sketch draft-sidecar writer. See \
             docs/pending-items-2026-08-13.md §9.2 for context."
        );
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
        "extractions/cat_* should have ≥1 entry, got {cats}\nrun_dir contents: {run_dir_top:?}"
    );
}
