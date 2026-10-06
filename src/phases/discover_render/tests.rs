use super::*;

use crate::discovery::matrix::{
    DISCOVERY_DIMENSIONS_SCHEMA_VERSION, Dimension, DimensionFacetDescription, Facet,
};
use crate::phases::util::write_json;

const RUN: &str = "01a0-test-run";

fn facet(id: &str, label: &str) -> Facet {
    Facet {
        id: id.into(),
        label: label.into(),
    }
}

fn matrix(facets: Vec<Facet>) -> ExplorationMatrix {
    ExplorationMatrix::new(
        vec![Dimension {
            id: "auth".into(),
            label: "Auth".into(),
            facets,
        }],
        3,
    )
}

fn sketch_json(id: &str, angle: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "thesis": format!("Thesis {id}"),
        "angle": angle,
        "hard_constraint_check": {"C1": true}
    })
}

/// A run dir named `RUN` with a two-cell matrix, a brief, three
/// sketches and one `.meta.json` seal that must not count as a sketch.
fn run_dir(root: &Path) -> PathBuf {
    let dir = root.join(RUN);
    let sketches = dir.join("sketches");
    std::fs::create_dir_all(&sketches).unwrap();
    write_json(
        &dir.join("exploration_matrix.json"),
        &matrix(vec![facet("oauth", "OAuth"), facet("api-key", "API key")]),
    )
    .unwrap();
    std::fs::write(
        dir.join("brief.json"),
        r#"{"problem":"Pick an auth scheme.","constraints":["No passwords"]}"#,
    )
    .unwrap();
    for (id, angle) in [
        ("sk_0001", "auth:oauth"),
        ("sk_0002", "auth:api-key"),
        ("sk_0003", "auth:oauth"),
    ] {
        std::fs::write(
            sketches.join(format!("{id}.json")),
            sketch_json(id, angle).to_string(),
        )
        .unwrap();
    }
    std::fs::write(
        sketches.join("sk_0001.json.meta.json"),
        r#"{"sha256":"00","bytes":10}"#,
    )
    .unwrap();
    dir
}

fn text(files: &[(PathBuf, String)], path: &str) -> String {
    files
        .iter()
        .find(|(p, _)| p == Path::new(path))
        .map(|(_, t)| t.clone())
        .unwrap_or_else(|| panic!("no rendered file {path}"))
}

#[test]
fn the_phase_is_named_discover_render() {
    assert_eq!(DiscoverRenderPhase.name(), "discover_render");
}

#[test]
fn a_run_dir_renders_every_sketch_and_ignores_meta_seals() {
    let tmp = tempfile::tempdir().unwrap();
    let files = render_run_dir(&run_dir(tmp.path())).unwrap();
    let readme = text(&files, "README.md");
    assert!(readme.contains("- Theses in this catalogue: 3 of 3 sketches (100.0 %)\n"));
    assert!(readme.contains("\nPick an auth scheme.\n"));
    let oauth = text(&files, "auth/oauth.md");
    assert!(oauth.contains("\n### sk_0001\n") && oauth.contains("\n### sk_0003\n"));
    assert!(text(&files, "auth/api-key.md").contains("\n### sk_0002\n"));
}

#[test]
fn the_run_id_shown_is_the_run_dir_name() {
    let tmp = tempfile::tempdir().unwrap();
    let files = render_run_dir(&run_dir(tmp.path())).unwrap();
    assert!(text(&files, "README.md").contains("\nRun `01a0-test-run`\n"));
    assert!(text(&files, "catalog.json").contains("\"run_id\": \"01a0-test-run\""));
}

#[test]
fn facet_descriptions_come_from_the_dimensions_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    let sidecar = DiscoveryDimensions {
        schema_version: DISCOVERY_DIMENSIONS_SCHEMA_VERSION.into(),
        brief_hash: "hash".into(),
        dimensions: Vec::new(),
        descriptions: vec![DimensionFacetDescription::new(
            "auth",
            "api-key",
            "A static key per client.",
        )],
        created_unix: 0,
    };
    write_json(&dir.join(DISCOVERY_DIMENSIONS_FILENAME), &sidecar).unwrap();
    let files = render_run_dir(&dir).unwrap();
    assert!(
        text(&files, "auth/api-key.md")
            .starts_with("# Auth → API key\n\nA static key per client.\n\n1 thesis.\n")
    );
}

#[test]
fn a_sketch_without_id_takes_its_file_stem() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    std::fs::write(
        dir.join("sketches").join("sketch_068.json"),
        sketch_json("", "auth:oauth").to_string(),
    )
    .unwrap();
    let files = render_run_dir(&dir).unwrap();
    assert!(text(&files, "auth/oauth.md").contains("\n### sketch_068\n"));
}

#[test]
fn a_run_dir_without_sketches_renders_an_empty_catalogue() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    std::fs::remove_dir_all(dir.join("sketches")).unwrap();
    let files = render_run_dir(&dir).unwrap();
    assert!(text(&files, "README.md").contains("- Theses in this catalogue: 0 of 0 sketches"));
}

#[test]
fn a_run_dir_without_brief_renders_the_problem_placeholder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    std::fs::remove_file(dir.join("brief.json")).unwrap();
    let files = render_run_dir(&dir).unwrap();
    assert!(text(&files, "README.md").contains("\n_No problem statement in brief.json._\n"));
}

#[test]
fn a_run_dir_without_matrix_is_an_invalid_state() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    std::fs::remove_file(dir.join("exploration_matrix.json")).unwrap();
    let err = render_run_dir(&dir).unwrap_err();
    assert!(matches!(err, Error::InvalidState(_)), "{err}");
    assert!(err.to_string().contains("exploration_matrix.json"), "{err}");
}

#[test]
fn two_sketch_files_with_the_same_id_fail_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    std::fs::write(
        dir.join("sketches").join("copy.json"),
        sketch_json("sk_0002", "auth:oauth").to_string(),
    )
    .unwrap();
    let err = render_run_dir(&dir).unwrap_err();
    assert!(matches!(err, Error::InvalidState(_)), "{err}");
    assert!(
        err.to_string()
            .contains("sketch sk_0002 appears 2 times in the catalogue"),
        "{err}"
    );
}

#[test]
fn colliding_facet_files_fail_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    write_json(
        &dir.join("exploration_matrix.json"),
        &matrix(vec![facet("api key", "A"), facet("api-key", "B")]),
    )
    .unwrap();
    let err = render_run_dir(&dir).unwrap_err();
    assert!(matches!(err, Error::InvalidState(_)), "{err}");
    assert!(err.to_string().contains("auth/api-key.md"), "{err}");
}

#[test]
fn write_catalog_writes_every_file_under_final_and_returns_the_readme() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    let files = render_run_dir(&dir).unwrap();
    let final_dir = dir.join("final");
    let readme = write_catalog(&final_dir, &files).unwrap();
    assert_eq!(readme, final_dir.join("README.md"));
    for (path, text) in &files {
        assert_eq!(
            &std::fs::read_to_string(final_dir.join(path)).unwrap(),
            text
        );
    }
}

#[test]
fn rendering_a_run_dir_again_writes_identical_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    let final_dir = dir.join("final");
    write_catalog(&final_dir, &render_run_dir(&dir).unwrap()).unwrap();
    let first = std::fs::read(final_dir.join("catalog.json")).unwrap();
    write_catalog(&final_dir, &render_run_dir(&dir).unwrap()).unwrap();
    assert_eq!(
        std::fs::read(final_dir.join("catalog.json")).unwrap(),
        first
    );
}

#[test]
fn the_phase_output_names_the_catalogue_readme() {
    let output = PhaseOutput::Catalog(PathBuf::from("final/README.md"));
    let json = serde_json::to_value(&output).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"kind": "Catalog", "path": "final/README.md"})
    );
}

// ------------------------------------------------------------ curation

/// Curation of `auth:oauth` (sk_0001, sk_0003) into one group led by
/// sk_0003, written under `<dir>/curation/`; `None` writes a failed one.
fn write_oauth_curation(dir: &Path, ok: bool, members: &[&str]) {
    use crate::discovery::curation::{Curation, RawCuration, curation_path, normalise};
    let ids: Vec<String> = members.iter().map(|s| (*s).to_owned()).collect();
    let raw: RawCuration = serde_json::from_value(serde_json::json!({
        "groups": [{"label": "Delegated login", "summary": "An identity provider signs users in.", "representative": 1}],
        "assign": (0..ids.len()).map(|n| serde_json::json!({"n": n, "g": 0})).collect::<Vec<_>>()
    }))
    .unwrap();
    let chunk = ok.then(|| normalise(&ids, &raw));
    write_json(
        &curation_path(&dir.join(CURATION_DIR), "auth", "oauth"),
        &Curation::merge("auth:oauth", ids, vec![chunk]),
    )
    .unwrap();
}

#[test]
fn a_curation_in_the_run_dir_groups_its_cell() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    write_oauth_curation(&dir, true, &["sk_0001", "sk_0003"]);
    let files = render_run_dir(&dir).unwrap();
    let facet_text = text(&files, "auth/oauth.md");
    assert!(facet_text.contains("\n2 theses in 1 group.\n\n## Delegated login\n\nAn identity provider signs users in.\n\n### ★ sk_0003\n"), "{facet_text}");
    assert!(facet_text.contains("\n### sk_0001\n"));
    assert!(text(&files, "auth/api-key.md").contains("\n## All theses\n"));
    assert!(text(&files, "README.md").contains("\n- Grouped cells: 1 of 2 with theses\n"));
}

#[test]
fn a_failed_curation_in_the_run_dir_shows_the_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    write_oauth_curation(&dir, false, &["sk_0001", "sk_0003"]);
    let facet_text = text(&render_run_dir(&dir).unwrap(), "auth/oauth.md");
    assert!(facet_text.contains("\n⚠ Grouping failed for this cell; its theses are listed without groups.\n\n## All theses\n"));
}

#[test]
fn a_curation_made_for_other_theses_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    write_oauth_curation(&dir, true, &["sk_0001"]);
    let files = render_run_dir(&dir).unwrap();
    let facet_text = text(&files, "auth/oauth.md");
    assert!(
        facet_text.contains("\n2 theses.\n\n## All theses\n"),
        "{facet_text}"
    );
    assert!(!facet_text.contains('★'));
}

#[test]
fn an_unreadable_curation_file_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = run_dir(tmp.path());
    std::fs::create_dir_all(dir.join(CURATION_DIR)).unwrap();
    std::fs::write(dir.join(CURATION_DIR).join("auth__oauth.json"), "{broken").unwrap();
    let facet_text = text(&render_run_dir(&dir).unwrap(), "auth/oauth.md");
    assert!(facet_text.contains("\n## All theses\n"));
}
