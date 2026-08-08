//! Human checkpoints (Phase D — V4 §5.14 + T01-06 §6.5).
//!
//! This module owns the prompt/response lifecycle for any phase that
//! wants to pause and ask the user a question before continuing. The
//! `AGENTS.md` no-go list forbids `dialoguer` and `inquire`, so the
//! implementation reads from `std::io::stdin()` directly.
//!
//! Per V4 §5.14:
//!
//! - No timeout on user inactivity. The user may take as long as they
//!   need.
//! - The answer is persisted verbatim so re-runs of the same brief
//!   can be audited.
//! - Modes opt-in/out: `--mode batch` skips checkpoints, interactive
//!   modes (`standard`, `deep`) show the final-checkpoint prompt.
//!
//! Per T01-06 §0.5#13 the original spec used `dialoguer::Input`
//! without a `tokio::time::timeout`. We keep that contract: the read
//! is blocking (no timeout).

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::HumanCheckpoint;
use crate::error::{Error, Result};
use crate::time::now_unix_secs;

/// Closed enum of when a checkpoint may fire. Mirrors the SQLite
/// CHECK constraint in `proposal-02-rust.md §2.1` so the two stay
/// in lock-step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// Fired at the end of `IntakePhase`.
    Intake,
    /// Fired at the end of `ClarifyPhase` when the brief has a
    /// blocking ambiguity.
    Clarify,
    /// Fired at the end of `DeliverPhase` to confirm the run should
    /// terminate.
    Final,
    /// Fired at the end of `DiscoverSummaryPhase` (V4 §6.11 +
    /// T01-06 §9.11). Carries the discovery roll-up counts so the
    /// question text can show "N categories, M facets, K
    /// contradictions" without re-reading the disk sidecars. The
    /// counts are not part of the persisted kind string (the SQLite
    /// `kind` column is the bare `"discovery"` token — the counts
    /// travel through the question text and the checkpoint id).
    Discovery {
        /// Number of `final/cat_NN.json` documents produced.
        cat_count: usize,
        /// Number of facet lists in `facets/`.
        facet_count: usize,
        /// Number of `Contradiction` entries in
        /// `contradictions/contradictions.json`.
        contradictions: usize,
    },
    /// Fired at any other point the pipeline defines.
    Custom,
}

impl CheckpointKind {
    /// Stable lowercase string used in the persisted JSON and the
    /// SQLite column. Discovery collapses to `"discovery"`; the
    /// roll-up counts travel through the question text and the
    /// sidecar's `id`, never through the kind token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Clarify => "clarify",
            Self::Final => "final",
            Self::Discovery { .. } => "discovery",
            Self::Custom => "custom",
        }
    }

    /// Phase that owns this checkpoint. Mirrors the SQLite `phase`
    /// column for telemetry indexing.
    pub fn phase_name(&self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Clarify => "clarify",
            Self::Final => "deliver",
            Self::Discovery { .. } => "discover_summary",
            Self::Custom => "custom",
        }
    }
}

impl std::fmt::Display for CheckpointKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CheckpointKind {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "intake" => Ok(Self::Intake),
            "clarify" => Ok(Self::Clarify),
            "final" => Ok(Self::Final),
            // The roll-up counts are not part of the wire form —
            // see the `Discovery` variant's doc. `FromStr` round-trip
            // therefore collapses to the all-zero triple; callers
            // that need real counts build the variant directly.
            "discovery" => Ok(Self::Discovery {
                cat_count: 0,
                facet_count: 0,
                contradictions: 0,
            }),
            "custom" => Ok(Self::Custom),
            other => Err(Error::InvalidArgs(format!(
                "unknown checkpoint kind: {other}"
            ))),
        }
    }
}

/// What the user (or CI script) said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "value")]
pub enum Resolution {
    /// User typed `y` / `yes` / `Y` (or hit enter on a yes/no
    /// prompt). The default answer was accepted.
    Approved,
    /// User typed `n` / `no` / `N`. The run is cancelled.
    Rejected,
    /// User typed a free-form text answer (e.g. a constraint or an
    /// edit). The text is captured verbatim.
    ///
    /// Call sites that match this variant should call
    /// [`crate::checkpoint::persist_modify_note`] to persist the
    /// text to `<run_dir>/state/modify_note.json` (F1 wire-up) so
    /// the rank and deliver phases can prepend the operator's
    /// correction to their LLM prompts on the next cycle.
    Modify(String),
}

impl Resolution {
    /// True when the user accepted the default / approved.
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }

    /// True when the user typed a free-form text answer.
    pub fn is_modify(&self) -> bool {
        matches!(self, Self::Modify(_))
    }
}

/// What the pipeline wants to ask.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Stable id (assigned by [`ask`] when persisting).
    pub id: String,
    /// Kind, drives the SQLite enum and the phase label.
    pub kind: CheckpointKind,
    /// Question shown verbatim to the user.
    pub question: String,
    /// Default answer when the user just hits enter. `true` =>
    /// approve, `false` => reject.
    pub default_yes: bool,
}

impl Checkpoint {
    /// Build a checkpoint with a fresh UUID v7 id. The id is part of
    /// the persisted file name (`h_<NN>.json`).
    pub fn new(kind: CheckpointKind, question: impl Into<String>, default_yes: bool) -> Self {
        Self {
            id: format!("h_{}", uuid::Uuid::now_v7().simple()),
            kind,
            question: question.into(),
            default_yes,
        }
    }

    /// Convenience: a yes/no prompt that defaults to yes.
    pub fn yes_no(kind: CheckpointKind, question: impl Into<String>) -> Self {
        Self::new(kind, question, true)
    }
}

/// Runtime options that gate the prompt.
#[derive(Debug, Clone, Default)]
pub struct CheckpointOpts {
    /// `true` for `standard` / `deep`, `false` for `batch` /
    /// `--non-interactive`. When `false`, [`ask`] is a no-op that
    /// returns `Resolution::Approved` without touching stdin.
    pub interactive: bool,
    /// When set, [`ask`] writes to this path (used by tests that
    /// want to inject a pre-canned answer). When `None`, the call
    /// reads from `std::io::stdin()` directly.
    pub stdin_override: Option<String>,
    /// Phase D sub-fase #6: when `Some`, every captured checkpoint
    /// is mirrored into the SQLite index via
    /// `Telemetry::record_checkpoint`. When `None` (tests), only
    /// the JSON sidecar is written. Best-effort: failures are
    /// logged inside `record_checkpoint` and never abort the run.
    pub telemetry: Option<crate::telemetry::Telemetry>,
}
impl CheckpointOpts {
    /// Non-interactive constructor — used by `--non-interactive` and
    /// `Mode::Batch`.
    pub fn non_interactive() -> Self {
        Self {
            interactive: false,
            stdin_override: None,
            telemetry: None,
        }
    }

    /// Build an opts with a pre-canned stdin response (tests only).
    pub fn with_stdin_override(response: impl Into<String>) -> Self {
        Self {
            interactive: true,
            stdin_override: Some(response.into()),
            telemetry: None,
        }
    }

    /// Attach the telemetry handle so the captured checkpoint is
    /// mirrored to SQLite. Called by the phase wiring (intake,
    /// clarify, deliver) which already hold a `Telemetry`
    /// via `RunContext`.
    pub fn with_telemetry(mut self, telemetry: crate::telemetry::Telemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }
}

/// Skip the prompt entirely. The captured answer is treated as
/// "approved" (default) and is persisted to disk so the audit trail
/// records that the prompt was suppressed, not that the user typed
/// nothing.
///
/// When `telemetry` is `Some`, the captured checkpoint is also
/// mirrored into the SQLite index via
/// `Telemetry::record_checkpoint` (Phase D sub-fase #6).
pub fn skip(
    checkpoint: &Checkpoint,
    dir: &Path,
    telemetry: Option<&crate::telemetry::Telemetry>,
) -> Result<Resolution> {
    let captured = HumanCheckpoint {
        id: checkpoint.id.clone(),
        phase: checkpoint.kind.phase_name().to_owned(),
        kind: checkpoint.kind.as_str().to_owned(),
        question: checkpoint.question.clone(),
        response: "<skipped:non_interactive>".to_owned(),
        at_unix: now_unix_secs(),
        accepted_default: checkpoint.default_yes,
        schema_version: "v1".to_owned(),
    };
    persist(dir, &captured, telemetry)?;
    Ok(if checkpoint.default_yes {
        Resolution::Approved
    } else {
        Resolution::Rejected
    })
}

/// Ask the user. Blocking on stdin. Returns the [`Resolution`] and
/// persists a `HumanCheckpoint` JSON sidecar.
///
/// When `opts.telemetry` is `Some`, the captured checkpoint is also
/// mirrored into the SQLite index via
/// `Telemetry::record_checkpoint` (Phase D sub-fase #6).
pub fn ask(checkpoint: &Checkpoint, dir: &Path, opts: &CheckpointOpts) -> Result<Resolution> {
    if !opts.interactive {
        return skip(checkpoint, dir, opts.telemetry.as_ref());
    }
    let (raw, accepted_default) = match opts.stdin_override.as_ref() {
        Some(s) => (s.clone(), false),
        None => read_line_interactive(checkpoint)?,
    };
    let parsed = parse_resolution(&raw, checkpoint.default_yes);
    let captured = HumanCheckpoint {
        id: checkpoint.id.clone(),
        phase: checkpoint.kind.phase_name().to_owned(),
        kind: checkpoint.kind.as_str().to_owned(),
        question: checkpoint.question.clone(),
        response: raw,
        at_unix: now_unix_secs(),
        accepted_default,
        schema_version: "v1".to_owned(),
    };
    persist(dir, &captured, opts.telemetry.as_ref())?;
    Ok(parsed)
}

fn read_line_interactive(checkpoint: &Checkpoint) -> Result<(String, bool)> {
    let suffix = if checkpoint.default_yes {
        "[Y/n]"
    } else {
        "[y/N]"
    };
    print!("[{}] {} {} ", checkpoint.kind, checkpoint.question, suffix);
    io::stdout().flush()?;
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).map_err(Error::from)?;
    let trimmed = line.trim().to_owned();
    let accepted_default = trimmed.is_empty();
    Ok((trimmed, accepted_default))
}

fn parse_resolution(raw: &str, default_yes: bool) -> Resolution {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return if default_yes {
            Resolution::Approved
        } else {
            Resolution::Rejected
        };
    }
    let lowered = trimmed.to_lowercase();
    // PR-20: the discovery checkpoint lists four actions
    // (`approve | review | block | export`) per V4 §6.11 / T01-06
    // §9.11. We recognise `approve` and `block` as the explicit
    // yes/no tokens so a user / CI script that types
    // `approve` (instead of the canonical `y`) still resolves to
    // `Approved`. `review` and `export` fall through to `Modify`
    // so the call site can persist them as a modify note.
    if matches!(lowered.as_str(), "y" | "yes" | "approve") {
        Resolution::Approved
    } else if matches!(lowered.as_str(), "n" | "no" | "block") {
        Resolution::Rejected
    } else {
        Resolution::Modify(trimmed.to_owned())
    }
}

fn persist(
    dir: &Path,
    checkpoint: &HumanCheckpoint,
    telemetry: Option<&crate::telemetry::Telemetry>,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path: PathBuf = dir.join(format!("{}.json", checkpoint.id));
    let json = serde_json::to_vec_pretty(checkpoint)?;
    crate::atomic::writer::AtomicWriter::new().write(&path, &json)?;
    // Phase D sub-fase #6: mirror to SQLite via Telemetry when
    // available. Best-effort — failures are logged inside
    // record_checkpoint and don't abort the run.
    if let Some(t) = telemetry {
        let _ = t.record_checkpoint(checkpoint);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trip() {
        for k in [
            CheckpointKind::Intake,
            CheckpointKind::Clarify,
            CheckpointKind::Final,
            CheckpointKind::Custom,
        ] {
            assert_eq!(k.as_str().parse::<CheckpointKind>().unwrap(), k);
        }
        // PR-20: the Discovery variant round-trips through the
        // all-zero collapse documented on `FromStr`. The roll-up
        // counts travel through the question text and the
        // checkpoint id, not through the kind token.
        let discovery = CheckpointKind::Discovery {
            cat_count: 0,
            facet_count: 0,
            contradictions: 0,
        };
        assert_eq!(
            discovery.as_str().parse::<CheckpointKind>().unwrap(),
            discovery
        );
    }

    #[test]
    fn parse_resolution_yes() {
        assert_eq!(parse_resolution("y", false), Resolution::Approved);
        assert_eq!(parse_resolution("Y", false), Resolution::Approved);
        assert_eq!(parse_resolution("yes", false), Resolution::Approved);
        // PR-20: the discovery checkpoint exposes `approve` as an
        // explicit yes token (V4 §6.11 / T01-06 §9.11) so CI
        // scripts can pipe the literal word without a
        // translation layer.
        assert_eq!(parse_resolution("approve", false), Resolution::Approved);
        assert_eq!(parse_resolution("APPROVE", false), Resolution::Approved);
    }

    #[test]
    fn parse_resolution_no() {
        assert_eq!(parse_resolution("n", true), Resolution::Rejected);
        assert_eq!(parse_resolution("no", true), Resolution::Rejected);
        assert_eq!(parse_resolution("N", true), Resolution::Rejected);
        // PR-20: `block` is the discovery checkpoint's explicit
        // no token.
        assert_eq!(parse_resolution("block", true), Resolution::Rejected);
        assert_eq!(parse_resolution("BLOCK", true), Resolution::Rejected);
    }

    #[test]
    fn parse_resolution_freeform_is_modify() {
        assert_eq!(
            parse_resolution("add constraint X", true),
            Resolution::Modify("add constraint X".to_owned())
        );
    }

    #[test]
    fn parse_resolution_empty_uses_default() {
        assert_eq!(parse_resolution("", true), Resolution::Approved);
        assert_eq!(parse_resolution("", false), Resolution::Rejected);
        // Whitespace-only input is treated as the empty default.
        // `read_line_interactive` trims before calling us, but the
        // contract is the same: whitespace -> default.
        assert_eq!(parse_resolution("   ", true), Resolution::Approved);
    }

    #[test]
    fn checkpoint_new_assigns_id_and_phase() {
        let c = Checkpoint::new(CheckpointKind::Clarify, "continue?", true);
        assert!(c.id.starts_with("h_"));
        assert_eq!(c.kind, CheckpointKind::Clarify);
        assert!(c.default_yes);
    }

    #[test]
    fn checkpoint_kind_phase_name_matches_pipeline() {
        assert_eq!(CheckpointKind::Intake.phase_name(), "intake");
        assert_eq!(CheckpointKind::Clarify.phase_name(), "clarify");
        assert_eq!(CheckpointKind::Final.phase_name(), "deliver");
        assert_eq!(CheckpointKind::Custom.phase_name(), "custom");
        // PR-20: discovery checkpoints are owned by the
        // `discover_summary` phase (V4 §6.11) so the SQLite
        // index surfaces them next to the rest of the discovery
        // timeline.
        assert_eq!(
            CheckpointKind::Discovery {
                cat_count: 0,
                facet_count: 0,
                contradictions: 0,
            }
            .phase_name(),
            "discover_summary"
        );
    }

    #[test]
    fn skip_persists_marker_and_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(CheckpointKind::Final, "ship it?", true);
        let res = skip(&c, tmp.path(), None).unwrap();
        assert_eq!(res, Resolution::Approved);
        // AtomicWriter emits both the data file and a `.meta.json`
        // sidecar. We only inspect the data file for this test.
        let data_path = tmp.path().join(format!("{}.json", c.id));
        assert!(data_path.exists());
        let json = std::fs::read_to_string(&data_path).unwrap();
        assert!(json.contains("<skipped:non_interactive>"));
        assert!(json.contains("\"accepted_default\": true"));
    }

    #[test]
    fn skip_persists_marker_default_no() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(CheckpointKind::Final, "ship it?", false);
        let res = skip(&c, tmp.path(), None).unwrap();
        assert_eq!(res, Resolution::Rejected);
        let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", c.id))).unwrap();
        assert!(json.contains("\"accepted_default\": false"));
    }

    #[test]
    fn ask_with_stdin_override_persists_response() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(CheckpointKind::Intake, "looks good?", true);
        let opts = CheckpointOpts::with_stdin_override("y");
        let res = ask(&c, tmp.path(), &opts).unwrap();
        assert_eq!(res, Resolution::Approved);
        let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", c.id))).unwrap();
        assert!(json.contains("\"response\": \"y\""));
        assert!(json.contains("\"accepted_default\": false"));
    }

    #[test]
    fn ask_non_interactive_calls_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(CheckpointKind::Final, "done?", true);
        let res = ask(&c, tmp.path(), &CheckpointOpts::non_interactive()).unwrap();
        assert_eq!(res, Resolution::Approved);
        let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", c.id))).unwrap();
        assert!(json.contains("<skipped:non_interactive>"));
    }

    #[test]
    fn ask_stdin_override_freeform_yields_modify() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(CheckpointKind::Clarify, "add constraint?", true);
        let opts = CheckpointOpts::with_stdin_override("add a 5GB cap");
        let res = ask(&c, tmp.path(), &opts).unwrap();
        assert_eq!(res, Resolution::Modify("add a 5GB cap".to_owned()));
    }

    #[test]
    fn ask_discovery_with_approve_resolves_approved() {
        // PR-20: the discovery checkpoint lists
        // `approve | review | block | export`. Piping `approve`
        // must resolve to `Resolution::Approved` so the manifest
        // sidecar can be sealed with `discovery.approved = true`.
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(
            CheckpointKind::Discovery {
                cat_count: 3,
                facet_count: 12,
                contradictions: 2,
            },
            "discovered 3 categories, 12 facets, 2 contradictions; next action?",
            true,
        );
        let opts = CheckpointOpts::with_stdin_override("approve");
        let res = ask(&c, tmp.path(), &opts).unwrap();
        assert_eq!(res, Resolution::Approved);
        let json = std::fs::read_to_string(tmp.path().join(format!("{}.json", c.id))).unwrap();
        // The kind token is the bare `"discovery"`; the counts
        // travel through the question text + the id, not through
        // the kind column.
        assert!(json.contains("\"kind\": \"discovery\""));
        assert!(json.contains("\"phase\": \"discover_summary\""));
        assert!(json.contains("\"response\": \"approve\""));
    }

    #[test]
    fn ask_discovery_with_block_resolves_rejected() {
        // PR-20: `block` is the explicit no token for the
        // discovery checkpoint.
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(
            CheckpointKind::Discovery {
                cat_count: 1,
                facet_count: 4,
                contradictions: 0,
            },
            "discovered 1 category, 4 facets, 0 contradictions; next action?",
            true,
        );
        let opts = CheckpointOpts::with_stdin_override("block");
        let res = ask(&c, tmp.path(), &opts).unwrap();
        assert_eq!(res, Resolution::Rejected);
    }

    #[test]
    fn ask_discovery_with_review_resolves_modify() {
        // PR-20: `review cat_02` (a free-form action prefix) is
        // captured verbatim so the call site can persist it as
        // a modify note.
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(
            CheckpointKind::Discovery {
                cat_count: 2,
                facet_count: 6,
                contradictions: 1,
            },
            "discovered 2 categories, 6 facets, 1 contradiction; next action?",
            true,
        );
        let opts = CheckpointOpts::with_stdin_override("review cat_02");
        let res = ask(&c, tmp.path(), &opts).unwrap();
        assert_eq!(res, Resolution::Modify("review cat_02".to_owned()));
    }

    #[test]
    fn resolution_helpers() {
        assert!(Resolution::Approved.is_approved());
        assert!(!Resolution::Rejected.is_approved());
        assert!(Resolution::Modify("x".into()).is_modify());
        assert!(!Resolution::Approved.is_modify());
    }
}
