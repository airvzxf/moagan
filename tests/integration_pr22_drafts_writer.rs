//! PR-22 integration test: verify the `drafts/` writer.
//!
//! Spec reference: V4 §6.10 — `drafts/<sketch_id>.md` per successful
//! sketch. The roadmap (PR-22) and the catalog (D.34 follow-up) call
//! for a per-sketch human-readable draft sidecar under `.runs/<id>/drafts/`
//! carrying the sketch text plus the LLM metadata (model, temperature,
//! role) so the discovery inspector / dashboard can render the raw
//! model output without re-parsing the JSON under `sketches/`.
//!
//! Before this PR the `drafts/` directory was created by
//! `RunDir::ensure()` (see `src/fs_layout.rs`) but no phase wrote into
//! it — the spec's "drafts/cat_NN/borrador.md" path also lives there,
//! so closing the gap means future per-cluster drafts can drop their
//! files alongside without colliding.
//!
//! What we lock down:
//!
//! 1. Running `DiscoverMatrixPhase` with `cardinality = 8` produces
//!    exactly 8 files in `.runs/<id>/drafts/` — one per sketch.
//! 2. Each draft's filename is `<sketch_id>.md` where `sketch_id`
//!    matches the corresponding `sketches/<sketch_id>.json`.
//! 3. Each draft's body contains:
//!    - YAML-style frontmatter with `id`, `model`, `role`, `temperature`
//!    - The sketch's `thesis` text verbatim
//!    - A `# <sketch_id>` heading so the file is readable in any
//!      markdown viewer
//! 4. The metadata recorded in the frontmatter matches the run
//!    (`mock-model`, role `sketch`, temperature `1.0` per the
//!    `temperature_for_role` table).
//!
//! The test deliberately drives `DiscoverMatrixPhase` directly (no
//! CLI) so it stays sub-second and independent of `moagan discover`'s
//! minimum-cardinality gate.

// The env mutex is intentionally held across `await` points so
// two test threads cannot both flip `MOAGAN_HOME` mid-flight.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use moagan::execution::Parallelism;
use moagan::fs_layout::MoaganHome;
use moagan::ids::RunId;
use moagan::llm::{MockProvider, MockResponse, ProviderRegistry};
use moagan::phases::{DiscoverMatrixPhase, Phase, PhaseOutput, RunContext};
use moagan::redact::RedactPolicy;
use moagan::telemetry::Telemetry;

/// Process-wide mutex that serialises every test which mutates
/// the `MOAGAN_HOME` env var. Mirrors the pattern used by the
/// other PR-XX integration tests.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Minimal sketch JSON the mock serves. Thesis length clears the
/// 30-char minimum-thesis gate. The mock cycles through one valid
/// response per cell, so each sketch has a unique `id` matched by
/// its filename in both `sketches/` and `drafts/`.
fn sketch_json_for(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "thesis": "Use Rust and SQLite for a single binary backend with strong typing and a robust test suite for the {id} cell.",
  "key_decisions": ["single binary", "embedded sqlite", "async runtime"],
  "architecture_outline": "The CLI binary owns the database, the cache, and the agent registry; each sketch is a distinct cell in the matrix.",
  "assumptions": ["users are comfortable with one process per run"],
  "strengths": ["simple deployment", "easy to test"],
  "weaknesses": ["no horizontal scaling"],
  "hard_constraint_check": {{"single_binary": true}},
  "expected_validation": "Build a 1k-line Rust crate that compiles in <2s.",
  "angle": "minimalist"
}}"#
    )
}

fn build_matrix_mock(cells: usize, per_cell: usize) -> Arc<MockProvider> {
    let mut p = MockProvider::empty();
    for n in 0..(cells * per_cell) {
        p.push(MockResponse::plain(sketch_json_for(&format!("sk_{n:04}"))));
    }
    p.set_cycle(false);
    Arc::new(p)
}

fn build_brief(run_dir: &moagan::fs_layout::RunDir<'_>) -> moagan::error::Result<()> {
    let brief = serde_json::json!({
        "problem": "Design a multi-tenant SaaS backend",
        "objectives": ["Implement auth", "Implement storage"],
        "deliverables": ["Architecture doc"],
        "constraints": ["Single Rust binary"],
        "assumptions": ["Postgres available"],
        "non_goals": ["Frontend"],
        "acceptance": ["Sketch coverage"],
        "risks": ["Concurrency"],
    });
    std::fs::write(run_dir.brief(), serde_json::to_vec_pretty(&brief).unwrap())?;
    Ok(())
}

fn build_run_context(
    home: Arc<MoaganHome>,
    provider: Arc<MockProvider>,
    run_id: RunId,
) -> RunContext {
    let mut registry = ProviderRegistry::default();
    let arc: Arc<dyn moagan::llm::Provider> = provider.clone();
    registry.insert("mock".into(), arc);
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().expect("ensure run dir");
    let telemetry =
        Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None).expect("open telemetry");
    let parallelism = Parallelism::new(2);
    RunContext::new(
        run_id,
        home,
        Arc::new(registry),
        "mock".into(),
        "mock-model".into(),
        parallelism,
        telemetry,
        "Design a multi-tenant SaaS backend".into(),
        "discover".into(),
    )
}

/// Pull every `.md` filename in `drafts/`. Skips the hidden
/// `.meta.json` sidecar that the `AtomicWriter` would have left
/// here (the draft writer deliberately uses `std::fs::write`
/// instead, so the listing should normally contain only `.md`
/// files — but the filter stays defensive in case a future
/// refactor reintroduces the sidecar).
fn list_draft_paths(drafts_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(drafts_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn drafts_dir_contains_one_md_per_successful_sketch() {
    // Spec verification (PR-22): a 4-cell × 2-per-cell = 8 sketch
    // matrix produces exactly 8 draft files under `drafts/`.
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let matrix = DiscoverMatrixPhase::from_dimensions(2, 2, 8);
    let mock = build_matrix_mock(matrix.matrix.cells(), matrix.matrix.sketches_per_cell);
    let ctx = build_run_context(home.clone(), mock, run_id);

    let outcome = matrix.execute(&ctx).await.expect("matrix phase runs");
    let PhaseOutput::Sketches(paths) = outcome else {
        panic!("expected PhaseOutput::Sketches");
    };
    assert_eq!(
        paths.len(),
        8,
        "8-slot matrix must produce 8 sketches; got {}",
        paths.len()
    );

    // Pin the contract: drafts/ has exactly one file per sketch,
    // and every draft filename matches the surviving sketch's id.
    let drafts_dir = run_dir.drafts();
    let draft_paths = list_draft_paths(&drafts_dir);
    assert_eq!(
        draft_paths.len(),
        8,
        "drafts/ must hold 8 markdown files; got {}. listing: {:#?}",
        draft_paths.len(),
        draft_paths
    );

    // Filename ↔ id roundtrip: extract the basename stem from each
    // draft path and assert it matches the `id` field of the
    // corresponding JSON sketch file. The matrix phase assigns
    // ids `sk_0000` through `sk_0007`; the draft writer uses the
    // same id verbatim.
    let mut stem_to_sketch_id: Vec<(String, String)> = Vec::new();
    for draft_path in &draft_paths {
        let stem = draft_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let json_path = run_dir.sketches().join(format!("{stem}.json"));
        assert!(
            json_path.exists(),
            "draft {stem}.md exists but matching {stem}.json does not"
        );
        let sketch: moagan::domain::Sketch =
            moagan::phases::util::read_json(&json_path).expect("sketch json");
        stem_to_sketch_id.push((stem, sketch.id.clone()));
    }
    stem_to_sketch_id.sort_by(|a, b| a.0.cmp(&b.0));
    let stems: Vec<String> = stem_to_sketch_id.iter().map(|(s, _)| s.clone()).collect();
    assert_eq!(
        stems,
        vec![
            "sk_0000".to_string(),
            "sk_0001".to_string(),
            "sk_0002".to_string(),
            "sk_0003".to_string(),
            "sk_0004".to_string(),
            "sk_0005".to_string(),
            "sk_0006".to_string(),
            "sk_0007".to_string(),
        ],
        "draft filenames must be sk_NNNN.md in fan-out order; got {stems:?}"
    );
    // And every draft's stem matches its sketch's id (the writer
    // uses `sketch.id` directly so this is the same string by
    // construction, but the assertion locks it down so a future
    // rename of one trips the test).
    for (stem, sketch_id) in &stem_to_sketch_id {
        assert_eq!(stem, sketch_id, "draft stem must equal sketch id");
    }
}

#[tokio::test]
async fn draft_body_carries_frontmatter_and_thesis() {
    // Pin the wire format: the frontmatter carries `model`,
    // `role`, `temperature`, the body carries the sketch's
    // `thesis` verbatim, and the `# <sketch_id>` heading sits at
    // the top so the file is greppable.
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let matrix = DiscoverMatrixPhase::from_dimensions(2, 2, 8);
    let mock = build_matrix_mock(matrix.matrix.cells(), matrix.matrix.sketches_per_cell);
    let ctx = build_run_context(home.clone(), mock, run_id);
    matrix.execute(&ctx).await.expect("matrix phase runs");

    // Pick the first draft alphabetically and assert every
    // contract point on it.
    let drafts_dir = run_dir.drafts();
    let first = list_draft_paths(&drafts_dir)
        .into_iter()
        .next()
        .expect("at least one draft");
    let body = std::fs::read_to_string(&first).expect("draft body");

    // Frontmatter block: every metadata field must be present
    // and well-formed (key: value, no JSON quoting).
    assert!(
        body.starts_with("---\n"),
        "draft must open with YAML frontmatter delimiter; got: {body}"
    );
    let frontmatter_end = body.find("\n---\n").expect("frontmatter closing delimiter");
    let frontmatter = &body[..frontmatter_end];
    assert!(
        frontmatter.contains("id: sk_0000"),
        "frontmatter missing `id: sk_0000`: {frontmatter}"
    );
    assert!(
        frontmatter.contains("model: mock-model"),
        "frontmatter missing `model: mock-model`: {frontmatter}"
    );
    assert!(
        frontmatter.contains("role: sketch"),
        "frontmatter missing `role: sketch`: {frontmatter}"
    );
    assert!(
        frontmatter.contains("temperature: 1.0"),
        "frontmatter must record Role::Sketch's T=1.0; got: {frontmatter}"
    );
    assert!(
        frontmatter.contains("written_at_unix: "),
        "frontmatter must record the write timestamp: {frontmatter}"
    );

    // Body: heading + thesis section.
    assert!(
        body.contains("\n# sk_0000\n"),
        "draft must carry a `# sk_0000` heading; got: {body}"
    );
    assert!(
        body.contains("## Thesis"),
        "draft must carry a `## Thesis` section: {body}"
    );
    let expected_thesis = "Use Rust and SQLite for a single binary backend with strong typing and a robust test suite for the sk_0000 cell.";
    assert!(
        body.contains(expected_thesis),
        "draft body must embed the sketch's `thesis` verbatim; got: {body}"
    );
    assert!(
        body.contains("## Key decisions"),
        "draft must carry a `## Key decisions` section: {body}"
    );
    assert!(
        body.contains("- single binary"),
        "draft must list every key_decision as a bullet: {body}"
    );
    assert!(
        body.contains("## Architecture outline"),
        "draft must carry a `## Architecture outline` section: {body}"
    );
}

#[tokio::test]
async fn draft_count_matches_sketch_count_when_partial_failure() {
    // When one of the eight sketches returns a malformed response,
    // the retry helper burns 3 attempts and the row gets
    // dropped (D.34.1). The surviving count drops below the
    // matrix cardinality, and `drafts/` must hold exactly the
    // number of drafts that landed in `sketches/` — i.e. one
    // draft per surviving sketch, never one per attempt.
    let _guard = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = Arc::new(MoaganHome::resolve().unwrap());
    home.ensure().unwrap();
    let run_id = RunId::new();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure().unwrap();
    build_brief(&run_dir).unwrap();

    let matrix = DiscoverMatrixPhase::from_dimensions(2, 2, 8);
    let _total = matrix.matrix.cells() * matrix.matrix.sketches_per_cell;
    let mut p = MockProvider::empty();
    // 6 valid responses + 2 broken-JSON-on-every-attempt slots.
    // The retry helper consumes 3 mock calls per broken slot,
    // so the broken slots emit "not-json-at-all" three times
    // each and then fail with MockExhausted. Each broken
    // slot costs the phase 1 dropped sketch, not 1 surviving
    // one.
    for n in 0..6 {
        p.push(MockResponse::plain(sketch_json_for(&format!("sk_{n:04}"))));
    }
    for _ in 0..6 {
        p.push(MockResponse::plain("not-json-at-all"));
    }
    p.set_cycle(false);
    let mock = Arc::new(p);

    let ctx = build_run_context(home.clone(), mock, run_id);
    let outcome = matrix.execute(&ctx).await.expect("matrix phase runs");
    let PhaseOutput::Sketches(paths) = outcome else {
        panic!("expected PhaseOutput::Sketches");
    };
    assert_eq!(
        paths.len(),
        6,
        "6 valid + 2 broken slots must yield 6 surviving sketches; got {}",
        paths.len()
    );

    let drafts = list_draft_paths(&run_dir.drafts());
    assert_eq!(
        drafts.len(),
        paths.len(),
        "draft count must equal surviving sketch count (one draft per successful sketch); got {} drafts vs {} sketches",
        drafts.len(),
        paths.len()
    );
    // Every surviving sketch must have a draft with the same id.
    for sketch_path in &paths {
        let id = sketch_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let draft = run_dir.drafts().join(format!("{id}.md"));
        assert!(draft.exists(), "missing draft for sketch {id}: {:?}", draft);
    }
}

#[test]
fn drafts_dir_helper_returns_correct_subdir() {
    // Static check: the `drafts()` path helper returns
    // `<run_dir>/drafts` so the writer's `create_dir_all` lands
    // in the same place the rest of the run reads from. Pin
    // the helper so a future rename of the helper does not
    // silently leave the writer writing to a different path.
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MOAGAN_HOME", tmp.path());
    }
    let home = MoaganHome::resolve().unwrap();
    let r = home.run_dir(RunId::new());
    assert!(r.drafts().ends_with("drafts"));
}
