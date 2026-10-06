//! Curation of one discover cell: the curator's grouping of its
//! theses, checked and normalised in Rust.
//!
//! The model sees the theses of a cell as a numbered list and answers
//! with groups plus one `{"n", "g"}` pair per thesis. Nothing in the
//! answer is trusted: every thesis ends in exactly one group (the
//! unassigned ones in [`UNGROUPED_LABEL`]), every group has a
//! representative among its members, and duplicates and tensions only
//! survive between real theses. Duplicates are folded under a thesis
//! of the same group, never removed.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::discovery::catalog::{Catalog, CellCuration, CellEntry, Group, Member, Tension};
use crate::discovery::render::path_segment;
use crate::domain::Sketch;

/// Directory of the run dir that holds one curation file per cell.
pub const CURATION_DIR: &str = "curation";

/// Most theses sent in one curator call; larger cells are split.
pub const MAX_THESES_PER_CALL: usize = 140;

/// Fewest theses a cell needs to be curated; smaller cells stay flat.
pub const MIN_THESES_TO_CURATE: usize = 2;

/// Label of the group that collects the theses the model did not assign.
pub const UNGROUPED_LABEL: &str = "Ungrouped";

/// Most key decisions of a thesis shown to the curator.
pub const MAX_KEY_DECISIONS: usize = 5;

/// The curator's answer as parsed, before any check. Each field keeps
/// raw JSON, so a field of the wrong type or one malformed entry never
/// discards the rest of the answer; a missing field is `null`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct RawCuration {
    /// `[{"label", "summary", "representative"}]`; group `g` is entry `g`.
    #[serde(default)]
    pub groups: serde_json::Value,
    /// `[{"n", "g"}]`: thesis `n` belongs to group `g`.
    #[serde(default)]
    pub assign: serde_json::Value,
    /// `[[n, m]]`: thesis `n` says the same as thesis `m`.
    #[serde(default)]
    pub duplicates: serde_json::Value,
    /// `[[n, m, "note"]]`: the choices of `n` and `m` cannot both be adopted.
    #[serde(default)]
    pub tensions: serde_json::Value,
}

/// What the checks found in one answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurationReport {
    /// Theses sent to the model.
    pub theses: usize,
    /// Theses without a valid assignment (they go to [`UNGROUPED_LABEL`]).
    pub unassigned: usize,
    /// `assign` entries that name no thesis or no group, or are malformed.
    pub unknown: usize,
    /// `assign` entries for a thesis already assigned (the first one wins).
    pub repeated: usize,
    /// `duplicates` entries dropped by the checks.
    pub dropped_duplicates: usize,
    /// `tensions` entries dropped by the checks.
    pub dropped_tensions: usize,
}

impl CurationReport {
    /// True when at most 20 % of the theses are unassigned.
    pub fn is_acceptable(&self) -> bool {
        self.unassigned * 5 <= self.theses
    }
}

/// The normalised curation of one chunk of theses.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkCuration {
    /// Groups in the model's order, [`UNGROUPED_LABEL`] last; never empty.
    pub groups: Vec<Group>,
    /// Valid tensions in the model's order.
    pub tensions: Vec<Tension>,
    /// What the checks found.
    pub report: CurationReport,
}

/// Outcome of the curation of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurationStatus {
    /// Every chunk got an acceptable answer.
    Ok,
    /// At least one chunk did not; the cell is rendered flat.
    Failed,
}

/// Content of `curation/<dimension>__<facet>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Curation {
    /// Cell key, `"<dimension_id>:<facet_id>"`.
    pub cell: String,
    /// Whether the groups can be used.
    pub status: CurationStatus,
    /// Sorted sketch ids the curation was made for.
    pub members: Vec<String>,
    /// Groups of every chunk in order; empty when failed.
    pub groups: Vec<Group>,
    /// Tensions of every chunk in order; empty when failed.
    pub tensions: Vec<Tension>,
    /// Report of each chunk that got an acceptable answer.
    pub reports: Vec<CurationReport>,
}

/// Check and normalise the answer for the theses `ids` (thesis `n` is
/// `ids[n]`):
///
/// * group `g` is entry `g` of `groups`; its label and summary have
///   their whitespace collapsed, an empty label becomes `Group {g+1}`;
/// * an `assign` entry `{"n", "g"}` with a known thesis and group puts
///   the thesis in the group unless it already has one; theses left
///   without a group form a last [`UNGROUPED_LABEL`] group;
/// * groups without members are dropped; the representative is the
///   model's when it is a member, else the member with the smallest id;
///   members are listed representative first, then by id;
/// * a `duplicates` pair counts only when both theses are in the same
///   model group; the pairs join theses into sets, and every thesis of
///   a set except its head (the representative when it is in the set,
///   else the smallest id) gets `duplicate_of = head`;
/// * a `tensions` entry counts when both theses exist and differ, once
///   per unordered pair; its note has its whitespace collapsed.
pub fn normalise(ids: &[String], raw: &RawCuration) -> ChunkCuration {
    let mut report = CurationReport {
        theses: ids.len(),
        ..CurationReport::default()
    };
    let raw_groups: &[serde_json::Value] = raw.groups.as_array().map_or(&[], Vec::as_slice);

    let mut group_of: Vec<Option<usize>> = vec![None; ids.len()];
    for entry in raw.assign.as_array().map_or(&[][..], Vec::as_slice) {
        let n = index_field(entry, "n").filter(|&n| n < ids.len());
        let g = index_field(entry, "g").filter(|&g| g < raw_groups.len());
        match (n, g) {
            (Some(n), Some(g)) => {
                if group_of[n].is_some() {
                    report.repeated += 1;
                } else {
                    group_of[n] = Some(g);
                }
            }
            _ => report.unknown += 1,
        }
    }

    // Group number (model's, or raw_groups.len() for Ungrouped) → index in `groups`.
    let mut groups: Vec<Group> = Vec::new();
    let mut slot_of: BTreeMap<usize, usize> = BTreeMap::new();
    for (g, value) in raw_groups.iter().enumerate() {
        let members: Vec<usize> = (0..ids.len()).filter(|&n| group_of[n] == Some(g)).collect();
        if members.is_empty() {
            continue;
        }
        let label = text_field(value, "label");
        let label = if label.is_empty() {
            format!("Group {}", g + 1)
        } else {
            label
        };
        let wanted = index_field(value, "representative").filter(|n| members.contains(n));
        slot_of.insert(g, groups.len());
        groups.push(make_group(
            ids,
            label,
            text_field(value, "summary"),
            &members,
            wanted,
        ));
    }
    let unassigned: Vec<usize> = (0..ids.len()).filter(|&n| group_of[n].is_none()).collect();
    report.unassigned = unassigned.len();
    if !unassigned.is_empty() {
        groups.push(make_group(
            ids,
            UNGROUPED_LABEL.to_owned(),
            String::new(),
            &unassigned,
            None,
        ));
    }

    // Duplicate sets per model group, joined with a small union-find.
    let mut parent: Vec<usize> = (0..ids.len()).collect();
    for entry in raw.duplicates.as_array().map_or(&[][..], Vec::as_slice) {
        let pair = pair_of(entry, ids.len());
        match pair {
            Some((n, m)) if group_of[n].is_some() && group_of[n] == group_of[m] => {
                let (rn, rm) = (find(&mut parent, n), find(&mut parent, m));
                parent[rn.max(rm)] = rn.min(rm);
            }
            _ => report.dropped_duplicates += 1,
        }
    }
    let mut sets: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for n in 0..ids.len() {
        let root = find(&mut parent, n);
        sets.entry(root).or_default().push(n);
    }
    for set in sets.values().filter(|set| set.len() > 1) {
        let Some(slot) = group_of[set[0]].and_then(|g| slot_of.get(&g).copied()) else {
            continue;
        };
        let group = &mut groups[slot];
        let representative = group.representative.clone();
        let head = set
            .iter()
            .map(|&n| ids[n].as_str())
            .find(|id| Some(*id) == representative.as_deref())
            .or_else(|| set.iter().map(|&n| ids[n].as_str()).min())
            .map(str::to_owned);
        for member in &mut group.members {
            let in_set = set.iter().any(|&n| ids[n] == member.sketch_id);
            if in_set && Some(&member.sketch_id) != head.as_ref() {
                member.duplicate_of = head.clone();
            }
        }
    }

    let mut tensions = Vec::new();
    let mut seen: BTreeSet<(usize, usize)> = BTreeSet::new();
    for entry in raw.tensions.as_array().map_or(&[][..], Vec::as_slice) {
        match pair_of(entry, ids.len()) {
            Some((n, m)) if seen.insert((n.min(m), n.max(m))) => tensions.push(Tension {
                a: ids[n].clone(),
                b: ids[m].clone(),
                note: entry
                    .get(2)
                    .and_then(serde_json::Value::as_str)
                    .map(one_line)
                    .unwrap_or_default(),
            }),
            _ => report.dropped_tensions += 1,
        }
    }

    ChunkCuration {
        groups,
        tensions,
        report,
    }
}

/// Positions of `n` theses split into `ceil(n / max)` consecutive
/// chunks of near-equal size (`max` below 1 counts as 1); no chunk for
/// `n = 0`.
pub fn chunk_ranges(n: usize, max: usize) -> Vec<Range<usize>> {
    let chunks = n.div_ceil(max.max(1));
    (0..chunks)
        .map(|i| i * n / chunks..(i + 1) * n / chunks)
        .collect()
}

#[derive(Serialize)]
struct Payload<'a> {
    cell: PayloadCell<'a>,
    constraints: Vec<String>,
    theses: Vec<PayloadThesis<'a>>,
}

#[derive(Serialize)]
struct PayloadCell<'a> {
    dimension: &'a str,
    facet: &'a str,
    description: &'a str,
}

#[derive(Serialize)]
struct PayloadThesis<'a> {
    n: usize,
    thesis: &'a str,
    key_decisions: &'a [String],
    violates: Vec<&'a str>,
}

/// User message of one curator call: the JSON object
/// `{"cell": {"dimension", "facet", "description"}, "constraints":
/// ["C1: …"], "theses": [{"n", "thesis", "key_decisions", "violates"}]}`
/// with the cell's labels, the brief constraints numbered from `C1`,
/// and `theses` numbered from 0 in the given order, each with its first
/// [`MAX_KEY_DECISIONS`] key decisions and the sorted keys of its
/// constraint checks marked false. Never names a sketch id.
pub fn curator_payload(cell: &CellEntry, constraints: &[String], theses: &[&Sketch]) -> String {
    let payload = Payload {
        cell: PayloadCell {
            dimension: &cell.dimension_label,
            facet: &cell.facet_label,
            description: &cell.description,
        },
        constraints: constraints
            .iter()
            .enumerate()
            .map(|(i, c)| format!("C{}: {c}", i + 1))
            .collect(),
        theses: theses
            .iter()
            .enumerate()
            .map(|(n, s)| PayloadThesis {
                n,
                thesis: &s.thesis,
                key_decisions: &s.key_decisions[..s.key_decisions.len().min(MAX_KEY_DECISIONS)],
                violates: s
                    .hard_constraint_check
                    .iter()
                    .filter(|(_, ok)| !**ok)
                    .map(|(k, _)| k.as_str())
                    .collect(),
            })
            .collect(),
    };
    // Plain structs with string keys: serialisation cannot fail.
    serde_json::to_string(&payload).unwrap_or_default()
}

impl Curation {
    /// Join the chunk results of the cell `cell` whose sorted sketch ids
    /// are `members`. Any `None` (a chunk without an acceptable answer)
    /// makes the curation `Failed`, with no groups and no tensions.
    pub fn merge(cell: &str, members: Vec<String>, chunks: Vec<Option<ChunkCuration>>) -> Self {
        let mut curation = Self {
            cell: cell.to_owned(),
            status: CurationStatus::Ok,
            members,
            groups: Vec::new(),
            tensions: Vec::new(),
            reports: Vec::new(),
        };
        for chunk in chunks {
            match chunk {
                Some(chunk) => {
                    curation.groups.extend(chunk.groups);
                    curation.tensions.extend(chunk.tensions);
                    curation.reports.push(chunk.report);
                }
                None => curation.status = CurationStatus::Failed,
            }
        }
        if curation.status == CurationStatus::Failed {
            curation.groups.clear();
            curation.tensions.clear();
        }
        curation
    }

    /// True when this curation was made for `cell` and for exactly the
    /// sorted sketch ids `ids`, and, when `Ok`, its groups list each of
    /// those ids exactly once.
    pub fn covers(&self, cell: &str, ids: &[String]) -> bool {
        if self.cell != cell || self.members != ids {
            return false;
        }
        if self.status == CurationStatus::Failed {
            return true;
        }
        let mut listed: Vec<&str> = self
            .groups
            .iter()
            .flat_map(|g| g.members.iter())
            .map(|m| m.sketch_id.as_str())
            .collect();
        listed.sort_unstable();
        listed.iter().copied().eq(ids.iter().map(String::as_str))
    }
}

/// Key of a cell: `"<dimension_id>:<facet_id>"`, the same form as a
/// sketch's `angle`.
pub fn cell_key(cell: &CellEntry) -> String {
    format!("{}:{}", cell.dimension_id, cell.facet_id)
}

/// Sorted sketch ids listed in `cell`.
pub fn cell_member_ids(cell: &CellEntry) -> Vec<String> {
    let mut ids: Vec<String> = cell
        .groups
        .iter()
        .flat_map(|g| g.members.iter())
        .map(|m| m.sketch_id.clone())
        .collect();
    ids.sort();
    ids
}

/// Path of a cell's curation file:
/// `<curation_dir>/<dimension_id>__<facet_id>.json`, each id sanitised
/// like the facet file path.
pub fn curation_path(curation_dir: &Path, dimension_id: &str, facet_id: &str) -> PathBuf {
    curation_dir.join(format!(
        "{}__{}.json",
        path_segment(dimension_id),
        path_segment(facet_id)
    ))
}

/// Use the curations that cover a cell (see [`Curation::covers`]): an
/// `Ok` one replaces the cell's groups and tensions and marks it
/// `Grouped`; a `Failed` one keeps the implicit group and marks it
/// `Failed`. Every other cell is left as it is.
pub fn apply_curations(catalog: &mut Catalog, curations: &[Curation]) {
    for cell in &mut catalog.cells {
        let key = cell_key(cell);
        let ids = cell_member_ids(cell);
        let Some(curation) = curations.iter().find(|c| c.covers(&key, &ids)) else {
            continue;
        };
        match curation.status {
            CurationStatus::Ok => {
                cell.groups = curation.groups.clone();
                cell.tensions = curation.tensions.clone();
                cell.curation = CellCuration::Grouped;
            }
            CurationStatus::Failed => cell.curation = CellCuration::Failed,
        }
    }
}

fn make_group(
    ids: &[String],
    label: String,
    summary: String,
    members: &[usize],
    wanted: Option<usize>,
) -> Group {
    let mut sorted: Vec<&str> = members.iter().map(|&n| ids[n].as_str()).collect();
    sorted.sort_unstable();
    let representative = wanted.map_or(sorted[0], |n| ids[n].as_str()).to_owned();
    let mut ordered = vec![representative.clone()];
    ordered.extend(
        sorted
            .into_iter()
            .filter(|id| *id != representative)
            .map(str::to_owned),
    );
    Group {
        label,
        summary,
        representative: Some(representative),
        members: ordered
            .into_iter()
            .map(|sketch_id| Member {
                sketch_id,
                duplicate_of: None,
            })
            .collect(),
    }
}

fn find(parent: &mut [usize], mut n: usize) -> usize {
    while parent[n] != n {
        parent[n] = parent[parent[n]];
        n = parent[n];
    }
    n
}

fn index_field(value: &serde_json::Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

fn pair_of(entry: &serde_json::Value, len: usize) -> Option<(usize, usize)> {
    let at = |i: usize| {
        entry
            .get(i)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n < len)
    };
    let (n, m) = (at(0)?, at(1)?);
    (n != m).then_some((n, m))
}

fn text_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(one_line)
        .unwrap_or_default()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests;
