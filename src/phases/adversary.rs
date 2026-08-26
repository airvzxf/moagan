//! Adversary phase. Reads every `evaluations/p_*.json` left by the
//! judge phase, runs the seven deterministic patterns from
//! [`crate::ranking::adversary_patterns::run_all_patterns`] against
//! each proposal, and writes a per-pattern audit report to
//! `rankings/adversary_report.json`.
//!
//! Spec contract (D.22.1 + D.12.5):
//!
//! - The phase is **deterministic** — no LLM calls, no DB writes,
//!   no I/O beyond reading `evaluations/`, `proposals/`, and writing
//!   the single sidecar. Re-running it on the same inputs produces
//!   the same byte-identical sidecar.
//! - The twelve canonical patterns (`AdversaryPattern::all`: the
//!   original seven from PR-11 / v0.5 plus the five D.12.5 add-on
//!   patterns `shared_blind_spots`, `unanimous_claims_without_evidence`,
//!   `hidden_assumptions`, `omitted_risks`, `unverified_claims`)
//!   produce one section each in the report. A section carries the
//!   `fired_count` (how many proposals tripped the pattern) and a
//!   per-proposal verdict with the raw metric payload so a
//!   post-mortem can see *why* each pattern fired (or didn't).
//! - The phase is **opt-in**. The pipeline builder inserts the
//!   phase only when the run is `Mode::Deep` or the operator passes
//!   `--adversary`. Other modes skip the sidecar entirely so the
//!   canonical run cost does not regress.
//!
//! Compatibility note: an LLM-based adversary pass also runs inside
//! [`crate::phases::judge::JudgePhase`] (writing
//! `adversaries/p_<id>.json` per proposal). The two coexist — the
//! LLM pass produces a free-form critique; this phase produces the
//! seven-pattern metric report. They are complementary, not
//! redundant. See the v0.5 roadmap PR-11 audit note for the full
//! history.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::Proposal;
use crate::error::Result;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::ranking::adversary_patterns::{AdversaryPattern, PatternVerdict, run_all_patterns};

/// Wire-format version of [`PatternAdversaryReport`]. Bump on any
/// change to the section / verdict shape so downstream consumers
/// can pin to a specific version.
pub const PATTERN_ADVERSARY_SCHEMA_VERSION: &str = "pattern_adversary.v1";

/// File name under `<run_dir>/rankings/`.
const REPORT_FILE_NAME: &str = "adversary_report.json";

/// Pattern-based adversary report. Differs from
/// [`crate::domain::AdversaryReport`] (the LLM-emitted report) by
/// being strictly deterministic and carrying one section per
/// [`AdversaryPattern`] instead of a free-form critique.
///
/// The `serde(default)` attribute keeps the struct parseable when a
/// new pattern is added in the future: legacy sidecars continue to
/// deserialize with the missing sections filled in as empty
/// vectors.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternAdversaryReport {
    /// Wire-format schema version (see [`PATTERN_ADVERSARY_SCHEMA_VERSION`]).
    pub schema_version: String,
    /// Number of proposals the phase inspected. Mirrors the
    /// `proposals_evaluated` row in `runs` so the dashboard can
    /// show a "X / Y proposals tripped this pattern" ratio.
    pub proposal_count: usize,
    /// Generation timestamp (Unix seconds). Best-effort — set to
    /// `0` when the wall clock is unavailable (tests).
    pub generated_at_unix: i64,
    /// One section per [`AdversaryPattern`], in the canonical
    /// order returned by [`AdversaryPattern::all`]. The
    /// length is always 12 by construction; the field is a `Vec`
    /// for forward-compatibility (a thirteenth pattern would just
    /// append).
    pub sections: Vec<PatternAdversarySection>,
}

/// Per-pattern section of the report. Aggregates the per-proposal
/// verdicts and exposes a `fired_count` so consumers do not have to
/// walk `per_proposal` just to learn whether the pattern tripped on
/// any proposal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternAdversarySection {
    /// Which pattern this section describes. Round-trips through
    /// serde via the [`AdversaryPattern`] derive so a future
    /// pattern addition deserialises legacy reports as a known
    /// variant.
    pub pattern: AdversaryPattern,
    /// Number of proposals whose verdict for this pattern had
    /// `fired == true`. Cached so consumers can answer "did the
    /// pattern fire at all?" without iterating `per_proposal`.
    pub fired_count: usize,
    /// One verdict per proposal, in the same order as the
    /// `proposals/p_*.json` files were discovered on disk. The
    /// `proposal_id` is the file stem (e.g. `p_000`).
    pub per_proposal: Vec<ProposalPatternVerdict>,
}

/// Per-proposal verdict inside a pattern section. Re-uses the
/// [`PatternVerdict`] field semantics (`fired`, `detail`) so the
/// on-disk shape matches what [`run_all_patterns`] returns
/// verbatim — no translation layer needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposalPatternVerdict {
    /// Proposal id (file stem of `proposals/p_*.json`, e.g.
    /// `p_000`).
    pub proposal_id: String,
    /// Whether the pattern tripped for this proposal.
    pub fired: bool,
    /// Human-readable metric payload (e.g. `spread=0.000` or
    /// `count=3`). Carried verbatim from [`PatternVerdict::detail`].
    pub detail: String,
}

/// Pattern-based adversary phase. Reads the `evaluations/` and
/// `proposals/` directories left by the preceding judge phase, runs
/// [`run_all_patterns`] against each proposal, and writes the
/// consolidated [`PatternAdversaryReport`] to
/// `rankings/adversary_report.json`.
///
/// The phase does **no** LLM calls and is safe to run as part of
/// any pipeline that reaches this point. The pipeline builder
/// gates it on `Mode::Deep` / `--adversary` so the canonical run
/// cost does not regress for the other modes.
#[derive(Debug, Clone, Default)]
pub struct AdversaryPhase {
    /// When `true`, the phase writes the report and emits a
    /// `PhaseOutput::PatternAdversary`. When `false`, the phase
    /// short-circuits to a no-op so the pipeline can keep the
    /// phase slot without paying for the disk walk in modes that
    /// do not want the report. The pipeline builder toggles this
    /// field based on the mode + `--adversary` flag.
    pub enable: bool,
}

impl AdversaryPhase {
    /// Build an enabled phase.
    pub fn enabled() -> Self {
        Self { enable: true }
    }

    /// Build a disabled phase (no-op).
    pub fn disabled() -> Self {
        Self { enable: false }
    }
}

#[async_trait]
impl Phase for AdversaryPhase {
    fn name(&self) -> &'static str {
        "adversary"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!(enabled = self.enable, "adversary: enter");
        let evaluations_dir = ctx.run_dir().evaluations();
        let proposals_dir = ctx.run_dir().proposals();
        let rankings_dir = ctx.run_dir().rankings();
        std::fs::create_dir_all(&rankings_dir)?;

        let out_path: PathBuf = rankings_dir.join(REPORT_FILE_NAME);

        if !self.enable {
            // Opt-out path: the mode does not want the report.
            // Write an empty sidecar so downstream consumers
            // (dashboard, audit) can distinguish "phase ran with
            // nothing to do" from "phase was skipped". The empty
            // document carries the schema version + zero proposal
            // count so a parser always sees a valid artefact.
            let report = PatternAdversaryReport {
                schema_version: PATTERN_ADVERSARY_SCHEMA_VERSION.to_owned(),
                proposal_count: 0,
                generated_at_unix: now_unix_secs(),
                sections: Vec::new(),
            };
            write_json(&out_path, &report)?;
            tracing::debug!(
                run_id = %ctx.run_id,
                stage = "adversary.skipped",
                reason = "disabled",
                "Adversary phase skipped (mode/flag opt-out)"
            );
            return Ok(PhaseOutput::PatternAdversary(out_path));
        }

        // Step 1: walk every `evaluations/p_*.json` in
        // deterministic (filename) order. Each file carries the
        // aggregated judge score; the per-judge scores are not
        // persisted today so the spread / stddev patterns receive
        // a single-value slice and report `fired = false`. The
        // text-driven patterns (`HallucinationSignature`,
        // `ProvenanceDrift`, `AudienceMismatch`) and the
        // evidence-count pattern still produce meaningful
        // verdicts against the proposal body — see the module
        // docstring for the rationale.
        let mut by_proposal: Vec<(String, Aggregated, Proposal)> = Vec::new();
        let entries = match std::fs::read_dir(&evaluations_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No evaluations → nothing to do; write an empty
                // report so the dashboard does not 404.
                let report = PatternAdversaryReport {
                    schema_version: PATTERN_ADVERSARY_SCHEMA_VERSION.to_owned(),
                    proposal_count: 0,
                    generated_at_unix: now_unix_secs(),
                    sections: empty_sections(),
                };
                write_json(&out_path, &report)?;
                return Ok(PhaseOutput::PatternAdversary(out_path));
            }
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let proposal_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_owned(),
                None => continue,
            };
            let agg: Aggregated = read_json(&path)?;
            let proposal = load_proposal(&proposals_dir, &proposal_id);
            by_proposal.push((proposal_id, agg, proposal));
        }
        // Sort by proposal id for stable ordering across runs
        // (the filesystem order on Linux ext4 is not guaranteed
        // even within a single mount).
        by_proposal.sort_by(|a, b| a.0.cmp(&b.0));

        // Step 2: run all 12 patterns on each proposal and bucket
        // the verdicts per pattern. The bucket is a
        // `Vec<PatternVerdict>` aligned with `by_proposal` so the
        // post-process step can build the section's `per_proposal`
        // list without re-walking the proposals.
        let mut per_pattern: std::collections::BTreeMap<AdversaryPattern, Vec<PatternVerdict>> =
            std::collections::BTreeMap::new();
        for pattern in AdversaryPattern::all() {
            per_pattern.insert(pattern, Vec::with_capacity(by_proposal.len()));
        }
        for (proposal_id, agg, proposal) in &by_proposal {
            let scores: Vec<f64> = vec![agg.score as f64];
            let provenance = build_provenance(proposal);
            let verdicts = run_all_patterns(&scores, proposal.evidence.len(), &provenance);
            for v in verdicts {
                per_pattern.entry(v.pattern).or_default().push(v);
            }
            tracing::trace!(
                run_id = %ctx.run_id,
                proposal_id = %proposal_id,
                stage = "adversary.evaluated",
                "Adversary phase"
            );
        }

        // Step 3: build the per-pattern sections, preserving the
        // canonical order from `AdversaryPattern::all` so the
        // JSON shape is stable across runs and across
        // refactors of `run_all_patterns` (the helper is the
        // canonical source of the order).
        let mut sections: Vec<PatternAdversarySection> = Vec::with_capacity(12);
        for pattern in AdversaryPattern::all() {
            let verdicts = per_pattern.remove(&pattern).unwrap_or_default();
            let mut per_proposal = Vec::with_capacity(verdicts.len());
            let mut fired_count = 0_usize;
            for ((proposal_id, _, _), v) in by_proposal.iter().zip(verdicts) {
                if v.fired {
                    fired_count += 1;
                }
                per_proposal.push(ProposalPatternVerdict {
                    proposal_id: proposal_id.clone(),
                    fired: v.fired,
                    detail: v.detail,
                });
            }
            sections.push(PatternAdversarySection {
                pattern,
                fired_count,
                per_proposal,
            });
        }

        let report = PatternAdversaryReport {
            schema_version: PATTERN_ADVERSARY_SCHEMA_VERSION.to_owned(),
            proposal_count: by_proposal.len(),
            generated_at_unix: now_unix_secs(),
            sections,
        };

        write_json(&out_path, &report)?;

        let total_fired: usize = report.sections.iter().map(|s| s.fired_count).sum();
        tracing::info!(
            run_id = %ctx.run_id,
            proposal_count = report.proposal_count,
            fired_verdicts = total_fired,
            section_count = report.sections.len(),
            stage = "adversary.summary",
            "Adversary phase completed"
        );

        Ok(PhaseOutput::PatternAdversary(out_path))
    }
}

/// Build the "provenance" string the patterns scan. Mirrors the
/// `proposal_text` helper in `rank.rs` so the
/// `HallucinationSignature` / `ProvenanceDrift` /
/// `AudienceMismatch` patterns see the same string the rest of
/// the pipeline uses for SimHash clustering and selection-plan
/// filtering.
fn build_provenance(p: &Proposal) -> String {
    format!(
        "{} {} {} {}",
        p.summary,
        p.approach,
        p.tradeoffs.join(" "),
        p.evidence.join(" ")
    )
}

/// Load the proposal sidecar. Falls back to a default `Proposal`
/// (carrying only the id) when the file is missing so the phase
/// never branches on a missing sidecar — the patterns still see
/// an empty provenance and the report is written. Mirrors the
/// `load_proposal` helper in `rank.rs` so the two phases agree on
/// what counts as a "proposal".
fn load_proposal(proposals_dir: &std::path::Path, proposal_id: &str) -> Proposal {
    let path = proposals_dir.join(format!("{proposal_id}.json"));
    match read_json::<Proposal>(&path) {
        Ok(p) => p,
        Err(_) => Proposal {
            id: proposal_id.to_owned(),
            summary: proposal_id.to_owned(),
            ..Proposal::default()
        },
    }
}

/// Empty sections vector for the "no proposals evaluated" path:
/// always twelve entries, one per canonical pattern, all with
/// `fired_count = 0` and an empty `per_proposal`. Keeps the JSON
/// shape identical between "ran with zero proposals" and "ran with
/// N proposals and none tripped" so consumers do not have to
/// special-case the absence of the field.
fn empty_sections() -> Vec<PatternAdversarySection> {
    AdversaryPattern::all()
        .into_iter()
        .map(|pattern| PatternAdversarySection {
            pattern,
            fired_count: 0,
            per_proposal: Vec::new(),
        })
        .collect()
}

/// Unix-seconds now. Wrapped so tests can stub the clock without
/// pulling in a `cfg(test)`-only time-crate dependency.
fn now_unix_secs() -> i64 {
    crate::time::now_unix_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Parallelism;
    use crate::fs_layout::MoaganHome;
    use crate::ids::RunId;
    use crate::llm::ProviderRegistry;
    use crate::redact::RedactPolicy;
    use crate::telemetry::Telemetry;

    fn empty_ctx() -> (
        tempfile::TempDir,
        std::sync::Arc<MoaganHome>,
        RunId,
        RunContext,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = std::sync::Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
        home.ensure().expect("ensure home");
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run_dir");
        let telemetry = Telemetry::open(run_id, &run_dir, RedactPolicy::default(), None)
            .expect("telemetry opens");
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            std::sync::Arc::new(ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);
        (tmp, home, run_id, ctx)
    }

    fn write_evaluation(
        home: &MoaganHome,
        run_id: RunId,
        id: &str,
        score: f32,
        evidence_len: usize,
    ) {
        let eval_dir = home.run_dir(run_id).evaluations();
        std::fs::create_dir_all(&eval_dir).unwrap();
        let path = eval_dir.join(format!("{id}.json"));
        let agg = Aggregated {
            score,
            correctness: score,
            completeness: score,
            fit: score,
            evidence: score,
            clarity: score,
            judges: 1,
            adversary_delta: 0.0,
        };
        write_json(&path, &agg).unwrap();

        let proposal_dir = home.run_dir(run_id).proposals();
        std::fs::create_dir_all(&proposal_dir).unwrap();
        let proposal_path = proposal_dir.join(format!("{id}.json"));
        let proposal = Proposal {
            id: id.into(),
            summary: format!("summary {id}"),
            approach: format!("approach {id}"),
            tradeoffs: Vec::new(),
            evidence: (0..evidence_len).map(|i| format!("evidence-{i}")).collect(),
            source_sketch: String::new(),
            artifacts: Vec::new(),
            replaced_by: None,
            source_nodes: Vec::new(),
        };
        write_json(&proposal_path, &proposal).unwrap();
    }

    /// The phase writes an `adversary_report.json` with exactly
    /// twelve sections, one per canonical pattern, when
    /// `enable == true`. Pins the spec contract for PR-11 +
    /// D.12.5.
    #[test]
    fn adversary_phase_writes_twelve_sections() -> Result<()> {
        let (_tmp, home, run_id, ctx) = empty_ctx();
        write_evaluation(&home, run_id, "p_a", 8.0, 3);
        write_evaluation(&home, run_id, "p_b", 6.0, 1);

        let phase = AdversaryPhase::enabled();
        let output = pollster::block_on(phase.execute(&ctx))?;

        match output {
            PhaseOutput::PatternAdversary(path) => {
                let raw = std::fs::read(&path).expect("report exists");
                let report: PatternAdversaryReport =
                    serde_json::from_slice(&raw).expect("report parses");
                assert_eq!(
                    report.sections.len(),
                    12,
                    "expected 12 sections (one per AdversaryPattern), got {}",
                    report.sections.len()
                );
                assert_eq!(report.proposal_count, 2);
                assert_eq!(report.schema_version, PATTERN_ADVERSARY_SCHEMA_VERSION);
                // Section ordering must follow AdversaryPattern::all().
                let patterns: Vec<AdversaryPattern> =
                    report.sections.iter().map(|s| s.pattern).collect();
                assert_eq!(patterns, AdversaryPattern::all().to_vec());
                Ok(())
            }
            other => panic!("expected PhaseOutput::PatternAdversary, got {other:?}"),
        }
    }

    /// The disabled phase still writes a sidecar so downstream
    /// consumers can distinguish "phase skipped (opt-out)" from
    /// "phase never ran" (file missing). The disabled sidecar
    /// has zero proposals and an empty `sections` vector so the
    /// dashboard's "adversary_report per run" view stays
    /// consistent.
    #[test]
    fn adversary_phase_disabled_writes_empty_report() -> Result<()> {
        let (_tmp, home, run_id, ctx) = empty_ctx();
        write_evaluation(&home, run_id, "p_a", 8.0, 3);

        let phase = AdversaryPhase::disabled();
        let output = pollster::block_on(phase.execute(&ctx))?;

        match output {
            PhaseOutput::PatternAdversary(path) => {
                let raw = std::fs::read(&path).expect("report exists");
                let report: PatternAdversaryReport =
                    serde_json::from_slice(&raw).expect("report parses");
                assert_eq!(report.proposal_count, 0);
                assert!(report.sections.is_empty());
                Ok(())
            }
            other => panic!("expected PhaseOutput::PatternAdversary, got {other:?}"),
        }
    }

    /// `HallucinationSignature` fires when the proposal text
    /// contains a known LLM-meta phrase. The phase must surface
    /// this through the per-proposal verdict of the
    /// `HallucinationSignature` section.
    #[test]
    fn adversary_phase_flags_hallucination_signature() -> Result<()> {
        let (_tmp, home, run_id, ctx) = empty_ctx();
        write_evaluation(&home, run_id, "p_ai", 7.0, 2);
        // Overwrite the proposal so the approach contains an
        // LLM-meta phrase that the pattern is wired to catch.
        let proposal_path = home.run_dir(run_id).proposals().join("p_ai.json");
        let mut proposal: Proposal = read_json(&proposal_path)?;
        proposal.approach = "As an AI, I cannot provide a kernel patch.".to_owned();
        write_json(&proposal_path, &proposal)?;

        let phase = AdversaryPhase::enabled();
        pollster::block_on(phase.execute(&ctx))?;

        let report_path = home.run_dir(run_id).rankings().join(REPORT_FILE_NAME);
        let report: PatternAdversaryReport = serde_json::from_slice(&std::fs::read(&report_path)?)?;
        let section = report
            .sections
            .iter()
            .find(|s| s.pattern == AdversaryPattern::HallucinationSignature)
            .expect("HallucinationSignature section");
        let verdict = section
            .per_proposal
            .iter()
            .find(|v| v.proposal_id == "p_ai")
            .expect("p_ai verdict");
        assert!(
            verdict.fired,
            "HallucinationSignature must fire for LLM-meta prose; detail={}",
            verdict.detail
        );
        Ok(())
    }

    /// `InsufficientEvidence` fires when the proposal carries
    /// fewer than 2 evidence items. Pins the contract that the
    /// evidence count comes from `proposal.evidence.len()` (not
    /// from the aggregated `evidence` score).
    #[test]
    fn adversary_phase_flags_insufficient_evidence() -> Result<()> {
        let (_tmp, home, run_id, ctx) = empty_ctx();
        write_evaluation(&home, run_id, "p_thin", 8.0, 1);
        write_evaluation(&home, run_id, "p_fat", 8.0, 5);

        let phase = AdversaryPhase::enabled();
        pollster::block_on(phase.execute(&ctx))?;

        let report_path = home.run_dir(run_id).rankings().join(REPORT_FILE_NAME);
        let report: PatternAdversaryReport = serde_json::from_slice(&std::fs::read(&report_path)?)?;
        let section = report
            .sections
            .iter()
            .find(|s| s.pattern == AdversaryPattern::InsufficientEvidence)
            .expect("InsufficientEvidence section");
        let thin = section
            .per_proposal
            .iter()
            .find(|v| v.proposal_id == "p_thin")
            .expect("p_thin verdict");
        let fat = section
            .per_proposal
            .iter()
            .find(|v| v.proposal_id == "p_fat")
            .expect("p_fat verdict");
        assert!(thin.fired, "1 evidence item must trip InsufficientEvidence");
        assert!(
            !fat.fired,
            "5 evidence items must NOT trip InsufficientEvidence"
        );
        Ok(())
    }

    /// The disabled-by-default `AdversaryPhase::default()` is
    /// consistent with the opt-in design (the pipeline builder
    /// only inserts the phase when mode == Deep or
    /// `--adversary` is set, but the constructor still defaults
    /// to off so a misuse does not silently emit the sidecar).
    #[test]
    fn adversary_phase_default_is_disabled() {
        let phase = AdversaryPhase::default();
        assert!(!phase.enable, "default phase must be opt-in (enable=false)");
        let enabled = AdversaryPhase::enabled();
        assert!(
            enabled.enable,
            "AdversaryPhase::enabled() must set enable=true"
        );
    }
}
