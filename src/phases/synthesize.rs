//! Synthesize phase. Phase D (V4 §5.13 + T01-06 §8.4).
//!
//! Reads every cluster produced by `ClusterProposalsPhase`, picks the
//! clusters that warrant a synthesis (default: clusters with ≥2
//! members), and asks the `Synthesizer` role to merge each cluster's
//! proposals into one `SynthesizedProposal`. The synthesized proposal
//! then competes against its sources per V4 §5.13; `RankPhase` reads
//! `synthesized/` and folds each `SynthesizedProposal` into the
//! ranking as if it were a normal proposal.
//!
//! The phase never produces a synthesized proposal for a singleton
//! cluster — synthesizing a single source is just a copy and the
//! `integrator` role would add no signal. To force synthesis on a
//! singleton set `force_singletons = true`.
//!
//! Pipeline propagation (V4 §5.13 + T01-06 §8.4): the synthesized
//! proposal "competes" — it passes gates, receives critique, is
//! evaluated, and enters the Pareto front. To make that work with
//! the existing phase pipeline (which iterates over `proposals/*.json`),
//! this phase writes two artifacts per synthesis:
//!
//! 1. `synthesized/s_<NN>.json` — the immutable lineage record
//!    carrying `source_proposals`, `cluster_id`, and `synthesis_strategy`.
//! 2. `proposals/s_<NN>.json` — a copy shaped as a `Proposal` so the
//!    downstream phases (`Gate`, `Critique`, `Repair`, `Judge`,
//!    `Rank`, `Deliver`) treat it like any other proposal.
//!
//! The `s_` prefix avoids collision with `p_<NN>` ids in `proposals/`
//! and lets `DeliverPhase` badge these as "synthesis" entries.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::{Proposal, SynthesizedProposal};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::cluster_proposals::ProposalCluster;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::time::now_unix_secs;

/// Convert a `SynthesizedProposal` into a `Proposal` for the pipeline.
/// The synthesized proposal keeps its `s_<NN>` id and inherits the
/// approach / summary / tradeoffs / evidence from the synthesizer.
/// `source_sketch` records the cluster the synthesis came from so
/// later phases can reconstruct the lineage if they need to.
pub fn synth_to_proposal(synth: &SynthesizedProposal) -> Proposal {
    Proposal {
        id: synth.id.clone(),
        summary: synth.summary.clone(),
        approach: synth.approach.clone(),
        tradeoffs: synth.tradeoffs.clone(),
        evidence: synth.evidence.clone(),
        source_sketch: format!("syn_from_{}", synth.cluster_id),
        artifacts: Vec::new(),
        replaced_by: None,
        source_nodes: Vec::new(),
    }
}

/// Synthesize phase. For each cluster with ≥2 members, calls the
/// `synthesizer` role to merge the cluster's proposals.
pub struct SynthesizePhase {
    /// Minimum cluster size that triggers synthesis. Default 2 —
    /// synthesizing a single source has no informational value.
    pub min_cluster_size: usize,
    /// Force synthesis on singleton clusters (mostly for tests).
    pub force_singletons: bool,
}

impl Default for SynthesizePhase {
    fn default() -> Self {
        Self {
            min_cluster_size: 2,
            force_singletons: false,
        }
    }
}

impl SynthesizePhase {
    /// Build the LLM user payload. The synthesizer receives the
    /// cluster's proposals plus its id and the target `s_<NN>` it
    /// must reuse.
    fn user_payload(cluster_id: &str, target_id: &str, proposals: &[Proposal]) -> String {
        let proposals_json = serde_json::to_string(proposals).unwrap_or_else(|_| "[]".to_owned());
        format!(
            "Cluster id: {cluster_id}\n\
             Target synthesized id: {target_id}\n\n\
             Source proposals (the cluster's members):\n\n\
             {proposals_json}\n\n\
             Return the JSON object described in the system prompt.",
        )
    }

    /// Read every `cluster_proposals/cp_*.json` from disk.
    fn load_clusters(ctx: &RunContext) -> Result<Vec<ProposalCluster>> {
        let dir = ctx.run_dir().cluster_proposals_dir();
        let mut out: Vec<ProposalCluster> = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(out),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            match read_json::<ProposalCluster>(&path) {
                Ok(c) => out.push(c),
                Err(_) => continue, // skip malformed files
            }
        }
        Ok(out)
    }

    /// Load every proposal referenced by a cluster. Mirrors the
    /// revision-aware lookup in `ClusterProposalsPhase::load_proposals`
    /// so synthesis sees the latest repaired version.
    fn load_proposals_for_cluster(ctx: &RunContext, ids: &[String]) -> Result<Vec<Proposal>> {
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let mut out: Vec<Proposal> = Vec::with_capacity(ids.len());
        for id in ids {
            let mut picked: Option<Proposal> = None;
            for n in (0..16).rev() {
                let rev_path: PathBuf = revisions_dir.join(format!("{id}_rev_{n}.json"));
                if rev_path.exists()
                    && let Ok(p) = read_json::<Proposal>(&rev_path)
                {
                    picked = Some(p);
                    break;
                }
            }
            let proposal = match picked {
                Some(p) => p,
                None => read_json::<Proposal>(&proposals_dir.join(format!("{id}.json")))?,
            };
            out.push(proposal);
        }
        Ok(out)
    }
}

#[async_trait]
impl Phase for SynthesizePhase {
    fn name(&self) -> &'static str {
        "synthesize"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let dir = ctx.run_dir().synthesized();
        std::fs::create_dir_all(&dir)?;
        let dir = std::sync::Arc::new(dir);

        let clusters = Self::load_clusters(ctx)?;
        let eligible: Vec<&ProposalCluster> = clusters
            .iter()
            .filter(|c| self.force_singletons || c.member_proposals.len() >= self.min_cluster_size)
            .collect();

        if eligible.is_empty() {
            return Ok(PhaseOutput::Synthesized(Vec::new()));
        }

        let futures = eligible.iter().enumerate().map(|(idx, cluster)| {
            let cluster: ProposalCluster = (*cluster).clone();
            let ctx = ctx.clone();
            let dir = std::sync::Arc::clone(&dir);
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let target_id = format!("s_{:02}", idx);
                let proposals =
                    SynthesizePhase::load_proposals_for_cluster(&ctx, &cluster.member_proposals)?;
                if proposals.is_empty() {
                    return Ok::<Option<PathBuf>, crate::error::Error>(None);
                }
                let user = SynthesizePhase::user_payload(
                    &cluster.id,
                    &target_id,
                    &proposals,
                );
                let system = system_prompt(Role::Synthesizer).to_owned();
                let parsed: SynthesizedProposal = ctx
                    .call_with_retry_parse(
                        Role::Synthesizer,
                        system,
                        user,
                        "SynthesizedProposal: {id, source_proposals[], cluster_id, synthesis_strategy, summary, approach, tradeoffs[], evidence[], sources[]}",
                        3,
                    )
                    .await?;
                let mut parsed = parsed;
                if parsed.id.is_empty() {
                    parsed.id = target_id.clone();
                }
                if parsed.cluster_id.is_empty() {
                    parsed.cluster_id = cluster.id.clone();
                }
                if parsed.source_proposals.is_empty() {
                    parsed.source_proposals = cluster.member_proposals.clone();
                }
                if parsed.sources.is_empty() {
                    parsed.sources = cluster.member_proposals.clone();
                }
                parsed.created_unix = now_unix_secs();
                let path = dir.join(format!("{}.json", parsed.id));
                write_json(&path, &parsed)?;

                // Phase D propagation (V4 §5.13 + T01-06 §8.4):
                // also drop a copy into `proposals/` shaped as a
                // `Proposal` so the downstream Gate / Critique /
                // Repair / Judge / Rank / Deliver phases pick the
                // synthesis up and it enters the Pareto front.
                let proposal = synth_to_proposal(&parsed);
                let prop_path = ctx
                    .run_dir()
                    .proposals()
                    .join(format!("{}.json", proposal.id));
                write_json(&prop_path, &proposal)?;

                Ok(Some(path))
            }
        });

        let results = join_all(futures).await;
        let mut paths: Vec<PathBuf> = Vec::new();
        for r in results {
            if let Some(p) = r? {
                paths.push(p);
            }
        }
        Ok(PhaseOutput::Synthesized(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_min_cluster_size_is_two() {
        let phase = SynthesizePhase::default();
        assert_eq!(phase.min_cluster_size, 2);
        assert!(!phase.force_singletons);
    }

    #[test]
    fn user_payload_contains_target_and_cluster() {
        let p = Proposal::default();
        let s = SynthesizePhase::user_payload("cp_00", "s_00", &[p]);
        assert!(s.contains("cp_00"));
        assert!(s.contains("s_00"));
    }

    #[test]
    fn synth_to_proposal_preserves_id() {
        let s = SynthesizedProposal {
            id: "s_07".into(),
            cluster_id: "cp_03".into(),
            summary: "summary text".into(),
            approach: "## Approach\n\nbody".into(),
            tradeoffs: vec!["t1".into()],
            evidence: vec!["sk_001".into()],
            ..Default::default()
        };
        let p = synth_to_proposal(&s);
        assert_eq!(p.id, "s_07");
    }

    #[test]
    fn synth_to_proposal_preserves_fields() {
        let s = SynthesizedProposal {
            id: "s_00".into(),
            cluster_id: "cp_00".into(),
            summary: "s".into(),
            approach: "a".into(),
            tradeoffs: vec!["t".into()],
            evidence: vec!["e".into()],
            ..Default::default()
        };
        let p = synth_to_proposal(&s);
        assert_eq!(p.summary, "s");
        assert_eq!(p.approach, "a");
        assert_eq!(p.tradeoffs, vec!["t".to_string()]);
        assert_eq!(p.evidence, vec!["e".to_string()]);
        assert!(p.artifacts.is_empty());
    }

    #[test]
    fn synth_to_proposal_records_source_cluster() {
        let s = SynthesizedProposal {
            id: "s_02".into(),
            cluster_id: "cp_99".into(),
            ..Default::default()
        };
        let p = synth_to_proposal(&s);
        assert_eq!(p.source_sketch, "syn_from_cp_99");
    }
}
