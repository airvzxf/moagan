//! `moagan diff <run_a> <run_b>` — cross-run comparison (D.14.2).
//!
//! Top-level wrapper over `TelemetryCmd::Compare` (the existing
//! `moagan telemetry compare`) that adds filesystem-aware metrics
//! (proposals, evaluations, phases_visited, ranking delta) and three
//! output formats (`text`, `md`, `json`).
//!
//! Inspired by T01-10 §7.1, T16-01 §6.1 and T10-08: given two runs of
//! the same problem (typically `continue` + original, or two reruns
//! with different modes), report a side-by-side on every metric the
//! telemetry layer tracks so operators can eyeball whether the new
//! run regressed on any axis.
//!
//! Exit codes:
//!   0 — comparison ran.
//!   2 — `Error::InvalidArgs` (malformed run id, self-diff).
//!   8 — `Error::Io` (filesystem failure on proposals/evaluations scan).

use std::collections::BTreeSet;

use clap::ValueEnum;
use tracing::{debug, trace, warn};

use crate::cli::telemetry_cmd::compare as compare_helpers;
use crate::domain::Ranking;
use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::storage::sqlite::{Db, RunAggregate, RunRow};

/// CLI arguments for `moagan diff <run_a> <run_b>`.
///
/// `format` is `Option<DiffFormat>` (default `text`) so callers who
/// only want canonical text output do not have to type `--format
/// text`. `include_proposals` opts the operator into a per-proposal
/// breakdown (proposal id + score delta); without it, only the
/// aggregate `proposals` count is reported.
#[derive(Debug, Clone)]
pub struct DiffArgs {
    /// First run id (UUID v7).
    pub run_a: String,
    /// Second run id (UUID v7).
    pub run_b: String,
    /// Output format. `None` defaults to [`DiffFormat::Text`].
    pub format: Option<DiffFormat>,
    /// Emit per-proposal breakdown for the ranking delta.
    pub include_proposals: bool,
    /// Explicit home override. When `Some`, the dispatcher
    /// uses the given `MoaganHome` instead of resolving
    /// `MOAGAN_HOME` from the environment. Production callers
    /// leave this `None`; tests set it to bypass the global
    /// env var (the parallel-test race on `MOAGAN_HOME`
    /// surfaces as a spurious `Error::InvalidState` when two
    /// tests share the same `meta.sqlite` file via env-var
    /// mutation). Mirrors `RepairArgs::home_override` from
    /// PR #129.
    #[doc(hidden)]
    pub home_override: Option<MoaganHome>,
}

/// Output format for `moagan diff`. Mirrors the spectrum already
/// shipped by `moagan telemetry export` (`text` / `md` / `json`),
/// so operators can pipe the JSON straight into `jq`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum DiffFormat {
    /// Human-readable text table. Default.
    #[default]
    Text,
    /// Markdown table — pasteable into issues / PRs.
    Md,
    /// Machine-readable JSON document.
    Json,
}

impl std::fmt::Display for DiffFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Md => "md",
            Self::Json => "json",
        })
    }
}

impl std::str::FromStr for DiffFormat {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "md" | "markdown" => Ok(Self::Md),
            "json" => Ok(Self::Json),
            other => Err(Error::InvalidArgs(format!(
                "invalid diff format '{other}' (expected 'text', 'md', or 'json')"
            ))),
        }
    }
}

/// Run the cross-run comparison. Returns the process exit code so
/// the central dispatcher can map `Error` variants onto `ExitCode`
/// (T01-06 §12.3).
pub fn run(args: DiffArgs) -> Result<i32> {
    let DiffArgs {
        run_a,
        run_b,
        format,
        include_proposals,
        home_override,
    } = args;
    let format = format.unwrap_or_default();
    debug!(
        run_a = %run_a,
        run_b = %run_b,
        ?format,
        include_proposals,
        "diff::run: enter"
    );

    // 1. Parse + validate run ids up-front. A self-diff is rejected
    //    here (operator error: trivially-empty result). Malformed
    //    run ids map to Error::InvalidArgs (exit 2) per the CLI
    //    contract.
    let a = parse_run_id(&run_a)?;
    let b = parse_run_id(&run_b)?;
    if a == b {
        warn!(run_id = %a, "diff: self-diff requested");
        return Err(Error::InvalidArgs(
            "cannot diff a run against itself".into(),
        ));
    }

    // 2. Resolve home + open the index. `home_override` lets
    //    tests inject a tempdir directly so they don't race
    //    other parallel tests that mutate MOAGAN_HOME; the
    //    CLI dispatcher always passes `None` so production
    //    behaviour is unchanged. Mirrors the same override
    //    pattern used by `RepairArgs` (PR #129).
    let home = home_override.unwrap_or(MoaganHome::resolve()?);
    let db = Db::open(&home.meta_db_path())?;

    // 3. Load both runs + aggregates. A missing run_id surfaces as
    //    Error::InvalidState (exit 2 via the dispatcher wrapper)
    //    so CI scripts can distinguish "I don't know that run" from
    //    "you typed garbage".
    let row_a = db
        .get_run(a)?
        .ok_or_else(|| Error::InvalidState(format!("run {run_a} not found in the index")))?;
    let row_b = db
        .get_run(b)?
        .ok_or_else(|| Error::InvalidState(format!("run {run_b} not found in the index")))?;
    let agg_a = db.run_aggregate(a)?;
    let agg_b = db.run_aggregate(b)?;

    // 4. Filesystem-aware metrics. Each helper returns Result so a
    //    broken disk drives Error::Io (exit 8) instead of silently
    //    zeroing out.
    let proposals_a = count_files_in(home.run_dir(a).proposals())?;
    let proposals_b = count_files_in(home.run_dir(b).proposals())?;
    let evaluations_a = count_files_in(home.run_dir(a).evaluations())?;
    let evaluations_b = count_files_in(home.run_dir(b).evaluations())?;
    let phases_visited_a = db.list_completed_phases(a)?.len() as i64;
    let phases_visited_b = db.list_completed_phases(b)?.len() as i64;
    let ranking_delta = ranking_delta(&home, &a, &b)?;

    // 5. Dispatch to the requested renderer.
    match format {
        DiffFormat::Text => print_text(
            &row_a,
            &agg_a,
            &row_b,
            &agg_b,
            proposals_a,
            proposals_b,
            evaluations_a,
            evaluations_b,
            phases_visited_a,
            phases_visited_b,
            &ranking_delta,
            include_proposals,
        ),
        DiffFormat::Md => print_md(
            &row_a,
            &agg_a,
            &row_b,
            &agg_b,
            proposals_a,
            proposals_b,
            evaluations_a,
            evaluations_b,
            phases_visited_a,
            phases_visited_b,
            &ranking_delta,
            include_proposals,
        ),
        DiffFormat::Json => print_json(
            &row_a,
            &agg_a,
            &row_b,
            &agg_b,
            proposals_a,
            proposals_b,
            evaluations_a,
            evaluations_b,
            phases_visited_a,
            phases_visited_b,
            &ranking_delta,
            include_proposals,
        )?,
    }
    Ok(0)
}

/// Parse a `String` coming off the CLI into a [`RunId`]. The error
/// variant is [`Error::InvalidArgs`] so a typo yields exit code 2 —
/// the same code used for the missing-file case in
/// `moagan validate`, giving operators one consistent failure
/// surface across both pre-flight commands.
pub(crate) fn parse_run_id(s: &str) -> Result<RunId> {
    trace!(raw = s, "parse_run_id: enter");
    let res = s
        .parse()
        .map_err(|e| Error::InvalidArgs(format!("invalid run id '{s}': {e}")));
    match &res {
        Ok(id) => debug!(run_id = %id, "parse_run_id: ok"),
        Err(e) => warn!(error = %e, "parse_run_id: error"),
    }
    res
}

/// Count the number of `*.json` files inside `dir`. Returns 0 when
/// the directory does not exist (the run may still be alive but the
/// phase hasn't run yet). Other I/O failures propagate as
/// [`Error::Io`] so a permission error doesn't masquerade as zero.
fn count_files_in(dir: std::path::PathBuf) -> Result<usize> {
    trace!(dir = %dir.display(), "count_files_in: enter");
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("json") {
            count = count.checked_add(1).ok_or_else(|| {
                Error::Io(crate::error::IoError::Raw(std::io::Error::other(
                    "proposal/evaluation count overflow",
                )))
            })?;
        }
    }
    Ok(count)
}

/// Basic delta between two rankings: added, removed, and which ids
/// moved between positions / scores. Missing files on either side
/// collapse the diff to "no ranking delta available" rather than
/// erroring — a run can be mid-flight and legitimately lack a
/// `ranking.json` yet.
fn ranking_delta(home: &MoaganHome, a: &RunId, b: &RunId) -> Result<RankingDelta> {
    let ra = load_ranking(home.run_dir(*a).rankings().join("ranking.json"));
    let rb = load_ranking(home.run_dir(*b).rankings().join("ranking.json"));
    Ok(diff_rankings(ra.as_ref(), rb.as_ref()))
}

/// Read + parse a `rankings/ranking.json`. Returns `None` when
/// the file does not exist (a mid-flight run) — every other I/O
/// error surfaces a warning on stderr and the comparison proceeds
/// with the missing side as `None`.
fn load_ranking(path: std::path::PathBuf) -> Option<Ranking> {
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Ranking>(&text).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("diff: could not read {}: {e}", path.display());
            None
        }
    }
}

/// Per-proposal ranking diff. Counts entries by id on each side,
/// records when an id appears in only one ranking (added/removed),
/// and surfaces score deltas for ids present in both.
#[derive(Debug, Clone, Default)]
struct RankingDelta {
    /// New ids in `b` that weren't in `a`'s ranking.
    added: Vec<String>,
    /// Ids that dropped out of `b` (present in `a` only).
    removed: Vec<String>,
    /// Ids present in both, with their score delta (`b - a`).
    changed: Vec<(String, f32, f32, f32)>,
    /// Whether either side had a parseable ranking file.
    has_ranking: bool,
}

fn diff_rankings(a: Option<&Ranking>, b: Option<&Ranking>) -> RankingDelta {
    let a_rank = a.map(ranked_ids);
    let b_rank = b.map(ranked_ids);
    let has_ranking = a.is_some() || b.is_some();
    let (Some(a_ids), Some(b_ids)) = (a_rank.as_ref(), b_rank.as_ref()) else {
        return RankingDelta {
            has_ranking,
            ..Default::default()
        };
    };

    let added: Vec<String> = b_ids.difference(a_ids).cloned().collect();
    let removed: Vec<String> = a_ids.difference(b_ids).cloned().collect();
    let mut changed = Vec::new();
    for id in a_ids.intersection(b_ids) {
        let sa = score_for(a.unwrap(), id);
        let sb = score_for(b.unwrap(), id);
        // Round to 4 decimal places so the printout doesn't print
        // noise; the underlying `f32` arithmetic stays unchanged
        // for the JSON output.
        let raw_delta = (sb - sa).round_to(4);
        if raw_delta.abs() > 0.0001 {
            changed.push((id.clone(), sa, sb, raw_delta));
        }
    }
    changed.sort_by(|x, y| y.3.partial_cmp(&x.3).unwrap_or(std::cmp::Ordering::Equal));
    RankingDelta {
        added,
        removed,
        changed,
        has_ranking,
    }
}

fn ranked_ids(ranking: &Ranking) -> BTreeSet<String> {
    r_rank_ids(ranking)
}

fn r_rank_ids(ranking: &Ranking) -> BTreeSet<String> {
    ranking.ranked.iter().map(|e| e.id.clone()).collect()
}

fn score_for(ranking: &Ranking, id: &str) -> f32 {
    ranking
        .ranked
        .iter()
        .find(|e| e.id == id)
        .map(|e| e.score)
        .unwrap_or(0.0)
}

/// Helper trait that clamps f32 arithmetic to 4 decimal places for
/// human display. The plain `f32` is kept in the JSON output so
/// downstream tooling sees the full precision.
trait RoundTo {
    fn round_to(self, decimals: u32) -> Self;
}

impl RoundTo for f32 {
    fn round_to(self, decimals: u32) -> Self {
        let factor = 10f32.powi(decimals as i32);
        (self * factor).round() / factor
    }
}

/// Human-readable text renderer. Reuses
/// [`compare_helpers::print_side_by_side`] and
/// [`compare_helpers::print_diff`] for the baseline eleven metrics
/// the existing `TelemetryCmd::Compare` covers, then appends an
/// "additional metrics" section for the four filesystem-aware
/// dimensions tracked here.
#[allow(clippy::too_many_arguments)]
fn print_text(
    row_a: &RunRow,
    agg_a: &RunAggregate,
    row_b: &RunRow,
    agg_b: &RunAggregate,
    proposals_a: usize,
    proposals_b: usize,
    evaluations_a: usize,
    evaluations_b: usize,
    phases_visited_a: i64,
    phases_visited_b: i64,
    ranking_delta: &RankingDelta,
    include_proposals: bool,
) {
    println!("=== moagan diff (text) — D.14.2 ===");
    compare_helpers::print_side_by_side(row_a, agg_a, row_b, agg_b);
    println!();
    println!("--- baseline metrics ---");
    compare_helpers::print_diff("tokens", agg_a.total_tokens(), agg_b.total_tokens());
    compare_helpers::print_diff("calls", agg_a.calls, agg_b.calls);
    compare_helpers::print_diff("ok_calls", agg_a.ok_calls(), agg_b.ok_calls());
    compare_helpers::print_diff("error_calls", agg_a.error_calls, agg_b.error_calls);
    compare_helpers::print_diff("timeout_calls", agg_a.timeout_calls, agg_b.timeout_calls);
    compare_helpers::print_diff(
        "cancelled_calls",
        agg_a.cancelled_calls,
        agg_b.cancelled_calls,
    );
    compare_helpers::print_diff("providers", agg_a.provider_count, agg_b.provider_count);
    compare_helpers::print_diff("phases", agg_a.phase_count, agg_b.phase_count);
    compare_helpers::print_diff("warnings", agg_a.warnings, agg_b.warnings);
    compare_helpers::print_diff("checkpoints", agg_a.checkpoints, agg_b.checkpoints);
    let dur_a = row_a.updated_unix.saturating_sub(row_a.created_unix).max(0);
    let dur_b = row_b.updated_unix.saturating_sub(row_b.created_unix).max(0);
    compare_helpers::print_diff("duration_secs", dur_a, dur_b);

    println!();
    println!("--- additional metrics ---");
    compare_helpers::print_diff("proposals", proposals_a as i64, proposals_b as i64);
    compare_helpers::print_diff("evaluations", evaluations_a as i64, evaluations_b as i64);
    compare_helpers::print_diff("phases_visited", phases_visited_a, phases_visited_b);

    print_ranking_delta_text(ranking_delta, include_proposals);
}

/// Markdown renderer. Builds a single table with all metrics so the
/// output is pasteable into an issue / PR. The ranking delta gets
/// either a single status line (default) or a full table when
/// `--include-proposals` is set.
#[allow(clippy::too_many_arguments)]
fn print_md(
    row_a: &RunRow,
    agg_a: &RunAggregate,
    row_b: &RunRow,
    agg_b: &RunAggregate,
    proposals_a: usize,
    proposals_b: usize,
    evaluations_a: usize,
    evaluations_b: usize,
    phases_visited_a: i64,
    phases_visited_b: i64,
    ranking_delta: &RankingDelta,
    include_proposals: bool,
) {
    println!("# moagan diff (D.14.2)");
    println!();
    let id_a = short(&row_a.run_id);
    let id_b = short(&row_b.run_id);
    println!("| metric | a (`{id_a}`) | b (`{id_b}`) | delta |");
    println!("|---|---:|---:|---:|");
    md_row("mode", &row_a.mode, &row_b.mode);
    md_row("status", &row_a.status, &row_b.status);
    md_row_int("tokens", agg_a.total_tokens(), agg_b.total_tokens());
    md_row_int("calls", agg_a.calls, agg_b.calls);
    md_row_int("ok_calls", agg_a.ok_calls(), agg_b.ok_calls());
    md_row_int("error_calls", agg_a.error_calls, agg_b.error_calls);
    md_row_int("timeout_calls", agg_a.timeout_calls, agg_b.timeout_calls);
    md_row_int(
        "cancelled_calls",
        agg_a.cancelled_calls,
        agg_b.cancelled_calls,
    );
    md_row_int("providers", agg_a.provider_count, agg_b.provider_count);
    md_row_int("phases", agg_a.phase_count, agg_b.phase_count);
    md_row_int("warnings", agg_a.warnings, agg_b.warnings);
    md_row_int("checkpoints", agg_a.checkpoints, agg_b.checkpoints);
    let dur_a = row_a.updated_unix.saturating_sub(row_a.created_unix).max(0);
    let dur_b = row_b.updated_unix.saturating_sub(row_b.created_unix).max(0);
    md_row_int("duration_secs", dur_a, dur_b);
    md_row_int("proposals", proposals_a as i64, proposals_b as i64);
    md_row_int("evaluations", evaluations_a as i64, evaluations_b as i64);
    md_row_int("phases_visited", phases_visited_a, phases_visited_b);

    println!();
    if !ranking_delta.has_ranking {
        println!("_ranking delta: not available (no `rankings/ranking.json` on either side)_");
    } else if ranking_delta.added.is_empty()
        && ranking_delta.removed.is_empty()
        && ranking_delta.changed.is_empty()
    {
        println!("_ranking delta: unchanged (same winner and score)_");
    } else {
        if !include_proposals {
            println!(
                "_ranking delta: {} added, {} removed, {} changed — pass `--include-proposals` for details_",
                ranking_delta.added.len(),
                ranking_delta.removed.len(),
                ranking_delta.changed.len(),
            );
        } else {
            if !ranking_delta.added.is_empty() {
                println!("**added**: {}", ranking_delta.added.join(", "));
            }
            if !ranking_delta.removed.is_empty() {
                println!("**removed**: {}", ranking_delta.removed.join(", "));
            }
            if !ranking_delta.changed.is_empty() {
                println!();
                println!("| proposal | score a | score b | delta |");
                println!("|---|---:|---:|---:|");
                for (id, sa, sb, d) in &ranking_delta.changed {
                    let sign = if *d > 0.0 { "+" } else { "" };
                    println!("| {id} | {sa:.4} | {sb:.4} | {sign}{d:.4} |");
                }
            }
        }
    }
}

fn md_row(label: &str, a: &str, b: &str) {
    println!("| {label} | {a} | {b} | — |");
}

fn md_row_int(label: &str, a: i64, b: i64) {
    let delta = b - a;
    let sign = if delta > 0 { "+" } else { "" };
    println!("| {label} | {a} | {b} | {sign}{delta} |");
}

/// JSON renderer. Emits a single object with `a`, `b`, `deltas`,
/// and (when available) `ranking`. Stable field order so callers
/// can pin a JSON schema in CI.
#[allow(clippy::too_many_arguments)]
fn print_json(
    row_a: &RunRow,
    agg_a: &RunAggregate,
    row_b: &RunRow,
    agg_b: &RunAggregate,
    proposals_a: usize,
    proposals_b: usize,
    evaluations_a: usize,
    evaluations_b: usize,
    phases_visited_a: i64,
    phases_visited_b: i64,
    ranking_delta: &RankingDelta,
    include_proposals: bool,
) -> Result<()> {
    let payload = serde_json::json!({
        "format": "diff",
        "spec": "D.14.2",
        "a": run_payload(row_a, agg_a, proposals_a, evaluations_a, phases_visited_a),
        "b": run_payload(row_b, agg_b, proposals_b, evaluations_b, phases_visited_b),
        "deltas": {
            "tokens": agg_b.total_tokens() - agg_a.total_tokens(),
            "calls": agg_b.calls - agg_a.calls,
            "ok_calls": agg_b.ok_calls() - agg_a.ok_calls(),
            "error_calls": agg_b.error_calls - agg_a.error_calls,
            "timeout_calls": agg_b.timeout_calls - agg_a.timeout_calls,
            "cancelled_calls": agg_b.cancelled_calls - agg_a.cancelled_calls,
            "providers": agg_b.provider_count - agg_a.provider_count,
            "phases": agg_b.phase_count - agg_a.phase_count,
            "warnings": agg_b.warnings - agg_a.warnings,
            "checkpoints": agg_b.checkpoints - agg_a.checkpoints,
            "duration_secs":
                (row_b.updated_unix.saturating_sub(row_b.created_unix).max(0))
                - (row_a.updated_unix.saturating_sub(row_a.created_unix).max(0)),
            "proposals": (proposals_b as i64) - (proposals_a as i64),
            "evaluations": (evaluations_b as i64) - (evaluations_a as i64),
            "phases_visited": phases_visited_b - phases_visited_a,
        },
        "ranking": ranking_payload(ranking_delta, include_proposals),
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn run_payload(
    row: &RunRow,
    agg: &RunAggregate,
    proposals: usize,
    evaluations: usize,
    phases_visited: i64,
) -> serde_json::Value {
    serde_json::json!({
        "run_id": row.run_id,
        "mode": row.mode,
        "status": row.status,
        "created_unix": row.created_unix,
        "updated_unix": row.updated_unix,
        "tokens": agg.total_tokens(),
        "calls": agg.calls,
        "ok_calls": agg.ok_calls(),
        "error_calls": agg.error_calls,
        "timeout_calls": agg.timeout_calls,
        "cancelled_calls": agg.cancelled_calls,
        "providers": agg.provider_count,
        "phases": agg.phase_count,
        "warnings": agg.warnings,
        "checkpoints": agg.checkpoints,
        "duration_secs": row.updated_unix.saturating_sub(row.created_unix).max(0),
        "proposals": proposals,
        "evaluations": evaluations,
        "phases_visited": phases_visited,
    })
}

fn ranking_payload(ranking_delta: &RankingDelta, include_proposals: bool) -> serde_json::Value {
    if !ranking_delta.has_ranking {
        return serde_json::json!({"available": false});
    }
    let changed: Vec<serde_json::Value> = ranking_delta
        .changed
        .iter()
        .map(|(id, sa, sb, _d)| {
            serde_json::json!({
                "id": id,
                "score_a": sa,
                "score_b": sb,
                "delta": sb - sa,
            })
        })
        .collect();
    let mut payload = serde_json::json!({
        "available": true,
        "added": ranking_delta.added,
        "removed": ranking_delta.removed,
        "changed_count": ranking_delta.changed.len(),
    });
    if include_proposals {
        payload["changed"] = serde_json::Value::Array(changed);
    }
    payload
}

fn print_ranking_delta_text(ranking_delta: &RankingDelta, include_proposals: bool) {
    println!();
    println!("--- ranking delta ---");
    if !ranking_delta.has_ranking {
        println!("  (no ranking.json on either side)");
        return;
    }
    if ranking_delta.added.is_empty()
        && ranking_delta.removed.is_empty()
        && ranking_delta.changed.is_empty()
    {
        println!("  unchanged: same winner, same scores");
        return;
    }
    println!("  added: {} ", fmt_set(&ranking_delta.added));
    println!("  removed: {}", fmt_set(&ranking_delta.removed));
    if include_proposals {
        println!("  changed ({} proposal(s)):", ranking_delta.changed.len());
        for (id, sa, sb, d) in &ranking_delta.changed {
            let sign = if *d > 0.0 { "+" } else { "" };
            println!("    {id:<22}  a={sa:.4}  b={sb:.4}  delta={sign}{d:.4}");
        }
    } else {
        println!(
            "  changed: {} (pass --include-proposals for details)",
            ranking_delta.changed.len()
        );
    }
}

fn fmt_set(s: &[String]) -> String {
    if s.is_empty() {
        "(none)".into()
    } else {
        s.join(", ")
    }
}

fn short(raw: &str) -> &str {
    raw.get(..8).unwrap_or(raw)
}

/// Convenience exposed for the tests so they can drive the helper
/// without re-implementing it.
#[cfg(test)]
pub(crate) fn _count_files_in_for_tests(dir: std::path::PathBuf) -> Result<usize> {
    count_files_in(dir)
}

/// Re-export the directory walker so the test module can poke at
/// the filesystem without going through the (run-aware) `count_files_in`.
#[cfg(test)]
pub(crate) fn _walk_dir_for_tests(dir: &std::path::Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().is_file() {
            count = count.checked_add(1).ok_or_else(|| {
                Error::Io(crate::error::IoError::Raw(std::io::Error::other(
                    "walk overflow",
                )))
            })?;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RankEntry;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn diff_format_parses_text_md_json() {
        let cases = [
            ("text", DiffFormat::Text),
            ("md", DiffFormat::Md),
            ("json", DiffFormat::Json),
            ("markdown", DiffFormat::Md),
        ];
        for (raw, expected) in cases {
            let parsed: DiffFormat = raw.parse().unwrap_or_else(|_| panic!("must parse {raw}"));
            assert_eq!(parsed, expected);
        }
        // Default == Text.
        assert_eq!(DiffFormat::default(), DiffFormat::Text);
        // Unknown format surfaces a parse error.
        assert!("xml".parse::<DiffFormat>().is_err());
    }

    #[test]
    fn diff_format_display_matches_value_enum() {
        assert_eq!(DiffFormat::Text.to_string(), "text");
        assert_eq!(DiffFormat::Md.to_string(), "md");
        assert_eq!(DiffFormat::Json.to_string(), "json");
    }

    /// Self-diff must surface as `Error::InvalidArgs` so the CLI
    /// contract maps it to exit code 2 — same failure mode as a
    /// missing brief in `moagan validate`.
    #[test]
    fn diff_self_run_returns_invalid_args() {
        let tmp = tempdir().expect("tmpdir");
        let cfg_path = tmp.path().join("MOAGAN_HOME");
        std::fs::create_dir_all(&cfg_path).expect("mkdir");
        // Inject the tempdir directly via home_override so we
        // don't race other parallel tests that mutate
        // MOAGAN_HOME. Same pattern as
        // `diff_unknown_run_returns_invalid_state` below and
        // `RepairArgs::home_override` (PR #129).
        let home = MoaganHome::at(cfg_path);
        let same = "01900000-0000-0000-0000-000000000000";
        let args = DiffArgs {
            run_a: same.into(),
            run_b: same.into(),
            format: None,
            include_proposals: false,
            home_override: Some(home),
        };
        let err = run(args).expect_err("self-diff must error");
        assert!(
            matches!(err, Error::InvalidArgs(_)),
            "expected Error::InvalidArgs, got {err:?}"
        );
        assert_eq!(err.exit_code() as i32, 2);
    }

    /// A well-formed but unregistered run id surfaces as
    /// `Error::InvalidState` (the run is absent from the index).
    /// Distinguishing this from `Error::InvalidArgs` lets CI
    /// scripts separate "you mistyped the id" from "the run is
    /// gone".
    #[test]
    fn diff_unknown_run_returns_invalid_state() {
        let tmp = tempdir().expect("tmpdir");
        let cfg_path = tmp.path().join("MOAGAN_HOME");
        std::fs::create_dir_all(&cfg_path).expect("mkdir");
        // Inject the tempdir directly via home_override so we
        // don't race other parallel tests that mutate
        // MOAGAN_HOME. Same pattern as `RepairArgs::home_override`
        // (PR #129); see the `reindex_no_diff_returns_zero` test
        // in src/cli/repair.rs for the long-form rationale.
        let home = MoaganHome::at(cfg_path);
        let args = DiffArgs {
            run_a: "01900000-0000-0000-0000-000000000000".into(),
            run_b: "01900000-0000-0000-0000-000000000001".into(),
            format: None,
            include_proposals: false,
            home_override: Some(home),
        };
        let err = run(args).expect_err("unknown run must error");
        assert!(
            matches!(err, Error::InvalidState(_)),
            "expected Error::InvalidState, got {err:?}"
        );
    }

    /// Filesystem scan counts files under a run's `proposals/`
    /// directory. Three dummy files = count of 3 (plus an extra on
    /// the b side to demonstrate per-id accounting even though the
    /// count helper only returns the total).
    /// Also pins the missing-directory fallback (count = 0) so a
    /// mid-flight run with no proposals yet does not blow up.
    #[test]
    fn diff_count_proposals_walks_directory() {
        let tmp = tempdir().expect("tmpdir");
        let proposals_dir: PathBuf = tmp.path().join("proposals");
        std::fs::create_dir_all(&proposals_dir).expect("mkdir");

        for n in 0..3 {
            let f = proposals_dir.join(format!("p_{n}.json"));
            std::fs::write(&f, b"{\"id\": \"p_a\"}").expect("write dummy proposal a");
        }
        std::fs::write(
            proposals_dir.join("p_99_extra.json"),
            b"{\"id\": \"p_b_extra\"}",
        )
        .expect("write dummy proposal b");

        let count = _walk_dir_for_tests(&proposals_dir).expect("walk");
        assert_eq!(count, 4, "all 4 dummy files must be counted");

        // Missing dir resolves to zero (no panic).
        let missing: PathBuf = tmp.path().join("nope");
        let count_missing = _walk_dir_for_tests(&missing).expect("missing walk");
        assert_eq!(count_missing, 0, "missing dir must count as 0");
    }

    /// `count_files_in` must distinguish between `*.json` files
    /// (counted) and other extensions (skipped) so a future `*.tmp`
    /// or `*.lock` file inside `proposals/` does not inflate the
    /// count.
    #[test]
    fn diff_count_files_filters_by_json_extension() {
        let tmp = tempdir().expect("tmpdir");
        let dir = tmp.path().join("proposals");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("good.json"), b"{}").expect("write good");
        std::fs::write(dir.join("bad.tmp"), b"{}").expect("write tmp");
        std::fs::write(dir.join("log.txt"), b"x").expect("write txt");
        let n = count_files_in(dir).expect("count");
        assert_eq!(n, 1, "only good.json should count");
    }

    /// Ranking delta reduces a synthetic pair: one entry on each
    /// side with overlapping + unique ids.
    #[test]
    fn diff_ranking_delta_marks_added_removed_changed() {
        let ra = Ranking {
            ranked: vec![
                RankEntry {
                    id: "shared".into(),
                    score: 8.0,
                    reason: String::new(),
                },
                RankEntry {
                    id: "drop".into(),
                    score: 5.0,
                    reason: String::new(),
                },
            ],
            ..Default::default()
        };
        let rb = Ranking {
            ranked: vec![
                RankEntry {
                    id: "shared".into(),
                    score: 9.0,
                    reason: String::new(),
                },
                RankEntry {
                    id: "new".into(),
                    score: 7.0,
                    reason: String::new(),
                },
            ],
            ..Default::default()
        };
        let delta = diff_rankings(Some(&ra), Some(&rb));
        assert_eq!(delta.added, vec!["new".to_string()]);
        assert_eq!(delta.removed, vec!["drop".to_string()]);
        // `shared` moved 8.0 -> 9.0 (delta ~+1), so it is in `changed`.
        assert!(
            delta.changed.iter().any(|(id, sa, sb, d)| id == "shared"
                && ((*sa - 8.0).abs() < 0.001)
                && ((*sb - 9.0).abs() < 0.001)
                && (*d - 1.0).abs() < 0.01),
            "expected shared to show a +1.0 delta, got {:?}",
            delta.changed
        );
    }
}
