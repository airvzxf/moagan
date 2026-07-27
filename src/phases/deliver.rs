//! Deliver phase. Reads the ranking and the winning proposal, asks
//! the model to write the final user-facing response, and writes
//! `final/portfolio.md` plus `final/portfolio.json`.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::domain::{FinalReport, Proposal, Ranking};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
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
        let ranking: Ranking = read_json(&ctx.run_dir().rankings().join("ranking.json"))?;
        let proposals_dir = ctx.run_dir().proposals();
        let winner_proposal: Proposal =
            read_json(&proposals_dir.join(format!("{}.json", ranking.winner))).or_else(|_| {
                let p = ctx
                    .run_dir()
                    .revisions()
                    .join(format!("{}_rev_0.json", ranking.winner));
                read_json(&p)
            })?;
        let user = serde_json::to_string(&serde_json::json!({
            "winner": ranking.winner,
            "proposal": winner_proposal,
            "ranked": ranking.ranked,
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
        let md = render_markdown(&report, &ranking);
        let md_path: PathBuf = final_dir.join("portfolio.md");
        std::fs::write(&md_path, md)?;
        Ok(PhaseOutput::Deliver(md_path))
    }
}

fn render_markdown(report: &FinalReport, ranking: &Ranking) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {}\n\n", report.title));
    s.push_str(&format!("{}\n\n", report.summary));
    s.push_str(&format!(
        "## Recommendation\n\n{}\n\n",
        report.recommendation
    ));
    if !report.alternatives.is_empty() {
        s.push_str("## Alternatives\n\n");
        for a in &report.alternatives {
            s.push_str(&format!("- {a}\n"));
        }
        s.push('\n');
    }
    s.push_str("## Ranking\n\n");
    for (i, r) in ranking.ranked.iter().enumerate() {
        s.push_str(&format!(
            "{}. **{}** — score {:.2}: {}\n",
            i + 1,
            r.id,
            r.score,
            r.reason
        ));
    }
    s.push('\n');
    if !report.next_steps.is_empty() {
        s.push_str("## Next Steps\n\n");
        for n in &report.next_steps {
            s.push_str(&format!("- {n}\n"));
        }
    }
    s
}
