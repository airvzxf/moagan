//! Replay the catalogue render over a recorded discover run.
//!
//! Dev-only harness, no LLM call:
//!
//! ```text
//! MOAGAN_REPLAY_RUN_DIR=<run dir> cargo test --test discover_render_replay -- --ignored --nocapture
//! ```
//!
//! The run dir is only read. The rendered files go to
//! `MOAGAN_REPLAY_OUT_DIR` when it is set, otherwise to a temp dir
//! removed at the end of the test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use moagan::phases::discover_render::{render_run_dir, write_catalog};

/// Primary sketch files of a run dir (`sketches/*.json` without `.meta.json` seals).
fn sketch_file_count(run_dir: &Path) -> usize {
    std::fs::read_dir(run_dir.join("sketches"))
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".json") && !n.ends_with(".meta.json"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
#[ignore = "replays a recorded run; set MOAGAN_REPLAY_RUN_DIR and run with --ignored"]
fn replay_lists_every_sketch_of_a_recorded_run_exactly_once() {
    let run_dir = PathBuf::from(
        std::env::var("MOAGAN_REPLAY_RUN_DIR").expect("set MOAGAN_REPLAY_RUN_DIR to a run dir"),
    );
    let files = render_run_dir(&run_dir).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let out = std::env::var_os("MOAGAN_REPLAY_OUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| tmp.path().to_path_buf());
    write_catalog(&out, &files).unwrap();

    let catalog: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("catalog.json")).unwrap()).unwrap();
    let listed: Vec<String> = catalog["sketches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect();
    let unique: BTreeSet<&String> = listed.iter().collect();
    let sketch_files = sketch_file_count(&run_dir);
    assert_eq!(unique.len(), listed.len(), "catalog.json repeats a sketch");
    assert_eq!(
        listed.len(),
        sketch_files,
        "catalog.json must list every sketch file"
    );

    let facets: Vec<&(PathBuf, String)> = files
        .iter()
        .filter(|(p, _)| p.components().count() == 2)
        .collect();
    for id in &listed {
        let heading = format!("### {id}");
        let hits: usize = facets
            .iter()
            .map(|(_, text)| text.lines().filter(|l| *l == heading).count())
            .sum();
        assert_eq!(hits, 1, "{heading} must appear in exactly one facet file");
    }

    println!(
        "replay: {} of {} sketch files catalogued across {} facet files -> {}",
        listed.len(),
        sketch_files,
        facets.len(),
        out.display()
    );
}
