//! Resume logic for paused runs.
//!
//! PR #131 wrote `paused.json` with a hard-coded `paused_at_phase`
//! (`"synthesize"`) and a hard-coded `completed_phases` list. This
//! module inspects the live run state via [`Db`] and surfaces the
//! actual completed phases so `moagan pause` and the upcoming
//! `moagan continue --from-pause` resume can rely on the SQLite
//! index instead of the legacy constants.
//!
//! The fallback constants ([`DEFAULT_COMPLETED_PHASES`] and
//! [`DEFAULT_PAUSED_AT_PHASE`]) are kept so a paused run whose
//! SQLite index was never written (e.g. the run was paused before
//! `db.register_run(...)` had a chance to commit, or the operator
//! wiped `<home>/meta.sqlite` between sessions) still produces a
//! readable `paused.json` and the resume path can decide whether
//! to start fresh.

use crate::error::Result;
use crate::ids::RunId;
use crate::storage::sqlite::Db;

/// Default `completed_phases` used when neither `--completed` nor a
/// live DB record are available. Matches the legacy hard-coded list
/// from PR #131 so existing on-disk `paused.json` files remain
/// readable and re-pauses of legacy runs produce the same shape.
pub const DEFAULT_COMPLETED_PHASES: &[&str] = &["intake", "clarify", "sketch", "propose", "gate"];

/// Default `paused_at_phase` used when neither `--phase` nor a live
/// DB record are available. Matches the legacy hard-coded value
/// from PR #131.
pub const DEFAULT_PAUSED_AT_PHASE: &str = "synthesize";

/// Look up the actual completed phases for `run_id` in the live
/// SQLite index. PR #131 wrote `paused.json` with a hard-coded list
/// of phases; this function is the source of truth that replaces it.
///
/// Returns `Ok(vec![])` when the run has no recorded phase ends yet
/// (newly-started, or the run was paused before any phase wrote to
/// the index). Callers should treat the empty vector as "use the
/// default fallback" — this function does not know about the
/// default list because the fallback policy lives in the CLI layer.
pub fn derive_completed_phases(db: &Db, run_id: RunId) -> Result<Vec<String>> {
    db.list_completed_phases(run_id)
}

/// Resolve the `paused_at_phase` string for `run_id`. Tries the live
/// SQLite index (`Db::last_completed_phase`) first so the pause
/// point lands at the actual last boundary the pipeline reached.
/// Falls back to [`DEFAULT_PAUSED_AT_PHASE`] when the run is not in
/// the index yet (a fresh run whose `db.register_run(...)` was not
/// yet flushed, or a run with no phase events).
///
/// The "last completed phase" semantics match `moagan continue`
/// (which uses the same `Db::last_completed_phase` lookup), so a
/// later `moagan continue --from-pause` resumes from exactly the
/// phase the pause was issued at.
pub fn derive_paused_at_phase(db: &Db, run_id: RunId) -> Result<String> {
    let last = db.last_completed_phase(run_id)?;
    Ok(last.unwrap_or_else(|| DEFAULT_PAUSED_AT_PHASE.to_string()))
}

/// True when the SQLite index has any record for `run_id`. Lets
/// `moagan pause` distinguish "run committed to DB, use live state"
/// from "run paused before DB commit, use legacy fallback" without
/// coupling the CLI to the `runs` table directly.
pub fn run_is_registered(db: &Db, run_id: RunId) -> Result<bool> {
    db.has_run(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fresh, on-disk test DB. Mirrors the helper used in
    /// `src/storage/sqlite.rs::tests::temp_db` so the resume tests
    /// stay self-contained.
    fn temp_db() -> Db {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.sqlite");
        // Leak the tempdir so the DB survives the test body — the
        // OS reclaims on exit. Same trick as `temp_db()` in the
        // storage module.
        std::mem::forget(tmp);
        Db::open(&path).unwrap()
    }

    /// `derive_completed_phases` returns every recorded phase-end
    /// for the run, in the order `Db::list_completed_phases`
    /// produces (`started_unix DESC, phase ASC`). Pins the contract
    /// so the resume CLI does not need to special-case phase
    /// ordering.
    #[test]
    fn derive_completed_phases_returns_every_end() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "standard", "running", "0.4.0", None, None, None)
            .unwrap();
        db.record_phase(run_id, "intake", 0, "start", None).unwrap();
        db.record_phase(run_id, "intake", 0, "end", None).unwrap();
        db.record_phase(run_id, "clarify", 0, "start", None)
            .unwrap();
        db.record_phase(run_id, "clarify", 0, "end", None).unwrap();
        let phases = derive_completed_phases(&db, run_id).unwrap();
        assert_eq!(phases, vec!["clarify".to_string(), "intake".to_string()]);
    }

    /// A run with no phase events returns the empty vector so the
    /// CLI can fall back to [`DEFAULT_COMPLETED_PHASES`].
    #[test]
    fn derive_completed_phases_empty_for_fresh_run() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        let phases = derive_completed_phases(&db, run_id).unwrap();
        assert!(phases.is_empty());
    }

    /// `derive_paused_at_phase` returns the last phase that ended
    /// successfully, matching the `moagan continue` lookup. A run
    /// with no ends falls back to [`DEFAULT_PAUSED_AT_PHASE`].
    #[test]
    fn derive_paused_at_phase_returns_last_end() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "standard", "running", "0.4.0", None, None, None)
            .unwrap();
        db.record_phase(run_id, "intake", 0, "end", None).unwrap();
        db.record_phase(run_id, "clarify", 0, "end", None).unwrap();
        assert_eq!(
            derive_paused_at_phase(&db, run_id).unwrap(),
            "clarify".to_string()
        );
    }

    #[test]
    fn derive_paused_at_phase_falls_back_when_empty() {
        let db = temp_db();
        let run_id = RunId::new();
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        assert_eq!(
            derive_paused_at_phase(&db, run_id).unwrap(),
            DEFAULT_PAUSED_AT_PHASE.to_string()
        );
    }

    /// `run_is_registered` distinguishes "run is in the index" from
    /// "operator typed a typo" so the CLI can pick between live
    /// state and the legacy fallback.
    #[test]
    fn run_is_registered_tracks_runs_table() {
        let db = temp_db();
        let registered = RunId::new();
        let unregistered = RunId::new();
        db.register_run(registered, "fast", "running", "0.4.0", None, None, None)
            .unwrap();
        assert!(run_is_registered(&db, registered).unwrap());
        assert!(!run_is_registered(&db, unregistered).unwrap());
    }
}
