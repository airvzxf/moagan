//! Text rendering of a discover catalogue.
//!
//! Pure: the same inputs always yield the same bytes (no clock, no
//! hash-map order). Structural headings are short English words; all
//! content is the model's text as written. Every count shown is
//! computed from the catalogue, never taken from a model answer.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::discovery::catalog::{Catalog, CellEntry, Member, OUTSIDE_DIMENSION_ID};
use crate::discovery::matrix::{ExplorationMatrix, TemperatureProfile};
use crate::domain::{Sketch, SketchProvenance};

/// Value of the `schema` field of `catalog.json`.
pub const CATALOG_SCHEMA: &str = "discover-catalog-v1";

/// Everything the renderer reads. Nothing else influences the output.
#[derive(Debug, Clone, Copy)]
pub struct RenderInput<'a> {
    /// Run id shown in `README.md` and `catalog.json`.
    pub run_id: &'a str,
    /// Problem statement from the brief.
    pub problem: &'a str,
    /// Brief constraints; entry `i` is constraint `C{i+1}`.
    pub constraints: &'a [String],
    /// Matrix of the run (profiles and fan-out).
    pub matrix: &'a ExplorationMatrix,
    /// Catalogue built from `sketches`.
    pub catalog: &'a Catalog,
    /// Every sketch of the run.
    pub sketches: &'a [Sketch],
}

/// Render the catalogue as files relative to `final/`, in this order:
/// `README.md`, one facet file per catalogue cell (catalogue order),
/// `constraints-annex.md`, `catalog.json`. Every file ends with `\n`.
pub fn render(input: &RenderInput<'_>) -> Vec<(PathBuf, String)> {
    let by_id: BTreeMap<&str, &Sketch> =
        input.sketches.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut files = vec![(PathBuf::from("README.md"), readme(input, &by_id))];
    for cell in &input.catalog.cells {
        files.push((
            facet_path(&cell.dimension_id, &cell.facet_id),
            facet_file(cell, &by_id),
        ));
    }
    files.push((PathBuf::from("constraints-annex.md"), annex(input, &by_id)));
    files.push((PathBuf::from("catalog.json"), catalog_json(input, &by_id)));
    files
}

/// Path of a cell's facet file relative to `final/`:
/// `<dimension_id>/<facet_id>.md`. Characters other than ASCII letters,
/// digits, `-` and `_` become `-`, so a model-chosen id can never
/// leave `final/`; an empty id becomes `-`.
pub fn facet_path(dimension_id: &str, facet_id: &str) -> PathBuf {
    PathBuf::from(path_segment(dimension_id)).join(format!("{}.md", path_segment(facet_id)))
}

/// `id` with every character other than ASCII letters, digits, `-` and
/// `_` replaced by `-`; an empty id becomes `-`.
pub(crate) fn path_segment(id: &str) -> String {
    let segment: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if segment.is_empty() {
        "-".to_owned()
    } else {
        segment
    }
}

fn facet_link(cell: &CellEntry) -> String {
    format!(
        "{}/{}.md",
        path_segment(&cell.dimension_id),
        path_segment(&cell.facet_id)
    )
}

fn cell_members(cell: &CellEntry) -> impl Iterator<Item = &Member> {
    cell.groups.iter().flat_map(|g| g.members.iter())
}

fn is_flagged_member(member: &Member, by_id: &BTreeMap<&str, &Sketch>) -> bool {
    by_id
        .get(member.sketch_id.as_str())
        .is_some_and(|s| s.hard_constraint_check.values().any(|ok| !ok))
}

fn readme(input: &RenderInput<'_>, by_id: &BTreeMap<&str, &Sketch>) -> String {
    let catalog = input.catalog;
    let members = catalog.member_count();
    let total = input.sketches.len();
    let pct = if total == 0 {
        0.0
    } else {
        members as f64 * 100.0 / total as f64
    };
    let matrix_cells: Vec<&CellEntry> = catalog
        .cells
        .iter()
        .filter(|c| c.dimension_id != OUTSIDE_DIMENSION_ID)
        .collect();
    let non_empty = matrix_cells.iter().filter(|c| !c.groups.is_empty()).count();
    let problem = if input.problem.trim().is_empty() {
        "_No problem statement in brief.json._".to_owned()
    } else {
        input.problem.trim().to_owned()
    };

    let mut run = vec![
        format!("- Theses in this catalogue: {members} of {total} sketches ({pct:.1} %)"),
        format!("- Cells: {} ({non_empty} with theses)", matrix_cells.len()),
    ];
    run.extend(profile_lines(input.matrix));
    run.push(format!(
        "- Sketches per cell and profile point: {}",
        input.matrix.sketches_per_cell
    ));

    let mut table = vec![
        "| Dimension | Facet | Theses | Groups | Duplicates | Flagged |".to_owned(),
        "|---|---|---:|---:|---:|---:|".to_owned(),
    ];
    let mut flagged_total = 0;
    for cell in &catalog.cells {
        let theses = cell_members(cell).count();
        let duplicates = cell_members(cell)
            .filter(|m| m.duplicate_of.is_some())
            .count();
        let flagged = cell_members(cell)
            .filter(|m| is_flagged_member(m, by_id))
            .count();
        flagged_total += flagged;
        table.push(format!(
            "| {} | [{}]({}) | {theses} | {} | {duplicates} | {flagged} |",
            table_text(&cell.dimension_label),
            table_text(&cell.facet_label),
            facet_link(cell),
            cell.groups.len(),
        ));
    }
    let annex_line = if flagged_total == 1 {
        "[Constraint annex](constraints-annex.md): 1 thesis marks at least one constraint as not met.".to_owned()
    } else {
        format!(
            "[Constraint annex](constraints-annex.md): {flagged_total} theses mark at least one constraint as not met."
        )
    };

    finish(vec![
        "# Discover catalogue".to_owned(),
        format!("Run `{}`", input.run_id),
        "## Problem".to_owned(),
        problem,
        "## Run".to_owned(),
        run.join("\n"),
        "## Matrix".to_owned(),
        table.join("\n"),
        annex_line,
    ])
}

/// One line per profile: sorted `temperature_profiles` keys, or the
/// default profile when the map is empty.
fn profile_lines(matrix: &ExplorationMatrix) -> Vec<String> {
    if matrix.temperature_profiles.is_empty() {
        return vec![profile_line("default", &matrix.default_profile)];
    }
    let sorted: BTreeMap<&String, &TemperatureProfile> =
        matrix.temperature_profiles.iter().collect();
    sorted
        .into_iter()
        .map(|(key, profile)| profile_line(key, profile))
        .collect()
}

fn profile_line(key: &str, profile: &TemperatureProfile) -> String {
    let temperatures: Vec<String> = profile
        .temperatures
        .iter()
        .map(|t| format!("{t:?}"))
        .collect();
    format!(
        "- Profile `{key}`: T={} × {} replica(s)",
        temperatures.join(", "),
        profile.replicas_per_temperature
    )
}

fn facet_file(cell: &CellEntry, by_id: &BTreeMap<&str, &Sketch>) -> String {
    let count = cell_members(cell).count();
    let mut blocks = vec![format!("# {} → {}", cell.dimension_label, cell.facet_label)];
    if !cell.description.trim().is_empty() {
        blocks.push(cell.description.trim().to_owned());
    }
    blocks.push(if count == 1 {
        "1 thesis.".to_owned()
    } else {
        format!("{count} theses.")
    });
    for group in &cell.groups {
        blocks.push(format!("## {}", group.label));
        if !group.summary.trim().is_empty() {
            blocks.push(group.summary.trim().to_owned());
        }
        for member in &group.members {
            blocks.push(format!("### {}", member.sketch_id));
            match by_id.get(member.sketch_id.as_str()) {
                Some(sketch) => blocks.extend(sketch_blocks(sketch)),
                None => blocks.push("_No sketch with this id._".to_owned()),
            }
        }
    }
    finish(blocks)
}

fn sketch_blocks(sketch: &Sketch) -> Vec<String> {
    let mut blocks = Vec::new();
    blocks.push(match &sketch.provenance {
        Some(p) => format!(
            "T={:?} · {}/{} · replica {} · index {}",
            p.temperature, p.section, p.model, p.replica, p.index
        ),
        None => "_Provenance not recorded (model, temperature, replica, index)._".to_owned(),
    });
    blocks.push(if sketch.thesis.trim().is_empty() {
        "_No thesis._".to_owned()
    } else {
        one_line(&sketch.thesis)
    });
    if !sketch.key_decisions.is_empty() {
        blocks.push(bullets(&sketch.key_decisions));
    }

    let mut details = Vec::new();
    if !sketch.architecture_outline.trim().is_empty() {
        details.push(format!(
            "**Outline.** {}",
            sketch.architecture_outline.trim()
        ));
    }
    for (title, items) in [
        ("Assumptions", &sketch.assumptions),
        ("Strengths", &sketch.strengths),
        ("Weaknesses", &sketch.weaknesses),
    ] {
        if !items.is_empty() {
            details.push(format!("**{title}.**"));
            details.push(bullets(items));
        }
    }
    if !sketch.hard_constraint_check.is_empty() {
        let checks: Vec<String> = ordered_checks(&sketch.hard_constraint_check)
            .into_iter()
            .map(|(key, ok)| format!("{key} {}", if ok { "✓" } else { "✗" }))
            .collect();
        details.push(format!("**Constraint check.** {}", checks.join(" · ")));
    }
    if !sketch.expected_validation.trim().is_empty() {
        details.push(format!(
            "**Expected validation.** {}",
            sketch.expected_validation.trim()
        ));
    }
    if !details.is_empty() {
        blocks.push("<details>\n<summary>Details</summary>".to_owned());
        blocks.extend(details);
        blocks.push("</details>".to_owned());
    }
    blocks
}

fn annex(input: &RenderInput<'_>, by_id: &BTreeMap<&str, &Sketch>) -> String {
    let mut cell_by_id: BTreeMap<&str, &CellEntry> = BTreeMap::new();
    for cell in &input.catalog.cells {
        for member in cell_members(cell) {
            cell_by_id.entry(member.sketch_id.as_str()).or_insert(cell);
        }
    }
    let mut failed: BTreeMap<&str, Vec<&Sketch>> = BTreeMap::new();
    for id in cell_by_id.keys() {
        if let Some(sketch) = by_id.get(id) {
            for (key, ok) in &sketch.hard_constraint_check {
                if !ok {
                    failed.entry(key.as_str()).or_default().push(sketch);
                }
            }
        }
    }

    let mut blocks = vec![
        "# Constraint annex".to_owned(),
        "Theses that mark at least one brief constraint as not met, by constraint.".to_owned(),
    ];
    if failed.is_empty() {
        blocks.push("No thesis marks a constraint as not met.".to_owned());
    }
    let mut keys: Vec<&str> = failed.keys().copied().collect();
    keys.sort_by_key(|k| constraint_rank(k));
    for key in keys {
        blocks.push(constraint_heading(key, input.constraints));
        let lines: Vec<String> = failed[key]
            .iter()
            .map(|sketch| {
                let (dimension, facet) = cell_by_id.get(sketch.id.as_str()).map_or(("", ""), |c| {
                    (c.dimension_label.as_str(), c.facet_label.as_str())
                });
                format!(
                    "- **{}** · {dimension} → {facet} · {}",
                    sketch.id,
                    one_line(&sketch.thesis)
                )
            })
            .collect();
        blocks.push(lines.join("\n"));
    }
    finish(blocks)
}

fn constraint_heading(key: &str, constraints: &[String]) -> String {
    let text = constraint_number(key)
        .and_then(|n| usize::try_from(n).ok())
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| constraints.get(i));
    match text {
        Some(text) => format!("## {key} — {}", one_line(text)),
        None => format!("## {key}"),
    }
}

#[derive(Serialize)]
struct CatalogJson<'a> {
    schema: &'static str,
    run_id: &'a str,
    sketch_count: usize,
    constraints: Vec<ConstraintJson<'a>>,
    cells: &'a [CellEntry],
    sketches: Vec<SketchJson<'a>>,
}

#[derive(Serialize)]
struct ConstraintJson<'a> {
    id: String,
    text: &'a str,
}

#[derive(Serialize)]
struct SketchJson<'a> {
    id: &'a str,
    cell: String,
    group: &'a str,
    duplicate_of: Option<&'a str>,
    flags: Vec<&'a str>,
    provenance: Option<&'a SketchProvenance>,
}

fn catalog_json(input: &RenderInput<'_>, by_id: &BTreeMap<&str, &Sketch>) -> String {
    let mut sketches: Vec<SketchJson<'_>> = Vec::new();
    for cell in &input.catalog.cells {
        for group in &cell.groups {
            for member in &group.members {
                let sketch = by_id.get(member.sketch_id.as_str());
                let flags = sketch.map_or_else(Vec::new, |s| {
                    ordered_checks(&s.hard_constraint_check)
                        .into_iter()
                        .filter(|(_, ok)| !ok)
                        .map(|(key, _)| key)
                        .collect()
                });
                sketches.push(SketchJson {
                    id: member.sketch_id.as_str(),
                    cell: format!("{}:{}", cell.dimension_id, cell.facet_id),
                    group: group.label.as_str(),
                    duplicate_of: member.duplicate_of.as_deref(),
                    flags,
                    provenance: sketch.and_then(|s| s.provenance.as_ref()),
                });
            }
        }
    }
    sketches.sort_by(|a, b| a.id.cmp(b.id));
    let doc = CatalogJson {
        schema: CATALOG_SCHEMA,
        run_id: input.run_id,
        sketch_count: input.sketches.len(),
        constraints: input
            .constraints
            .iter()
            .enumerate()
            .map(|(i, text)| ConstraintJson {
                id: format!("C{}", i + 1),
                text: text.as_str(),
            })
            .collect(),
        cells: &input.catalog.cells,
        sketches,
    };
    // Plain structs with string keys: serialisation cannot fail.
    let mut json = serde_json::to_string_pretty(&doc).unwrap_or_default();
    json.push('\n');
    json
}

/// Checks in natural key order: `C1, C2, …, C10` by number, then every
/// other key alphabetically.
fn ordered_checks(checks: &BTreeMap<String, bool>) -> Vec<(&str, bool)> {
    let mut keyed: Vec<(&str, bool)> = checks.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    keyed.sort_by_key(|(k, _)| constraint_rank(k));
    keyed
}

fn constraint_rank(key: &str) -> (u8, u64, &str) {
    match constraint_number(key) {
        Some(n) => (0, n, key),
        None => (1, 0, key),
    }
}

fn constraint_number(key: &str) -> Option<u64> {
    let digits = key.strip_prefix('C')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn bullets(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {}", one_line(item)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn table_text(text: &str) -> String {
    one_line(text).replace('|', "\\|")
}

fn finish(blocks: Vec<String>) -> String {
    let mut text = blocks.join("\n\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests;
