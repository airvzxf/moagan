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

use crate::domain::constraint::{HARD_INCOMPATIBILITIES, find_conflicts};
use crate::domain::{MergePlan, Proposal, SynthesizedProposal};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::cluster_proposals::ProposalCluster;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::time::now_unix_secs;

/// Cheap whole-word substring check used by `extract_tags`. Returns
/// `true` when `tag` appears in `text` delimited by a non-alphanumeric
/// boundary on both sides (or at a string boundary). `text` is
/// expected to be lowercase and `tag` is matched lowercase.
fn word_contains(text: &str, tag: &str) -> bool {
    if tag.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len();
    let mut start = 0;
    while start + tag_len <= bytes.len() {
        // Fast substring search first.
        if &bytes[start..start + tag_len] != tag_bytes {
            start += 1;
            continue;
        }
        // Check the left boundary: non-alphanumeric or string start.
        let left_ok = start == 0 || !is_alnum(bytes[start - 1]);
        // Check the right boundary: non-alphanumeric or string end.
        let right_idx = start + tag_len;
        let right_ok = right_idx == bytes.len() || !is_alnum(bytes[right_idx]);
        if left_ok && right_ok {
            return true;
        }
        start += 1;
    }
    false
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

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

/// Convert a `MergePlan` (the new MergeSynthesizer role output) into
/// the existing `SynthesizedProposal` shape that the downstream
/// pipeline already understands. The two structures are equivalent
/// modulo the richer `hard_constraint_check` field on the plan,
/// which we keep on the plan and surface as an extra `evidence`
/// line on the proposal (the schema for downstream phases doesn't
/// need the structured map).
pub fn merge_plan_to_synthesized(
    plan: MergePlan,
    cluster: &crate::phases::cluster_proposals::ProposalCluster,
    target_id: &str,
) -> SynthesizedProposal {
    let now = now_unix_secs();
    let sources: Vec<String> = if plan.sources.is_empty() {
        cluster.member_proposals.clone()
    } else {
        plan.sources
    };
    let evidence = if plan.hard_constraint_check.is_empty() {
        plan.evidence
    } else {
        let mut evidence = plan.evidence;
        let hard = plan
            .hard_constraint_check
            .iter()
            .map(|(k, ok)| format!("hard:{}={}", k, ok))
            .collect::<Vec<_>>()
            .join(", ");
        evidence.push(format!("hard_constraints[{hard}]"));
        evidence
    };
    SynthesizedProposal {
        id: target_id.to_string(),
        source_proposals: cluster.member_proposals.clone(),
        cluster_id: cluster.id.clone(),
        synthesis_strategy: "merge_invariants".into(),
        summary: plan.summary,
        approach: plan.approach,
        tradeoffs: plan.tradeoffs,
        evidence,
        sources,
        created_unix: now,
        schema_version: "v1".into(),
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

    /// Extract candidate architectural tags from a proposal's textual
    /// fields. The match is a whole-word, case-insensitive scan over
    /// the summary, approach, tradeoffs, and evidence so we never miss
    /// a tag the model wrote in the body. Returns deduplicated tags
    /// preserving first-seen order.
    pub fn extract_tags(proposal: &Proposal) -> Vec<String> {
        // Build the search corpus: every public text field on the
        // proposal. Tradeoffs and evidence are joined so a tag like
        // "sql" listed in the evidence array is found.
        let mut corpus = String::new();
        corpus.push_str(&proposal.summary);
        corpus.push('\n');
        corpus.push_str(&proposal.approach);
        corpus.push('\n');
        for t in &proposal.tradeoffs {
            corpus.push_str(t);
            corpus.push('\n');
        }
        for e in &proposal.evidence {
            corpus.push_str(e);
            corpus.push('\n');
        }
        let corpus_lower = corpus.to_lowercase();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for (a, b) in HARD_INCOMPATIBILITIES {
            for tag in [*a, *b] {
                if !seen.contains(tag) && word_contains(&corpus_lower, tag) {
                    seen.insert(tag.to_string());
                    out.push(tag.to_string());
                }
            }
        }
        out
    }

    /// Detect incompatible tag pairs across the cluster's proposals.
    /// Returns the offending `(tag_a, tag_b)` pair (first match
    /// wins) plus the full tag list collected from every proposal.
    pub fn cluster_conflict(proposals: &[Proposal]) -> Option<(String, String, Vec<String>)> {
        let mut all_tags: Vec<String> = Vec::new();
        for p in proposals {
            all_tags.extend(Self::extract_tags(p));
        }
        let borrowed: Vec<&str> = all_tags.iter().map(String::as_str).collect();
        if let Some((a, b)) = find_conflicts(&borrowed).into_iter().next() {
            Some((a.to_string(), b.to_string(), all_tags))
        } else {
            None
        }
    }

    /// Persist a `synthesized/skipped_<NN>.json` sidecar in `dir`.
    /// This is the canonical filesystem-first write; the caller is
    /// expected to mirror the row into SQLite afterwards.
    pub fn write_skipped_in_dir(
        dir: &std::path::Path,
        cluster_id: &str,
        skipped_seq: usize,
        conflict: &(String, String, Vec<String>),
    ) -> Result<PathBuf> {
        #[derive(serde::Serialize)]
        struct SkippedCluster {
            cluster_id: String,
            skipped: bool,
            reason: String,
            tags: Vec<String>,
            schema_version: String,
        }
        let (a, b, tags) = conflict;
        let payload = SkippedCluster {
            cluster_id: cluster_id.to_string(),
            skipped: true,
            reason: format!("incompatible_tags: {a},{b}"),
            tags: tags.clone(),
            schema_version: "v1".into(),
        };
        let bytes = serde_json::to_vec_pretty(&payload)?;
        let path = dir.join(format!("skipped_{:02}.json", skipped_seq));
        crate::atomic::writer::AtomicWriter::new().write(&path, &bytes)?;
        Ok(path)
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
                // K.1 (proposal-03 §D.13.15): skip clusters whose
                // proposals mix hard-incompatible tags. The synthesizer
                // LLM would otherwise be asked to merge contradictory
                // decisions (e.g. monolith + microservices) which
                // produces incoherent output.
                if let Some(conflict) = SynthesizePhase::cluster_conflict(&proposals) {
                    let (a, b, _tags) = &conflict;
                    tracing::warn!(
                        cluster_id = %cluster.id,
                        tag_a = %a,
                        tag_b = %b,
                        "synthesize phase skipping cluster: incompatible tags"
                    );
                    let skipped_path = SynthesizePhase::write_skipped_in_dir(
                        &dir,
                        &cluster.id,
                        idx,
                        &conflict,
                    )?;
                    return Ok(Some(skipped_path));
                }
                let user = SynthesizePhase::user_payload(
                    &cluster.id,
                    &target_id,
                    &proposals,
                );
                // V1: route the intra-cluster merge through the
                // catalog role `MergeSynthesizer` (D.7.1) instead
                // of the legacy `Synthesizer`. The new role returns
                // a `MergePlan` with a stricter schema (sources
                // array, hard_constraint_check, evidence per
                // source); the phase converts it to the
                // `SynthesizedProposal` shape the downstream
                // pipeline already consumes.
                let system = system_prompt(Role::MergeSynthesizer).to_owned();
                let plan: MergePlan = ctx
                    .call_with_retry_parse(
                        Role::MergeSynthesizer,
                        system,
                        user,
                        "MergePlan: {summary, approach, tradeoffs[], evidence[], sources[], hard_constraint_check{...}, expected_validation}",
                        3,
                    )
                    .await?;
                let mut parsed = merge_plan_to_synthesized(plan, &cluster, &target_id);
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

    /// V1: `merge_plan_to_synthesized` carries the MergePlan fields
    /// forward and merges `hard_constraint_check` into the evidence
    /// stream so downstream phases (Gate, Critique, etc.) can still
    /// see why a synthesis was rejected.
    #[test]
    fn merge_plan_to_synthesized_carries_fields_and_hard_constraints() {
        let mut plan = MergePlan::default();
        plan.summary = "summary text".into();
        plan.approach = "## Approach\n\nbody".into();
        plan.tradeoffs = vec!["t1".into()];
        plan.evidence = vec!["sk_001".into()];
        plan.sources = vec!["p_001".into(), "p_002".into()];
        plan
            .hard_constraint_check
            .insert("single_binary".into(), true);
        plan.expected_validation = "unit tests pass".into();
        let cluster = crate::phases::cluster_proposals::ProposalCluster {
            schema_version: "v1".into(),
            id: "cp_00".into(),
            member_proposals: vec!["p_001".into(), "p_002".into()],
            cluster_text_sample: String::new(),
            created_unix: 0,
        };
        let s = merge_plan_to_synthesized(plan, &cluster, "s_00");
        assert_eq!(s.id, "s_00");
        assert_eq!(s.summary, "summary text");
        assert_eq!(s.sources, vec!["p_001".to_string(), "p_002".into()]);
        assert_eq!(s.cluster_id, "cp_00");
        assert_eq!(s.synthesis_strategy, "merge_invariants");
        // hard_constraint_check is surfaced as a single extra
        // evidence line so the downstream pipeline can show it.
        assert!(
            s.evidence.iter().any(|e| e.contains("hard_constraints[")),
            "evidence should carry hard_constraints line, got {:?}",
            s.evidence
        );
    }
}
