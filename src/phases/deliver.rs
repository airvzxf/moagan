//! Deliver phase. Reads the ranking, the representatives, the
//! critiques, and the evaluations; asks the model to write the final
//! user-facing response; writes `final/portfolio.md` and
//! `final/portfolio.json` with the complete §5.15 package:
//!
//! 1. resumen ejecutivo
//! 2. portfolio (top-3 representatives with badges)
//! 3. matriz comparativa (proposals × criteria)
//! 4. mapa de divergencias (critiques' issues)
//! 5. evidencia (links to sidecars)
//! 6. auditoría (run_id, provider, model, weights, mode)
//!
//! Phase D (V4 §5.14): after the model writes the report, the phase
//! fires the final-checkpoint prompt to confirm the user accepts the
//! portfolio. The check is no-op in non-interactive runs.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::checkpoint::modify_note;
use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::domain::{FinalReport, Proposal, Ranking};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::telemetry::dashboard_static;

/// Deliver phase.
pub struct DeliverPhase;

#[async_trait]
impl Phase for DeliverPhase {
    fn name(&self) -> &'static str {
        "deliver"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!(interactive = ctx.interactive, "deliver: enter");
        let ranking_path = ctx.run_dir().rankings().join("ranking.json");
        let ranking: Ranking = read_json(&ranking_path)?;
        tracing::trace!(
            winner = %ranking.winner,
            ranked_len = ranking.ranked.len(),
            representatives_len = ranking.representatives.len(),
            "deliver: ranking loaded"
        );
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let winner_proposal: Proposal =
            read_json(&proposals_dir.join(format!("{}.json", ranking.winner))).or_else(|_| {
                let p = revisions_dir.join(format!("{}_rev_0.json", ranking.winner));
                read_json(&p)
            })?;

        let evaluations = load_evaluations(&ctx.run_dir().evaluations());
        let critiques = load_critiques(&ctx.run_dir().critiques());
        tracing::trace!(
            evaluations_loaded = evaluations.len(),
            critiques_loaded = critiques.len(),
            "deliver: artefacts loaded"
        );

        // c2: `portfolio_finalized` decision event. Summary level.
        // Emitted once per run, immediately after the ranking is
        // resolved and the deliver LLM call has been prepared —
        // operators who watch the bus see the winner locked in
        // before the deliver-side text generation finishes.
        // `ranking_strategy` is the
        // `Config::selection_plan.kind` label (`top_n` /
        // `diverse_n` / `outlier_n`); `alternatives` is the top-N
        // representative ids excluding the winner so a downstream
        // consumer can reconstruct the portfolio without
        // re-reading `ranking.json`.
        let winner_id = ranking.winner.clone();
        let alternatives: Vec<String> = ranking
            .representatives
            .iter()
            .map(|r| r.id.clone())
            .filter(|id| id != &winner_id)
            .collect();
        let ranking_strategy = match ctx.config.selection_plan.kind {
            crate::phases::cardinality::SelectionKind::TopN => "top_n",
            crate::phases::cardinality::SelectionKind::DiverseN => "diverse_n",
            crate::phases::cardinality::SelectionKind::OutlierN => "outlier_n",
        };
        crate::telemetry::stdout_events::emit_decision("portfolio_finalized", || {
            serde_json::json!({
                "proposal_id": winner_id,
                "ranking_strategy": ranking_strategy,
                "alternatives": alternatives,
            })
        });

        let user = serde_json::to_string(&serde_json::json!({
            "winner": ranking.winner,
            "proposal": winner_proposal,
            "ranked": ranking.ranked,
            "representatives": ranking.representatives,
        }))
        .map_err(crate::Error::from)?;
        // F1: prepend the operator note (if any) to the user
        // prompt. The note is wrapped in a tagged block so the
        // deliver model can distinguish the correction from the
        // underlying ranking payload.
        let user = modify_note::prepend_to_prompt(ctx.run_dir().root(), &user);
        let system = system_prompt(Role::Deliver).to_owned();
        let report: FinalReport = ctx
            .call_with_retry_parse(
                Role::Deliver,
                system,
                user,
                "FinalReport: {title, summary, recommendation, alternatives[], next_steps[]}",
                5,
            )
            .await?;
        tracing::debug!(
            alternatives = report.alternatives.len(),
            next_steps = report.next_steps.len(),
            "deliver: model produced report"
        );

        let final_dir = ctx.run_dir().final_dir();
        std::fs::create_dir_all(&final_dir)?;
        let json_path: PathBuf = final_dir.join("portfolio.json");
        write_json(&json_path, &report)?;
        tracing::trace!(path = %json_path.display(), "deliver: portfolio.json written");

        let md = render_markdown(
            &report,
            &ranking,
            &evaluations,
            &critiques,
            ctx.run_id.to_string().as_str(),
            &ctx.default_provider,
            &ctx.default_model,
            &ctx.mode,
        );
        let md_path: PathBuf = final_dir.join("portfolio.md");
        std::fs::write(&md_path, md)?;
        tracing::trace!(path = %md_path.display(), "deliver: portfolio.md written");

        // Phase D final checkpoint: confirm the portfolio before
        // terminating the run. Persisted under
        // `checkpoints/h_<uuid>.json` for auditability.
        if ctx.interactive {
            tracing::info!(
                winner = %ranking.winner,
                "deliver: final checkpoint fired"
            );
            let cp = Checkpoint::yes_no(
                CheckpointKind::Final,
                format!("ship portfolio with winner `{}`?", ranking.winner),
            );
            let opts = CheckpointOpts {
                interactive: true,
                stdin_override: None,
                telemetry: Some(ctx.telemetry.clone()),
            };
            // V4 §5.14 ships the portfolio only on Approved. Reject
            // aborts the run with Error::Cancelled so the operator
            // gets a non-zero exit and the manifest status flips to
            // 'failed'.
            match crate::checkpoint::ask(&cp, &ctx.run_dir().checkpoints(), &opts)? {
                crate::checkpoint::Resolution::Approved => {
                    tracing::debug!("deliver: final checkpoint approved");
                }
                crate::checkpoint::Resolution::Modify(text) => {
                    tracing::info!(text_len = text.len(), "deliver: final checkpoint modified");
                    // F1: persist the operator's correction. The
                    // current deliver call already shipped, so the
                    // note informs the next rank/deliver cycle
                    // (e.g. `moagan rerank`) rather than this run.
                    crate::checkpoint::persist_modify_note(ctx.run_dir().root(), "deliver", &text)?;
                }
                crate::checkpoint::Resolution::Rejected => {
                    tracing::warn!("deliver: final checkpoint rejected, cancelling run");
                    return Err(crate::error::Error::Cancelled(
                        "user rejected the final portfolio".into(),
                    ));
                }
            }
        }

        // D.17.8: drop the static dashboard HTML into the run
        // directory so the dashboard server can serve a self-
        // contained page that fetches `/api/runs` and renders the
        // run table. `write_dashboard` is idempotent (it
        // `create_dir_all` + `write` the same constant HTML), so
        // re-runs of `deliver` from `moagan rerank` are safe.
        // The io::Error is intentionally not propagated: a
        // transient failure to write the dashboard HTML must not
        // roll back the deliver phase's portfolio; the worst case
        // is a missing UI page, which the operator can regenerate
        // by re-running the dashboard command.
        if let Err(e) = dashboard_static::write_dashboard(ctx.run_dir().root()) {
            tracing::warn!(
                run_id = %ctx.run_id,
                error = %e,
                "failed to write dashboard.html; portfolio surface still written"
            );
        }

        Ok(PhaseOutput::Deliver(md_path))
    }
}

/// Load every `evaluations/p_*.json` keyed by proposal id. Missing or
/// unreadable files are silently skipped.
fn load_evaluations(dir: &std::path::Path) -> Vec<(String, Aggregated)> {
    let mut out: Vec<(String, Aggregated)> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("p_unknown")
            .to_owned();
        if let Ok(agg) = read_json::<Aggregated>(&path) {
            out.push((id, agg));
        }
    }
    out
}

/// Load every `critiques/p_*_critic_*.json` keyed by proposal id. The
/// issues arrays are concatenated so the divergence map can surface
/// every critic's complaint in one place.
fn load_critiques(dir: &std::path::Path) -> Vec<(String, Vec<String>)> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return map.into_iter().collect(),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        // stem looks like `p_000_critic_0`. Split off the trailing
        // `_critic_<n>` to recover the proposal id.
        let proposal_id = match stem.find("_critic_") {
            Some(idx) => stem[..idx].to_owned(),
            None => stem.to_owned(),
        };
        if let Ok(c) = read_json::<crate::domain::Critique>(&path) {
            for issue in c.issues {
                map.entry(proposal_id.clone())
                    .or_default()
                    .push(format!("{}: {issue}", c.verdict));
            }
            for sugg in c.suggestions {
                map.entry(proposal_id.clone())
                    .or_default()
                    .push(format!("suggestion: {sugg}"));
            }
        }
    }
    map.into_iter().collect()
}

/// Return a small marker that tells the reader whether a portfolio
/// entry is a regular proposal or a synthesized one (Phase D). The
/// `s_` prefix is the contract `SynthesizePhase` writes; proposals
/// carry `p_<NN>` ids. Empty string when neither applies.
pub fn kind_badge_for(id: &str) -> &'static str {
    if id.starts_with("synth_") || id.starts_with("s_") {
        "synthesis"
    } else {
        ""
    }
}

fn render_markdown(
    report: &FinalReport,
    ranking: &Ranking,
    evaluations: &[(String, Aggregated)],
    critiques: &[(String, Vec<String>)],
    run_id: &str,
    provider: &str,
    model: &str,
    mode: &str,
) -> String {
    let mut s = String::new();

    // §5.15 piece 1: resumen ejecutivo + portfolio (winner prose).
    s.push_str(&format!("# {}\n\n", report.title));
    s.push_str(&format!("{}\n\n", report.summary));
    s.push_str(&format!(
        "## Recommendation\n\n{}\n\n",
        report.recommendation
    ));

    // §5.15 piece 2: portfolio (top-3 representatives with badges).
    // We prefer the diverse representatives; if the front is too small
    // we fall back to the top-3 of the full ranking so the user always
    // sees three cards.
    let top3: Vec<&crate::domain::RankEntry> = if !ranking.representatives.is_empty() {
        ranking.representatives.iter().take(3).collect()
    } else {
        ranking.ranked.iter().take(3).collect()
    };
    s.push_str("## Portfolio (top-3)\n\n");
    for (i, r) in top3.iter().enumerate() {
        let badge = match i {
            0 => "winner",
            1 => "runner-up",
            2 => "third",
            _ => "",
        };
        let kind_badge = kind_badge_for(&r.id);
        let combined_badge = if kind_badge.is_empty() {
            badge.to_owned()
        } else if badge.is_empty() {
            kind_badge.to_owned()
        } else {
            format!("{badge}, {kind_badge}")
        };
        s.push_str(&format!(
            "{}. **{}** ({}) — score {:.2}\n   {}\n",
            i + 1,
            r.id,
            combined_badge,
            r.score,
            r.reason
        ));
    }
    s.push('\n');

    if !report.alternatives.is_empty() {
        s.push_str("## Alternatives\n\n");
        for a in &report.alternatives {
            s.push_str(&format!("- {a}\n"));
        }
        s.push('\n');
    }

    // §5.15 piece 3: matriz comparativa.
    if !evaluations.is_empty() {
        s.push_str("## Comparative matrix\n\n");
        s.push_str(
            "| Proposal | correctness | completeness | fit | evidence | clarity | overall |\n",
        );
        s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for (id, agg) in evaluations {
            s.push_str(&format!(
                "| `{id}` | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
                agg.correctness, agg.completeness, agg.fit, agg.evidence, agg.clarity, agg.score,
            ));
        }
        s.push('\n');
    }

    // §5.15 piece 4: mapa de divergencias.
    if !critiques.is_empty() {
        s.push_str("## Divergence map\n\n");
        for (id, issues) in critiques {
            if issues.is_empty() {
                continue;
            }
            s.push_str(&format!("### {id}\n\n"));
            for issue in issues {
                s.push_str(&format!("- {issue}\n"));
            }
            s.push('\n');
        }
    }

    // §5.15 piece 5: evidencia — pointer list to the sidecar files so
    // an inspector can follow the breadcrumb.
    s.push_str("## Evidence\n\n");
    s.push_str("- `manifest.json`\n");
    s.push_str("- `brief.json`\n");
    s.push_str("- `proposals/p_*.json`\n");
    s.push_str("- `proposals/s_*.json` (synthesized proposals, Phase D)\n");
    s.push_str("- `synthesized/s_*.json` (synthesis lineage, immutable)\n");
    s.push_str("- `cluster_proposals/cp_*.json` (intra-cluster grouping)\n");
    s.push_str("- `critiques/p_*_critic_*.json`\n");
    s.push_str("- `critiques/s_*_critic_*.json` (synthesis critiques)\n");
    s.push_str("- `evaluations/p_*.json`\n");
    s.push_str("- `evaluations/s_*.json` (synthesis evaluations)\n");
    s.push_str("- `adversaries/p_*.json` (third-judge reports)\n");
    s.push_str("- `validation/p_*.json`\n");
    s.push_str("- `rankings/ranking.json`\n");
    s.push_str("- `revisions/p_*_rev_*.json`\n");
    s.push_str("- `revisions/s_*_rev_*.json` (synthesis revisions)\n");
    s.push('\n');

    // §5.15 piece 6: auditoría — operator-facing provenance metadata.
    s.push_str("## Audit\n\n");
    s.push_str(&format!("- run_id: `{run_id}`\n"));
    s.push_str(&format!("- mode: `{mode}`\n"));
    s.push_str(&format!("- provider: `{provider}`\n"));
    s.push_str(&format!("- model: `{model}`\n"));
    s.push_str(&format!("- winner: `{}`\n", ranking.winner));
    s.push_str(&format!("- ranking size: {}\n", ranking.ranked.len()));
    s.push_str(&format!(
        "- representatives: {}\n",
        ranking.representatives.len()
    ));
    s.push('\n');

    if !report.next_steps.is_empty() {
        s.push_str("## Next Steps\n\n");
        for n in &report.next_steps {
            s.push_str(&format!("- {n}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FinalReport, RankEntry, Ranking};

    #[test]
    fn kind_badge_for_recognises_synthesized_prefix() {
        assert_eq!(kind_badge_for("s_00"), "synthesis");
        assert_eq!(kind_badge_for("synth_001"), "synthesis");
    }

    #[test]
    fn kind_badge_for_returns_empty_for_proposals() {
        assert_eq!(kind_badge_for("p_000"), "");
        assert_eq!(kind_badge_for("p_001"), "");
    }

    #[test]
    fn kind_badge_for_returns_empty_for_unknown_prefix() {
        assert_eq!(kind_badge_for("winner"), "");
        assert_eq!(kind_badge_for(""), "");
    }

    fn sample_ranking() -> Ranking {
        Ranking {
            ranked: vec![
                RankEntry {
                    id: "p_000".into(),
                    score: 8.5,
                    reason: "x".into(),
                },
                RankEntry {
                    id: "s_00".into(),
                    score: 7.9,
                    reason: "y".into(),
                },
                RankEntry {
                    id: "p_001".into(),
                    score: 7.5,
                    reason: "z".into(),
                },
            ],
            representatives: vec![RankEntry {
                id: "s_00".into(),
                score: 7.9,
                reason: "y".into(),
            }],
            winner: "p_000".into(),
            stability_score: None,
            stability_label: None,
            stability_sigma: None,
        }
    }

    fn sample_report() -> FinalReport {
        FinalReport {
            title: "T".into(),
            summary: "S".into(),
            recommendation: "R".into(),
            alternatives: vec!["a".into()],
            next_steps: vec!["n".into()],
        }
    }

    #[test]
    fn render_markdown_badge_marks_synthesized_in_portfolio() {
        let r = sample_ranking();
        let rep = sample_report();
        let md = render_markdown(
            &rep,
            &r,
            &[],
            &[],
            "rid",
            "minimax",
            "MiniMax-M3",
            "standard",
        );
        assert!(
            md.contains("synthesis"),
            "expected 'synthesis' badge for s_00, got:\n{md}"
        );
    }

    #[test]
    fn render_markdown_keeps_winner_for_regular_proposals() {
        let r = sample_ranking();
        let rep = sample_report();
        let md = render_markdown(
            &rep,
            &r,
            &[],
            &[],
            "rid",
            "minimax",
            "MiniMax-M3",
            "standard",
        );
        assert!(md.contains("winner"));
        assert!(md.contains("p_000"));
    }

    #[test]
    fn render_markdown_evidence_section_mentions_phase_d_paths() {
        let r = sample_ranking();
        let rep = sample_report();
        let md = render_markdown(
            &rep,
            &r,
            &[],
            &[],
            "rid",
            "minimax",
            "MiniMax-M3",
            "standard",
        );
        assert!(md.contains("synthesized/s_*.json"));
        assert!(md.contains("cluster_proposals/cp_*.json"));
        assert!(md.contains("proposals/s_*.json"));
        assert!(md.contains("adversaries/p_*.json"));
    }

    /// Test-only provider that records every `Request` it receives
    /// and replies with a canned `FinalReport` JSON. Used by the
    /// F1 prompt-injection test below to capture what the
    /// deliver-phase LLM call actually saw as its `user` prompt.
    struct RecordingDeliverProvider {
        name: String,
        model: String,
        recorded: parking_lot::Mutex<Vec<crate::llm::wire::Request>>,
        canned: crate::llm::wire::Response,
    }

    impl RecordingDeliverProvider {
        fn new() -> Self {
            Self {
                name: "recording".into(),
                model: "recording-model".into(),
                recorded: parking_lot::Mutex::new(Vec::new()),
                canned: crate::llm::wire::Response {
                    text:
                        r#"{"title":"T","summary":"S","recommendation":"R","alternatives":[],"next_steps":[]}"#
                            .to_owned(),
                    finish_reason: Some("end_turn".into()),
                    truncated: false,
                    usage: crate::llm::wire::Usage::default(),
                },
            }
        }

        fn recorded(&self) -> Vec<crate::llm::wire::Request> {
            self.recorded.lock().clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::llm::Provider for RecordingDeliverProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn model(&self) -> &str {
            &self.model
        }
        fn endpoint(&self) -> &str {
            "record://local"
        }
        async fn send(
            &self,
            req: &crate::llm::wire::Request,
        ) -> crate::error::Result<(u16, crate::llm::wire::Response)> {
            self.recorded.lock().push(req.clone());
            Ok((200, self.canned.clone()))
        }
    }

    /// F1: when an operator note is persisted to
    /// `<run_dir>/state/modify_note.json` before the deliver phase
    /// runs, the `user` prompt that the LLM receives must be
    /// wrapped with the operator note in a `[operator_modify_note]`
    /// tagged block. The contract is end-to-end: the test builds a
    /// real `RunContext`, wires a `RecordingDeliverProvider`, runs
    /// `DeliverPhase::execute`, and asserts on the recorded
    /// request.
    #[test]
    fn deliver_phase_includes_modify_note_in_prompt() -> crate::error::Result<()> {
        use std::sync::Arc;

        use crate::execution::Parallelism;
        use crate::fs_layout::MoaganHome;
        use crate::ids::RunId;
        use crate::llm::ProviderRegistry;
        use crate::phases::phase::{Phase, RunContext};
        use crate::phases::util::write_json;
        use crate::telemetry::Telemetry;

        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = Arc::new(MoaganHome::resolve().unwrap());
        home.ensure().unwrap();
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();

        // Minimal winning proposal (the deliver phase reads
        // `proposals/p_000.json` first; revisions are tried next).
        let proposal = Proposal {
            id: "p_000".into(),
            summary: "winning summary".into(),
            ..Proposal::default()
        };
        let proposal_path = run_dir.proposals().join("p_000.json");
        std::fs::create_dir_all(proposal_path.parent().unwrap()).unwrap();
        write_json(&proposal_path, &proposal)?;

        // Minimal ranking sidecar.
        let ranking = Ranking {
            ranked: vec![RankEntry {
                id: "p_000".into(),
                score: 9.0,
                reason: "test winner".into(),
            }],
            representatives: vec![],
            winner: "p_000".into(),
            stability_score: None,
            stability_label: None,
            stability_sigma: None,
        };
        let ranking_path = run_dir.rankings().join("ranking.json");
        std::fs::create_dir_all(ranking_path.parent().unwrap()).unwrap();
        write_json(&ranking_path, &ranking)?;

        // F1: persist the operator note *before* deliver runs.
        let run_root = run_dir.root().to_path_buf();
        crate::checkpoint::persist_modify_note(
            &run_root,
            "rank",
            "drop weak evidence in the recommendation",
        )?;

        // Wire the recording provider into the registry. The
        // deliver phase resolves its provider through
        // `RunContext::provider()` which looks up by
        // `default_provider == "mock"` here — we register under
        // "mock" so the lookup path stays identical to the
        // production wire-up.
        let recorder = Arc::new(RecordingDeliverProvider::new());
        let mut registry = ProviderRegistry::default();
        registry.insert(
            "mock".into(),
            recorder.clone() as Arc<dyn crate::llm::Provider>,
        );

        // Non-interactive so the deliver phase skips the final
        // checkpoint prompt (we only care about the LLM call's
        // user prompt, not the operator interaction).
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            Arc::new(registry),
            "mock".into(),
            "recording-model".into(),
            Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        let phase = DeliverPhase;
        pollster::block_on(phase.execute(&ctx))?;

        // The recording provider must have at least one entry —
        // the deliver phase makes at least one LLM call (the
        // `FinalReport` synthesis). The very last attempt's
        // `user` field is the prompt that would be re-sent on a
        // parse retry; what we care about is that the operator
        // note is present on every recorded prompt.
        let calls = recorder.recorded();
        assert!(
            !calls.is_empty(),
            "deliver phase must invoke the LLM at least once"
        );
        for call in &calls {
            assert!(
                call.user.contains("[operator_modify_note]"),
                "user prompt must open the note tag; got:\n{}",
                call.user
            );
            assert!(
                call.user
                    .contains("drop weak evidence in the recommendation"),
                "operator note text must appear; got:\n{}",
                call.user
            );
            assert!(
                call.user.contains("[/operator_modify_note]"),
                "user prompt must close the note tag; got:\n{}",
                call.user
            );
            // The underlying ranking payload must still be
            // present below the note block — prepending is
            // additive.
            assert!(
                call.user.contains("p_000"),
                "underlying ranking payload must remain; got:\n{}",
                call.user
            );
        }
        Ok(())
    }
}
