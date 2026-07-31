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

use crate::checkpoint::{Checkpoint, CheckpointKind, CheckpointOpts};
use crate::domain::{FinalReport, Proposal, Ranking};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::judge::Aggregated;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Deliver phase.
pub struct DeliverPhase;

#[async_trait]
impl Phase for DeliverPhase {
    fn name(&self) -> &'static str {
        "deliver"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let ranking_path = ctx.run_dir().rankings().join("ranking.json");
        let ranking: Ranking = read_json(&ranking_path)?;
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let winner_proposal: Proposal =
            read_json(&proposals_dir.join(format!("{}.json", ranking.winner))).or_else(|_| {
                let p = revisions_dir.join(format!("{}_rev_0.json", ranking.winner));
                read_json(&p)
            })?;

        let evaluations = load_evaluations(&ctx.run_dir().evaluations());
        let critiques = load_critiques(&ctx.run_dir().critiques());

        let user = serde_json::to_string(&serde_json::json!({
            "winner": ranking.winner,
            "proposal": winner_proposal,
            "ranked": ranking.ranked,
            "representatives": ranking.representatives,
        }))
        .map_err(crate::Error::from)?;
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

        let final_dir = ctx.run_dir().final_dir();
        std::fs::create_dir_all(&final_dir)?;
        let json_path: PathBuf = final_dir.join("portfolio.json");
        write_json(&json_path, &report)?;

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

        // Phase D final checkpoint: confirm the portfolio before
        // terminating the run. Persisted under
        // `checkpoints/h_<uuid>.json` for auditability.
        if ctx.interactive {
            let cp = Checkpoint::yes_no(
                CheckpointKind::Final,
                format!("ship portfolio with winner `{}`?", ranking.winner),
            );
            let opts = CheckpointOpts {
                interactive: true,
                stdin_override: None,
                telemetry: Some(ctx.telemetry.clone()),
            };
            let _ = crate::checkpoint::ask(&cp, &ctx.run_dir().checkpoints(), &opts)?;
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
}
