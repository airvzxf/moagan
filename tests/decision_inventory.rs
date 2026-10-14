//! The decision inventory lists exactly the kinds the code emits (#1020).
//!
//! `DECISION_KIND_INVENTORY` feeds `docs/events-reference.md` and the
//! `--decision-format` help. A kind with no `emit_decision("<kind>", …)`
//! call site promises an event that never comes; an emitted kind
//! missing from the inventory is undocumented.

use std::collections::BTreeSet;
use std::path::Path;

use moagan::telemetry::stdout_events::DECISION_KIND_INVENTORY;

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The first string literal after each `emit_decision(` outside a
/// comment line, across `src/`.
fn emitted_kinds() -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut kinds = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for (at, _) in code.match_indices("emit_decision(") {
            let rest = code[at + "emit_decision(".len()..].trim_start();
            if let Some(literal) = rest.strip_prefix('"')
                && let Some(end) = literal.find('"')
            {
                kinds.insert(literal[..end].to_owned());
            }
        }
    }
    kinds
}

#[test]
fn every_inventory_kind_is_emitted_and_every_emitted_kind_is_listed() {
    let listed: BTreeSet<String> = DECISION_KIND_INVENTORY
        .iter()
        .map(|(kind, _)| (*kind).to_owned())
        .collect();
    let emitted = emitted_kinds();
    assert!(
        !emitted.is_empty(),
        "no emit_decision call found under src/"
    );
    let never_emitted: Vec<&String> = listed.difference(&emitted).collect();
    let unlisted: Vec<&String> = emitted.difference(&listed).collect();
    assert!(
        never_emitted.is_empty(),
        "listed but never emitted: {never_emitted:?}"
    );
    assert!(unlisted.is_empty(), "emitted but not listed: {unlisted:?}");
}

#[test]
fn the_removed_kinds_are_gone_from_the_events_reference() {
    let doc = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/events-reference.md"),
    )
    .expect("read docs/events-reference.md");
    for gone in ["cache_hit", "category_assigned"] {
        assert!(
            !doc.contains(&format!("`{gone}`")),
            "{gone} is never emitted"
        );
    }
}
