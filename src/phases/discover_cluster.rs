//! Discovery mode — `discover_cluster` phase.
//!
//! Per V4 §6.6 and proposal-02-rust.md §9.5, the clustering step is
//! a two-pass process:
//!
//! 1. **SimHash pass** (`src/discovery/clusterer.rs::cluster`):
//!    fingerprints each sketch (thesis + key_decisions + outline) and
//!    groups ones with Jaccard distance `<= threshold` together.
//!    Cheap, deterministic, scales to 500 sketches without LLM calls.
//!
//! 2. **LLM refinement pass** (this phase): for each cluster, ask
//!    the LLM for a short `label` and `summary`. The LLM is called
//!    with the cluster's centroid text (the longest member) and the
//!    member ids. The result is a `Cluster` that the integrator
//!    phase consumes.
//!
//! Output:
//! - `clusters/cluster_NN.json` — one per cluster.
//! - `clusters/index.json` — tally of `(cluster_id, member_count,
//   label)`.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::discovery::clusterer::{
    bucket_by_cluster, cluster, cluster_id_for, cohesion, member_ids, SketchRecord,
};
use crate::domain::{Cluster, Sketch};
use crate::error::{Error, Result};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Response schema for the LLM refinement. The LLM is asked to
/// return a (label, summary) pair per cluster. Both fields are
/// optional so the model can return just one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ClusterRefinement {
    /// Short label (e.g. "auth strategies").
    label: String,
    /// Short summary (1-2 sentences).
    summary: String,
}

/// Discovery cluster phase. Reads every sketch, runs the SimHash
/// pass, then runs the LLM refinement pass per cluster.
pub struct DiscoverClusterPhase {
    /// SimHash threshold. Default 0.7.
    pub threshold: f32,
}

impl DiscoverClusterPhase {
    /// Build the LLM user payload. The model receives the cluster
    /// member ids and the centroid text, and is asked to return a
    /// `ClusterRefinement` JSON object.
    fn user_payload(member_ids: &[String], centroid: &str) -> String {
        let ids = member_ids.join(", ");
        format!(
            "Cluster members: {ids}\n\n\
             Centroid text:\n{centroid}\n\n\
             Return a JSON object with two fields:\n\
             - \"label\": a short label for the cluster (1-30 chars).\n\
             - \"summary\": 1-2 sentences describing the cluster.\n\n\
             Respond only with JSON."
        )
    }

    /// Pick the member with the longest text as the cluster centroid.
    fn centroid(records: &[SketchRecord], chunk_member_indices: &[usize]) -> String {
        let mut best = 0usize;
        let mut best_len = 0;
        for (i, idx) in chunk_member_indices.iter().enumerate() {
            let l = records[*idx].text.len();
            if l > best_len {
                best_len = l;
                best = i;
            }
        }
        let idx = chunk_member_indices[best];
        records[idx].text.clone()
    }
}

impl Default for DiscoverClusterPhase {
    fn default() -> Self {
        Self { threshold: 0.7 }
    }
}

#[async_trait]
impl Phase for DiscoverClusterPhase {
    fn name(&self) -> &'static str {
        "discover_cluster"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let sketches_dir = ctx.run_dir().sketches();
        let clusters_dir = ctx.run_dir().clusters();
        std::fs::create_dir_all(&clusters_dir)?;

        // Read every sketch.
        let mut sketch_paths: Vec<PathBuf> = std::fs::read_dir(&sketches_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        sketch_paths.sort();

        let mut records: Vec<SketchRecord> = Vec::with_capacity(sketch_paths.len());
        for path in &sketch_paths {
            let sk: Sketch = read_json(path)?;
            let mut text = sk.thesis.clone();
            text.push('\n');
            text.push_str(&sk.key_decisions.join("; "));
            text.push('\n');
            text.push_str(&sk.architecture_outline);
            records.push(SketchRecord { id: sk.id.clone(), text });
        }

        if records.is_empty() {
            return Err(Error::InvalidState(
                "discover_cluster found zero sketches".into(),
            ));
        }

        // 1. SimHash pass.
        let chunks = cluster(&records, self.threshold);
        let buckets = bucket_by_cluster(&records, &chunks);

        // 2. LLM refinement pass — fan-out per cluster, parallel.
        let refinement_inputs: Vec<(String, Vec<String>, String)> = chunks
            .iter()
            .enumerate()
            .map(|(idx, c)| {
                let id = cluster_id_for(idx);
                let ids = member_ids(&records, c);
                let centroid = Self::centroid(&records, &c.member_indices);
                (id, ids, centroid)
            })
            .collect();

        let futures = refinement_inputs.iter().map(|(id, ids, centroid)| {
            let id = id.clone();
            let ids = ids.clone();
            let centroid = centroid.clone();
            let ctx = ctx.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let user = DiscoverClusterPhase::user_payload(&ids, &centroid);
                let raw: ClusterRefinement = ctx
                    .call_with_retry_parse(
                        crate::llm::Role::Tagger,
                        // The LLM refinement pass uses the tagger
                        // system prompt (T=0.0, top_p=0.2) so the
                        // label is deterministic. The integrator
                        // has its own prompt for prose.
                        crate::llm::prompts::system_prompt(crate::llm::Role::Tagger)
                            .to_owned(),
                        user,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Tagger),
                        3,
                    )
                    .await
                    .unwrap_or_default();
                Ok::<(String, ClusterRefinement), crate::error::Error>((id, raw))
            }
        });
        let results = join_all(futures).await;

        let mut refinements: std::collections::BTreeMap<String, ClusterRefinement> =
            std::collections::BTreeMap::new();
        for (id, raw) in results.into_iter().flatten() {
            refinements.insert(id, raw);
        }

        // 3. Persist one cluster JSON per cluster.
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut index_entries: Vec<serde_json::Value> = Vec::new();
        for (idx, chunk) in chunks.iter().enumerate() {
            let id = cluster_id_for(idx);
            let cohesion_score = cohesion(&records, chunk);
            let refinement = refinements.get(&id).cloned().unwrap_or_default();
            let cluster = Cluster {
                id: id.clone(),
                label: refinement.label,
                summary: refinement.summary,
                category_id: String::new(),
                members: buckets.get(&id).cloned().unwrap_or_default(),
                centroid_simhash: String::new(),
                cohesion: cohesion_score,
                schema_version: "v1".into(),
            };
            let path = clusters_dir.join(format!("{id}.json"));
            write_json(&path, &cluster)?;
            paths.push(path);
            index_entries.push(serde_json::json!({
                "cluster_id": id,
                "label": cluster.label,
                "summary": cluster.summary,
                "members": cluster.members,
                "cohesion": cluster.cohesion,
            }));
        }

        if paths.is_empty() {
            return Err(Error::InvalidState("discover_cluster produced zero clusters".into()));
        }

        let index = serde_json::json!({
            "version": "v1",
            "clusters_dir": "clusters",
            "threshold": self.threshold,
            "cluster_count": paths.len(),
            "entries": index_entries,
        });
        let index_path = clusters_dir.join("index.json");
        write_json(&index_path, &index)?;

        // Surface the cluster paths as the hand-off to the next
        // phase. The `PhaseOutput::Sketches` variant is reused
        // because it is just `Vec<PathBuf>` — the next phase
        // (`discover_contradict`) reads the index not the paths.
        Ok(PhaseOutput::Sketches(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_payload_contains_member_ids() {
        let ids = vec!["sk_001".to_string(), "sk_002".to_string()];
        let s = DiscoverClusterPhase::user_payload(&ids, "centroid");
        assert!(s.contains("sk_001, sk_002"));
        assert!(s.contains("centroid"));
    }

    #[test]
    fn centroid_picks_longest_text() {
        let records = vec![
            SketchRecord { id: "sk_001".into(), text: "short".into() },
            SketchRecord { id: "sk_002".into(), text: "this is a much longer text".into() },
            SketchRecord { id: "sk_003".into(), text: "mid".into() },
        ];
        let c = DiscoverClusterPhase::centroid(&records, &[0, 1, 2]);
        assert!(c.contains("longer"));
    }

    #[test]
    fn refinement_round_trips() {
        let r = ClusterRefinement {
            label: "auth".into(),
            summary: "JWT-based".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: ClusterRefinement = serde_json::from_str(&j).unwrap();
        assert_eq!(back.label, "auth");
    }
}
