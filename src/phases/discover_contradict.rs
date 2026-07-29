//! Discovery mode — `discover_contradict` phase.
//!
//! For each pair of clusters with a sufficiently high disagreement
//! score (default: cohesion delta), the LLM is asked to summarise
//! the contradiction with a topic, description, and severity.
//!
//! Output: `contradictions/contradictions.json` containing one
//! `Contradiction` per pair.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::discovery::contradiction::{severity_rank, top_pairs, ContradictionRecord};
use crate::domain::{Cluster, Contradiction};
use crate::error::Result;
use crate::ids::RunId;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Maximum cross-cluster pairs to surface. The detection runs in
/// O(n^2) so we cap the input here.
const MAX_PAIRS: usize = 16;

/// Discovery contradiction phase.
pub struct DiscoverContradictPhase {
    /// Cohesion delta threshold (0..=1). Pairs with `|a - b|`
    /// above this become candidates. Default 0.3.
    pub delta_threshold: f32,
}

impl Default for DiscoverContradictPhase {
    fn default() -> Self {
        Self { delta_threshold: 0.3 }
    }
}

/// Response schema for the LLM. The LLM is asked to return one
/// `(topic, description, severity)` triple per pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ContradictionRefinement {
    topic: String,
    description: String,
    severity: String,
}

impl DiscoverContradictPhase {
    /// Build the LLM user payload. The model receives the two
    /// cluster summaries and is asked to identify any disagreement.
    fn user_payload(a: &Cluster, b: &Cluster) -> String {
        format!(
            "Cluster A:\n  id: {a_id}\n  label: {a_label}\n  summary: {a_summary}\n  \
             members: {a_members}\n\n\
             Cluster B:\n  id: {b_id}\n  label: {b_label}\n  summary: {b_summary}\n  \
             members: {b_members}\n\n\
             Return a JSON object with three fields:\n\
             - \"topic\": a short topic label (e.g. \"consistency\", \"deployment\").\n\
             - \"description\": 1-2 sentences describing the disagreement.\n\
             - \"severity\": one of \"low\", \"medium\", \"high\".\n\n\
             If the clusters are not actually in tension, return severity = \"low\" \
             and description = \"no significant contradiction\".",
            a_id = a.id,
            a_label = a.label,
            a_summary = a.summary,
            a_members = a.members.join(", "),
            b_id = b.id,
            b_label = b.label,
            b_summary = b.summary,
            b_members = b.members.join(", "),
        )
    }
}

#[async_trait]
impl Phase for DiscoverContradictPhase {
    fn name(&self) -> &'static str {
        "discover_contradict"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let clusters_dir = ctx.run_dir().clusters();
        let contradictions_dir = ctx.run_dir().contradictions();
        std::fs::create_dir_all(&contradictions_dir)?;

        // Read every cluster.
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&clusters_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|s| s.to_str()) == Some("json")
                    && p.file_name().and_then(|s| s.to_str()) != Some("index.json")
            })
            .collect();
        paths.sort();

        let mut clusters: Vec<Cluster> = Vec::with_capacity(paths.len());
        for path in &paths {
            clusters.push(read_json(path)?);
        }

        if clusters.len() < 2 {
            // Nothing to compare.
            let path = contradictions_dir.join("contradictions.json");
            write_json(&path, &Vec::<Contradiction>::new())?;
            return Ok(PhaseOutput::Sketches(vec![path]));
        }

        // Compute pairwise distances from the cohesion score.
        let mut distances: Vec<(String, String, f32)> = Vec::new();
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let (a, b) = (&clusters[i], &clusters[j]);
                let delta = (a.cohesion - b.cohesion).abs();
                if delta >= self.delta_threshold {
                    distances.push((a.id.clone(), b.id.clone(), delta));
                }
            }
        }
        distances.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
        let top = top_pairs(&distances, MAX_PAIRS);

        // LLM pass per pair.
        let by_id: Arc<std::collections::HashMap<String, Cluster>> = Arc::new(
            clusters.iter().map(|c| (c.id.clone(), c.clone())).collect(),
        );

        let futures = top.iter().map(|(a_id, b_id, _delta)| {
            let a_id = a_id.clone();
            let b_id = b_id.clone();
            let by_id = Arc::clone(&by_id);
            let ctx = ctx.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let a = by_id.get(&a_id).cloned().unwrap_or_default();
                let b = by_id.get(&b_id).cloned().unwrap_or_default();
                let user = DiscoverContradictPhase::user_payload(&a, &b);
                let raw: ContradictionRefinement = ctx
                    .call_with_retry_parse(
                        crate::llm::Role::Tagger,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Tagger)
                            .to_owned(),
                        user,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Tagger),
                        3,
                    )
                    .await
                    .unwrap_or_default();
                Ok::<(String, String, ContradictionRefinement, Vec<String>), crate::error::Error>(
                    (
                        a_id,
                        b_id,
                        raw,
                        {
                            let mut v = a.members.clone();
                            v.extend(b.members.iter().cloned());
                            v
                        },
                    ),
                )
            }
        });
        let results = join_all(futures).await;

        let mut items: Vec<Contradiction> = Vec::new();
        for (idx, (a_id, b_id, raw, representatives)) in results.into_iter().flatten().enumerate() {
            let id = format!("c_{:02}", idx);
            let record = ContradictionRecord {
                cluster_a: a_id.clone(),
                cluster_b: b_id.clone(),
                representatives: representatives.clone(),
                topic: raw.topic,
                description: raw.description,
                severity: raw.severity,
            };
            items.push(Contradiction {
                id,
                cluster_a: record.cluster_a,
                cluster_b: record.cluster_b,
                representatives: record.representatives,
                topic: record.topic,
                description: record.description,
                severity: record.severity,
                schema_version: "v1".into(),
            });
        }

        // Sort by severity descending so the integrator picks the
        // high-severity entries first.
        items.sort_by_key(|c| std::cmp::Reverse(severity_rank(&c.severity)));

        let path = contradictions_dir.join("contradictions.json");
        write_json(&path, &items)?;

        // Run-id carried for the sidecar schema in case any
        // downstream tool wants to know which run produced this.
        let _ = RunId::default();
        Ok(PhaseOutput::Sketches(vec![path]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_payload_contains_cluster_ids() {
        let a = Cluster {
            id: "cluster_01".into(),
            label: "auth".into(),
            summary: "JWT-based".into(),
            members: vec!["sk_001".into()],
            ..Default::default()
        };
        let b = Cluster {
            id: "cluster_02".into(),
            label: "session".into(),
            summary: "Cookie-based".into(),
            members: vec!["sk_002".into()],
            ..Default::default()
        };
        let s = DiscoverContradictPhase::user_payload(&a, &b);
        assert!(s.contains("cluster_01"));
        assert!(s.contains("cluster_02"));
        assert!(s.contains("JWT-based"));
        assert!(s.contains("Cookie-based"));
    }

    #[test]
    fn refinement_round_trips() {
        let r = ContradictionRefinement {
            topic: "consistency".into(),
            description: "ACID vs eventual".into(),
            severity: "high".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: ContradictionRefinement = serde_json::from_str(&j).unwrap();
        assert_eq!(back.severity, "high");
    }
}
