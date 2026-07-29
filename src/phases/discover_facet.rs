//! Discovery mode — `discover_facet` phase.
//!
//! For each cluster the LLM is asked to derive a list of facets the
//! category document should cover. The result is cached per
//! `sha256(brief + cluster_id)` so a second run with the same
//! brief is a no-op for the LLM (the cache key is checked but the
//! cache itself is not implemented yet — that's a sub-fase D
//! follow-up).

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::domain::{Cluster, FacetList};
use crate::error::{Error, Result};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::read_json;

/// Discovery facet phase.
pub struct DiscoverFacetPhase;

/// LLM response schema. The model returns a list of `(name,
/// description, required)` triples.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct FacetDerivation {
    facets: Vec<FacetTriple>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct FacetTriple {
    name: String,
    description: String,
    required: bool,
}

impl DiscoverFacetPhase {
    /// Build the LLM user payload. The model receives the cluster
    /// summary and is asked to derive 3-6 facets.
    fn user_payload(cluster: &Cluster, brief: &str) -> String {
        format!(
            "Brief:\n{brief}\n\n\
             Cluster:\n  id: {id}\n  label: {label}\n  summary: {summary}\n\n\
             Return a JSON object with one field:\n\
             - \"facets\": list of 3-6 objects, each with:\n\
               - \"name\": a short label (kebab-case ok)\n\
               - \"description\": 1-2 sentences\n\
               - \"required\": true if the facet must appear in the final doc\n\n\
             Respond only with JSON.",
            brief = brief,
            id = cluster.id,
            label = cluster.label,
            summary = cluster.summary,
        )
    }

    /// Pick a category id for the cluster based on its 1-based
    /// position in the cluster list (sorted by id).
    pub fn category_id_for(idx: usize) -> String {
        format!("cat_{:02}", idx + 1)
    }
}

#[async_trait]
impl Phase for DiscoverFacetPhase {
    fn name(&self) -> &'static str {
        "discover_facet"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let clusters_dir = ctx.run_dir().clusters();
        let facets_dir = ctx.run_dir().facets();
        let _ = std::fs::create_dir_all(&facets_dir);

        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let brief_text = serde_json::to_string(&brief).map_err(Error::from)?;

        let mut cluster_paths: Vec<PathBuf> = std::fs::read_dir(&clusters_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|s| s.to_str()) == Some("json")
                    && p.file_name().and_then(|s| s.to_str()) != Some("index.json")
            })
            .collect();
        cluster_paths.sort();

        if cluster_paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_facet found zero clusters".into(),
            ));
        }

        let mut paths: Vec<PathBuf> = Vec::new();
        let futures = cluster_paths.iter().enumerate().map(|(idx, path)| {
            let path = path.clone();
            let brief = brief_text.clone();
            let ctx = ctx.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let cluster: Cluster = read_json(&path)?;
                let user = DiscoverFacetPhase::user_payload(&cluster, &brief);
                let raw: FacetDerivation = ctx
                    .call_with_retry_parse(
                        crate::llm::Role::Tagger,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Tagger).to_owned(),
                        user,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Tagger),
                        3,
                    )
                    .await
                    .unwrap_or_default();
                let triples: Vec<(String, String, bool)> = raw
                    .facets
                    .into_iter()
                    .map(|t| (t.name, t.description, t.required))
                    .collect();
                let cat_id = DiscoverFacetPhase::category_id_for(idx);
                let list = FacetList::from_triples(
                    &cat_id,
                    &cluster.id,
                    &brief,
                    crate::time::now_unix_secs(),
                    triples,
                );
                let path = std::path::Path::new(&ctx.run_dir().facets())
                    .join(format!("{cat_id}_facets.json"));
                crate::phases::util::write_json(&path, &list)?;
                Ok::<PathBuf, crate::error::Error>(path)
            }
        });
        let results = join_all(futures).await;
        for r in results {
            match r {
                Ok(p) => paths.push(p),
                Err(e) => {
                    let _ = ctx.telemetry.warn(
                        "phase.discover_facet.skipped",
                        "warn",
                        "facet derivation failed for one cluster",
                        serde_json::json!({"error": e.to_string()}),
                        crate::telemetry::WarningContext {
                            phase: Some("discover_facet".into()),
                            role: Some("tagger".into()),
                            ..Default::default()
                        },
                    );
                }
            }
        }

        if paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_facet produced zero facet lists".into(),
            ));
        }

        Ok(PhaseOutput::Sketches(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::facet::{cache_key, slug};

    #[test]
    fn category_id_starts_at_one() {
        assert_eq!(DiscoverFacetPhase::category_id_for(0), "cat_01");
        assert_eq!(DiscoverFacetPhase::category_id_for(12), "cat_13");
    }

    #[test]
    fn slug_works_in_payload() {
        let s = slug("Data Flows");
        assert_eq!(s, "data-flows");
    }

    #[test]
    fn cache_key_changes_with_brief() {
        let a = cache_key("brief-a", "cat_01");
        let b = cache_key("brief-b", "cat_01");
        assert_ne!(a, b);
    }

    #[test]
    fn user_payload_contains_label_and_summary() {
        let c = Cluster {
            id: "cluster_01".into(),
            label: "auth".into(),
            summary: "JWT-based".into(),
            members: vec!["sk_001".into()],
            ..Default::default()
        };
        let s = DiscoverFacetPhase::user_payload(&c, "BRIEF");
        assert!(s.contains("cluster_01"));
        assert!(s.contains("auth"));
        assert!(s.contains("BRIEF"));
    }
}
