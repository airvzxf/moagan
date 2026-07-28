//! Validate phase. Runs the `validators::Validator` suite against
//! every proposal and persists the resulting evidence.
//!
//! Insertion point: between `ProposePhase` and `GatePhase` for
//! `--mode deep` and `--mode batch` (the proposal-level spec for
//! the executable evidence in V4 §5.8). `fast`, `standard`, and
//! `explore` skip this phase because they have no executable
//! artefacts to validate.
//!
//! Output: one sidecar per proposal under
//! `<run_dir>/validation/p_<id>.evidence.json`. The existing
//! `validation/p_<id>.json` (written by the gate phase) keeps the
//! `Gate { pass, issues, missing }` shape and stays untouched; the
//! validate phase writes a sibling file with the same stem and a
//! distinct suffix so downstream readers can disambiguate.
//!
//! Compliance: `proposal-02-rust.md` §5.8 + `proposal-01-concept.md`
//! §13.6 "Segunda etapa".

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::{Brief, Proposal};
use crate::error::Result;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::read_json;
use crate::sandbox::{Sandbox, SandboxConfig};
use crate::validators::{
    ConstraintsValidator, PythonValidator, RustValidator, StructuralValidator, TypeScriptValidator,
    ValidationEvidence, Validator,
};

/// Sidecar schema persisted by the validate phase. Serialised as
/// pretty JSON so an operator inspecting a run directory can read
/// it without a JSON viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationSidecar {
    /// Schema version. Bumped on incompatible changes.
    pub schema_version: String,
    /// Proposal id this evidence belongs to.
    pub proposal_id: String,
    /// Aggregated verdict across every validator that ran.
    pub status: String,
    /// Per-validator evidence (one entry per validator).
    pub validators: Vec<ValidationEvidence>,
}

impl ValidationSidecar {
    /// Current schema version.
    pub const SCHEMA_VERSION: &'static str = "v1";
}

/// Validate phase. Runs Structural, Constraints, Rust, Python, and
/// TypeScript validators and writes a sidecar per proposal.
#[derive(Debug, Clone, Copy)]
pub struct ValidatePhase {
    /// Sandbox timeout in seconds. Default 30s matches the proposal.
    pub sandbox_timeout_secs: u64,
}

impl Default for ValidatePhase {
    fn default() -> Self {
        Self {
            sandbox_timeout_secs: 30,
        }
    }
}

impl ValidatePhase {
    /// Build a phase with the default 30s sandbox timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the per-validator sandbox timeout.
    pub fn with_sandbox_timeout(mut self, secs: u64) -> Self {
        self.sandbox_timeout_secs = secs.max(1);
        self
    }

    /// Compose the validator suite the phase runs.
    ///
    /// Order matters only for the composite aggregator: structural
    /// first so a hard failure short-circuits the language checks,
    /// then constraints, then language validators. The order of
    /// language validators does not matter; each one is independent.
    fn build_validators() -> Vec<Box<dyn Validator>> {
        vec![
            Box::new(StructuralValidator::new()),
            Box::new(ConstraintsValidator::new()),
        ]
    }
}

#[async_trait]
impl Phase for ValidatePhase {
    fn name(&self) -> &'static str {
        "validate"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let validation_dir = ctx.run_dir().validation();
        // Evidence sidecars live under validation/evidence/ so they
        // never collide with the Gate phase's bare validation/p_*.json
        // sidecars, which Repair reads as Gate structs.
        let evidence_dir = validation_dir.join("evidence");
        std::fs::create_dir_all(&evidence_dir)?;

        let sandbox = Sandbox::new(
            SandboxConfig::new()
                .with_timeout(std::time::Duration::from_secs(self.sandbox_timeout_secs)),
        )?;

        // Read the brief once so the constraints validator sees the
        // real hard constraints instead of the no-op trait path.
        let brief: Brief = read_json(&ctx.run_dir().brief()).unwrap_or_default();

        let validators = Self::build_validators();
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let proposal: Proposal = read_json(&path)?;
            let id = proposal.id.clone();

            // Structural + constraints validators see the
            // Proposal only (no source code attached). They run
            // first so a hard structural failure short-circuits
            // the (more expensive) language validators. The
            // constraints validator is invoked with the real
            // brief constraints so "requisitos duros" actually
            // match the proposal text instead of short-circuiting
            // to a no-op Pass.
            let mut evidences: Vec<ValidationEvidence> = Vec::new();
            for v in &validators {
                let ev = if v.name() == ConstraintsValidator::name_static() {
                    ConstraintsValidator::check_with_brief(&proposal, &brief)
                } else {
                    match v.validate(&proposal, Some(&sandbox)) {
                        Ok(ev) => ev,
                        Err(e) => ValidationEvidence {
                            validator: v.name().into(),
                            status: crate::validators::ValidationStatus::Error,
                            failed_checks: vec![format!("validator error: {e}")],
                            ..ValidationEvidence::default()
                        },
                    }
                };
                evidences.push(ev);
            }

            // Language validators (rust / python / typescript)
            // run per artifact. Unknown languages log a Skipped
            // evidence so the sidecar still records the dispatch.
            for artifact in &proposal.artifacts {
                let lang = artifact.language.as_str();
                let result = match lang {
                    RustValidator::LANGUAGE => RustValidator::check(artifact, &sandbox).await,
                    PythonValidator::LANGUAGE => PythonValidator::check(artifact, &sandbox).await,
                    TypeScriptValidator::LANGUAGE => {
                        TypeScriptValidator::check(artifact, &sandbox).await
                    }
                    other => Ok(ValidationEvidence::skipped(
                        other,
                        "no validator registered for this language",
                    )),
                };
                match result {
                    Ok(ev) => evidences.push(ev),
                    Err(e) => evidences.push(ValidationEvidence {
                        validator: lang.into(),
                        status: crate::validators::ValidationStatus::Error,
                        failed_checks: vec![format!("validator error: {e}")],
                        ..ValidationEvidence::default()
                    }),
                }
            }

            // Aggregate verdict: any Fail collapses to Fail,
            // otherwise any Warn is Warn, otherwise Pass. The
            // composite helper is overkill for two validators but
            // gives us the rule in one place.
            let status = aggregate_status(&evidences);
            let sidecar = ValidationSidecar {
                schema_version: ValidationSidecar::SCHEMA_VERSION.into(),
                proposal_id: id.clone(),
                status,
                validators: evidences,
            };

            let out_path: PathBuf = evidence_dir.join(format!("{id}.json"));
            crate::phases::util::write_json(&out_path, &sidecar)?;
            paths.push(out_path);
        }

        Ok(PhaseOutput::Validations(paths))
    }
}

fn aggregate_status(evidences: &[ValidationEvidence]) -> String {
    use crate::validators::ValidationStatus;
    let mut current = ValidationStatus::Pass;
    for ev in evidences {
        current = match (current, ev.status) {
            (_, ValidationStatus::Fail) | (ValidationStatus::Fail, _) => ValidationStatus::Fail,
            (ValidationStatus::Pass, ValidationStatus::Warn)
            | (ValidationStatus::Warn, ValidationStatus::Warn) => ValidationStatus::Warn,
            (cur, ValidationStatus::Pass) => cur,
            (ValidationStatus::Pass, other) => other,
            (cur, other) => match cur {
                ValidationStatus::Pass => other,
                _ => cur,
            },
        };
    }
    current.as_str().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_pass_only_stays_pass() {
        let ev = ValidationEvidence::pass("x", "y");
        assert_eq!(aggregate_status(&[ev]), "pass");
    }

    #[test]
    fn aggregate_warn_only_is_warn() {
        let mut ev = ValidationEvidence::pass("x", "y");
        ev.status = crate::validators::ValidationStatus::Warn;
        assert_eq!(aggregate_status(&[ev]), "warn");
    }

    #[test]
    fn aggregate_fail_demotes_to_fail() {
        let pass = ValidationEvidence::pass("p", "p");
        let fail = ValidationEvidence::fail("f", "f");
        assert_eq!(aggregate_status(&[pass, fail]), "fail");
    }

    #[test]
    fn aggregate_handles_empty_slice() {
        // Empty slice means no validators ran; the pipeline treats
        // that as a Pass because there is no evidence either way.
        assert_eq!(aggregate_status(&[]), "pass");
    }

    #[test]
    fn sidecar_round_trips_json() {
        let s = ValidationSidecar {
            schema_version: ValidationSidecar::SCHEMA_VERSION.into(),
            proposal_id: "p_001".into(),
            status: "pass".into(),
            validators: vec![ValidationEvidence::pass("structural", "id")],
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: ValidationSidecar = serde_json::from_str(&j).unwrap();
        assert_eq!(back.proposal_id, "p_001");
        assert_eq!(back.status, "pass");
        assert_eq!(back.validators.len(), 1);
    }

    /// The phase must run end-to-end against a real run directory:
    /// intake-style run context, a proposal on disk, and a sandbox.
    /// Without this smoke test, the integration_audit_e2e failure
    /// would be invisible to the unit-test suite.
    #[tokio::test]
    async fn execute_with_one_proposal_writes_evidence() {
        use crate::domain::Proposal;
        use crate::execution::Parallelism;
        use crate::fs_layout::MoaganHome;
        use crate::ids::RunId;
        use crate::llm::ProviderRegistry;
        use crate::phases::util::{read_json, write_json};
        use crate::telemetry::Telemetry;
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
        home.ensure().unwrap();
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();

        let proposal = Proposal {
            id: "p_001".into(),
            summary: "summary that's long enough to satisfy the structural check".into(),
            approach: "approach".into(),
            tradeoffs: vec!["t".into()],
            evidence: vec!["e".into()],
            ..Proposal::default()
        };
        let proposals_dir = run_dir.proposals();
        std::fs::create_dir_all(&proposals_dir).unwrap();
        write_json(&proposals_dir.join("p_001.json"), &proposal).unwrap();

        let ctx = RunContext::new(
            run_id,
            Arc::clone(&home),
            Arc::new(ProviderRegistry::default()),
            "minimax".into(),
            "minimax-m3".into(),
            Parallelism::new(1),
            Telemetry::noop(),
            "test".into(),
            "deep".into(),
        );

        let phase = ValidatePhase::new();
        let result = phase.execute(&ctx).await;
        assert!(result.is_ok(), "execute failed: {result:?}");
        let out_path = run_dir.validation().join("evidence").join("p_001.json");
        assert!(out_path.exists(), "evidence file missing at {out_path:?}");
        let sidecar: ValidationSidecar = read_json(&out_path).unwrap();
        assert_eq!(sidecar.proposal_id, "p_001");
        assert_eq!(sidecar.validators.len(), 2);
    }
}
