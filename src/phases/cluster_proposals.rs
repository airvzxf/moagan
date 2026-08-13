//! Cluster proposals by similarity. Phase D (V4 §5.13 + T01-06 §8.4).
//!
//! Reads every `proposals/p_*.json` (and falls back to its
//! `revisions/p_*_rev_0.json` when present, mirroring `JudgePhase`),
//! clusters them with the SimHash / Jaccard helper from
//! `src/ranking/cluster.rs`, and writes one `cluster_proposals/cp_<NN>.json`
//! per cluster. The output drives `SynthesizePhase` — each cluster
//! becomes a candidate for synthesis.
//!
//! The clustering threshold is intentionally permissive (0.7) so the
//! MCP pipeline produces a small number of clusters on a typical MVP
//! run (3 proposals → 1 or 2 clusters).

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::Proposal;
use crate::error::{Error, Result};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::ranking::cluster::cluster_by_simhash;
use crate::time::now_unix_secs;

/// Default Jaccard threshold for proposal clustering (V4 §5.13).
pub const CLUSTER_THRESHOLD: f32 = 0.7;

/// One cluster of proposals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalCluster {
    /// Schema version.
    #[serde(default = "schema_version")]
    pub schema_version: String,
    /// Cluster id (`cp_<NN>`).
    pub id: String,
    /// Proposal ids that belong to this cluster.
    pub member_proposals: Vec<String>,
    /// Concatenated source text (`summary + approach + tradeoffs +
    /// evidence`) used for clustering, kept verbatim for debugging.
    pub cluster_text_sample: String,
    /// Unix seconds when this file was written.
    pub created_unix: i64,
}

fn schema_version() -> String {
    "v1".to_owned()
}

impl Default for ProposalCluster {
    fn default() -> Self {
        Self {
            schema_version: schema_version(),
            id: String::new(),
            member_proposals: Vec::new(),
            cluster_text_sample: String::new(),
            created_unix: 0,
        }
    }
}

/// Cluster-proposals phase. Always runs after `CritiquePhase` and
/// before `SynthesizePhase`. Skips itself when there are fewer than
/// two proposals — nothing to merge.
pub struct ClusterProposalsPhase {
    /// Jaccard threshold (0..=1). Default `CLUSTER_THRESHOLD`.
    pub threshold: f32,
}

impl Default for ClusterProposalsPhase {
    fn default() -> Self {
        Self {
            threshold: CLUSTER_THRESHOLD,
        }
    }
}

impl ClusterProposalsPhase {
    /// Compute the text used for clustering: summary + approach +
    /// tradeoffs + evidence. Pure function so tests can pin it.
    fn cluster_text(p: &Proposal) -> String {
        let mut s = String::new();
        s.push_str(&p.summary);
        s.push('\n');
        s.push_str(&p.approach);
        s.push('\n');
        for t in &p.tradeoffs {
            s.push_str(t);
            s.push('\n');
        }
        for e in &p.evidence {
            s.push_str(e);
            s.push('\n');
        }
        s
    }

    /// Load every proposal from disk. For each id, prefer the
    /// latest revision (`p_<id>_rev_<n>.json`) when present;
    /// otherwise fall back to the original proposal.
    fn load_proposals(ctx: &RunContext) -> Result<Vec<(String, Proposal)>> {
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let mut out: Vec<(String, Proposal)> = Vec::new();
        let entries = match std::fs::read_dir(&proposals_dir) {
            Ok(e) => e,
            Err(_) => return Ok(out),
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
            // Try revisions first (newest first).
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
                None => read_json::<Proposal>(&path)?,
            };
            out.push((id, proposal));
        }
        Ok(out)
    }

    /// Build the cluster text list and cluster ids.
    fn cluster(&self, items: &[(String, Proposal)]) -> Vec<ProposalCluster> {
        let texts: Vec<String> = items.iter().map(|(_, p)| Self::cluster_text(p)).collect();
        let groups = cluster_by_simhash(&texts, self.threshold);
        let now = now_unix_secs();
        groups
            .into_iter()
            .enumerate()
            .map(|(idx, members)| {
                let member_proposals: Vec<String> =
                    members.iter().map(|i| items[*i].0.clone()).collect();
                let sample = members
                    .iter()
                    .map(|i| texts[*i].clone())
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                ProposalCluster {
                    schema_version: schema_version(),
                    id: format!("cp_{:02}", idx),
                    member_proposals,
                    cluster_text_sample: sample,
                    created_unix: now,
                }
            })
            .collect()
    }
}

#[async_trait]
impl Phase for ClusterProposalsPhase {
    fn name(&self) -> &'static str {
        "cluster_proposals"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let dir = ctx.run_dir().cluster_proposals_dir();
        std::fs::create_dir_all(&dir)?;

        let items = Self::load_proposals(ctx)?;
        if items.len() < 2 {
            // Nothing meaningful to cluster. Write an empty marker so
            // downstream phases can detect "we ran, but there was
            // nothing to merge".
            let empty = ProposalCluster {
                schema_version: schema_version(),
                id: "cp_00".to_owned(),
                member_proposals: Vec::new(),
                cluster_text_sample: String::new(),
                created_unix: now_unix_secs(),
            };
            let path = dir.join("cp_00.json");
            write_json(&path, &empty)?;
            return Ok(PhaseOutput::ClusterProposals(vec![path]));
        }

        let clusters = self.cluster(&items);
        let mut paths: Vec<PathBuf> = Vec::with_capacity(clusters.len());
        for c in clusters {
            let path = dir.join(format!("{}.json", c.id));
            write_json(&path, &c)?;
            paths.push(path);
        }
        if paths.is_empty() {
            return Err(Error::InvalidState(
                "cluster_proposals produced zero clusters".into(),
            ));
        }
        Ok(PhaseOutput::ClusterProposals(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_text_concatenates_summary_approach_tradeoffs_evidence() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "summary".into(),
            approach: "approach".into(),
            tradeoffs: vec!["t1".into(), "t2".into()],
            evidence: vec!["e1".into()],
            source_sketch: String::new(),
            artifacts: Vec::new(),
            replaced_by: None,
            source_nodes: Vec::new(),
        };
        let text = ClusterProposalsPhase::cluster_text(&p);
        assert!(text.contains("summary"));
        assert!(text.contains("approach"));
        assert!(text.contains("t1"));
        assert!(text.contains("e1"));
    }

    #[test]
    fn default_threshold_matches_spec() {
        let phase = ClusterProposalsPhase::default();
        assert!((phase.threshold - 0.7).abs() < 1e-6);
    }

    #[test]
    fn cluster_returns_one_per_group_for_simhash() {
        // Two clearly different proposals should produce two clusters.
        let phase = ClusterProposalsPhase::default();
        let items = vec![
            (
                "p_001".to_owned(),
                Proposal {
                    id: "p_001".into(),
                    summary: "Use a SQL database".into(),
                    approach: "ACID transactions".into(),
                    tradeoffs: vec![],
                    evidence: vec![],
                    source_sketch: String::new(),
                    artifacts: Vec::new(),
                    replaced_by: None,
                    source_nodes: Vec::new(),
                },
            ),
            (
                "p_002".to_owned(),
                Proposal {
                    id: "p_002".into(),
                    summary: "Use an in-memory cache".into(),
                    approach: "Redis-like key-value store".into(),
                    tradeoffs: vec![],
                    evidence: vec![],
                    source_sketch: String::new(),
                    artifacts: Vec::new(),
                    replaced_by: None,
                    source_nodes: Vec::new(),
                },
            ),
        ];
        let clusters = phase.cluster(&items);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].member_proposals, vec!["p_001".to_string()]);
        assert_eq!(clusters[1].member_proposals, vec!["p_002".to_string()]);
    }

    #[test]
    fn cluster_merges_near_duplicates() {
        let phase = ClusterProposalsPhase { threshold: 0.99 };
        let items = vec![
            (
                "p_001".to_owned(),
                Proposal {
                    id: "p_001".into(),
                    summary: "Use Rust and SQLite".into(),
                    approach: "single binary".into(),
                    tradeoffs: vec![],
                    evidence: vec![],
                    source_sketch: String::new(),
                    artifacts: Vec::new(),
                    replaced_by: None,
                    source_nodes: Vec::new(),
                },
            ),
            (
                "p_002".to_owned(),
                Proposal {
                    id: "p_002".into(),
                    summary: "Use Rust and SQLite single binary".into(),
                    approach: "tight integration".into(),
                    tradeoffs: vec![],
                    evidence: vec![],
                    source_sketch: String::new(),
                    artifacts: Vec::new(),
                    replaced_by: None,
                    source_nodes: Vec::new(),
                },
            ),
        ];
        let clusters = phase.cluster(&items);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].member_proposals.len(), 2);
        assert_eq!(clusters[0].id, "cp_00");
    }

    #[test]
    fn empty_items_yield_empty_clusters() {
        let phase = ClusterProposalsPhase::default();
        let clusters = phase.cluster(&[]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn proposal_cluster_round_trips() {
        let c = ProposalCluster {
            schema_version: "v1".into(),
            id: "cp_02".into(),
            member_proposals: vec!["p_001".into(), "p_002".into()],
            cluster_text_sample: "merged".into(),
            created_unix: 1_700_000_000,
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: ProposalCluster = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, "cp_02");
        assert_eq!(back.member_proposals.len(), 2);
        assert_eq!(back.schema_version, "v1");
    }
}
