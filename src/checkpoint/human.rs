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
    /// Fired at any other point the pipeline defines.
    Custom,
}

impl CheckpointKind {
    /// Stable lowercase string used in the persisted JSON and the
    /// SQLite column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Clarify => "clarify",
            Self::Final => "final",
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
#[derive(Debug, Clone)]
pub struct CheckpointOpts {
    /// `true` for `standard` / `deep`, `false` for `batch` /
    /// `--non-interactive`. When `false`, [`ask`] is a no-op that
    /// returns `Resolution::Approved` without touching stdin.
    pub interactive: bool,
    /// When set, [`ask`] writes to this path (used by tests that
    /// want to inject a pre-canned answer). When `None`, the call
    /// reads from `std::io::stdin()` directly.
    pub stdin_override: Option<String>,
}

impl Default for CheckpointOpts {
    fn default() -> Self {
        Self {
            interactive: true,
            stdin_override: None,
        }
    }
}

impl CheckpointOpts {
    /// Non-interactive constructor — used by `--non-interactive` and
    /// `Mode::Batch`.
    pub fn non_interactive() -> Self {
        Self {
            interactive: false,
            stdin_override: None,
        }
    }

    /// Build an opts with a pre-canned stdin response (tests only).
    pub fn with_stdin_override(response: impl Into<String>) -> Self {
        Self {
            interactive: true,
            stdin_override: Some(response.into()),
        }
    }
}

/// Skip the prompt entirely. The captured answer is treated as
/// "approved" (default) and is persisted to disk so the audit trail
/// records that the prompt was suppressed, not that the user typed
/// nothing.
pub fn skip(checkpoint: &Checkpoint, dir: &Path) -> Result<Resolution> {
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
    persist(dir, &captured)?;
    Ok(if checkpoint.default_yes {
        Resolution::Approved
    } else {
        Resolution::Rejected
    })
}

/// Ask the user. Blocking on stdin. Returns the [`Resolution`] and
/// persists a `HumanCheckpoint` JSON sidecar.
pub fn ask(checkpoint: &Checkpoint, dir: &Path, opts: &CheckpointOpts) -> Result<Resolution> {
    if !opts.interactive {
        return skip(checkpoint, dir);
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
    persist(dir, &captured)?;
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
    if matches!(lowered.as_str(), "y" | "yes") {
        Resolution::Approved
    } else if matches!(lowered.as_str(), "n" | "no") {
        Resolution::Rejected
    } else {
        Resolution::Modify(trimmed.to_owned())
    }
}

fn persist(dir: &Path, checkpoint: &HumanCheckpoint) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path: PathBuf = dir.join(format!("{}.json", checkpoint.id));
    let json = serde_json::to_vec_pretty(checkpoint)?;
    crate::atomic::writer::AtomicWriter::new().write(&path, &json)?;
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
    }

    #[test]
    fn parse_resolution_yes() {
        assert_eq!(parse_resolution("y", false), Resolution::Approved);
        assert_eq!(parse_resolution("Y", false), Resolution::Approved);
        assert_eq!(parse_resolution("yes", false), Resolution::Approved);
    }

    #[test]
    fn parse_resolution_no() {
        assert_eq!(parse_resolution("n", true), Resolution::Rejected);
        assert_eq!(parse_resolution("no", true), Resolution::Rejected);
        assert_eq!(parse_resolution("N", true), Resolution::Rejected);
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
    }

    #[test]
    fn skip_persists_marker_and_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Checkpoint::new(CheckpointKind::Final, "ship it?", true);
        let res = skip(&c, tmp.path()).unwrap();
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
        let res = skip(&c, tmp.path()).unwrap();
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
    fn resolution_helpers() {
        assert!(Resolution::Approved.is_approved());
        assert!(!Resolution::Rejected.is_approved());
        assert!(Resolution::Modify("x".into()).is_modify());
        assert!(!Resolution::Approved.is_modify());
    }
}
