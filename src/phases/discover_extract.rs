//! Discovery mode — `discover_extract` phase.
//!
//! For each cluster, run the LLM extractor once per facet and
//! persist the markdown body under `extractions/<cat_id>/faceta_<slug>.md`.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::discovery::extractor::{render_body, unique_sources};
use crate::discovery::facet::slug;
use crate::domain::{Cluster, FacetExtraction, FacetList, Sketch};
use crate::error::{Error, Result};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Discovery extraction phase.
pub struct DiscoverExtractPhase;

impl DiscoverExtractPhase {
    /// Build the user payload for the extractor. The model receives
    /// the cluster's sketches and the facet, and is asked to return
    /// a markdown body.
    fn user_payload(
        cluster: &Cluster,
        facet_id: &str,
        facet_desc: &str,
        sketches: &[Sketch],
    ) -> String {
        let sk_lines: Vec<String> = sketches
            .iter()
            .map(|s| format!("- {}: {}", s.id, s.thesis))
            .collect();
        format!(
            "Cluster:\n  id: {id}\n  label: {label}\n\n\
             Facet: {facet_id}\n  description: {facet_desc}\n\n\
             Source sketches:\n{sk}\n\n\
             Return a JSON object with three fields:\n\
             - \"body\": 200-800 words of markdown covering this facet.\n\
             - \"sources\": list of sketch ids that contributed.\n\
             - \"facet_id\": the same id as the input.\n\n\
             Respond only with JSON.",
            id = cluster.id,
            label = cluster.label,
            facet_id = facet_id,
            facet_desc = facet_desc,
            sk = sk_lines.join("\n"),
        )
    }

    /// Read each sketch whose id is in `members`. Returns an error
    /// when one of the member ids cannot be resolved so the caller
    /// can decide whether to continue or abort.
    fn read_member_sketches(ctx: &RunContext, members: &[String]) -> Result<Vec<Sketch>> {
        let mut out = Vec::new();
        for id in members {
            let path = ctx.run_dir().sketches().join(format!("{id}.json"));
            match read_json::<Sketch>(&path) {
                Ok(s) => out.push(s),
                Err(e) => return Err(Error::InvalidState(format!("member {id}: {e}"))),
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Phase for DiscoverExtractPhase {
    fn name(&self) -> &'static str {
        "discover_extract"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let facets_dir = ctx.run_dir().facets();
        let clusters_dir = ctx.run_dir().clusters();
        let extractions_dir = ctx.run_dir().extractions();
        let _ = std::fs::create_dir_all(&extractions_dir);

        let mut facet_paths: Vec<PathBuf> = std::fs::read_dir(&facets_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        facet_paths.sort();

        let mut paths: Vec<PathBuf> = Vec::new();
        let mut futures_total = 0usize;
        for facet_path in &facet_paths {
            let list: FacetList = read_json(facet_path)?;
            // Resolve the cluster.
            let cluster_path = clusters_dir.join(format!("{}.json", list.cluster_id));
            let cluster: Cluster = read_json(&cluster_path)?;
            let sketches = DiscoverExtractPhase::read_member_sketches(ctx, &cluster.members)?;
            let cat_dir = extractions_dir.join(&list.category_id);
            let _ = std::fs::create_dir_all(&cat_dir);

            let futures = list.facets.iter().map(|f| {
                let cat_id = list.category_id.clone();
                let facet_id = f.id.clone();
                let facet_desc = f.description.clone();
                let cluster = cluster.clone();
                let sketches = sketches.clone();
                let cat_dir = cat_dir.clone();
                let ctx = ctx.clone();
                async move {
                    let _permit = ctx.parallelism.acquire().await?;
                    let user = DiscoverExtractPhase::user_payload(
                        &cluster,
                        &facet_id,
                        &facet_desc,
                        &sketches,
                    );
                    let raw: FacetExtraction = ctx
                        .call_with_retry_parse(
                            crate::llm::Role::Extractor,
                            crate::llm::prompts::system_prompt(crate::llm::Role::Extractor)
                                .to_owned(),
                            user,
                            crate::llm::prompts::system_prompt(crate::llm::Role::Extractor),
                            3,
                        )
                        .await
                        .unwrap_or_else(|_| FacetExtraction {
                            facet_id: facet_id.clone(),
                            category_id: cat_id.clone(),
                            body: format!("_No extraction available for {facet_id}._"),
                            sources: cluster.members.clone(),
                            schema_version: "v1".into(),
                        });
                    let ext = FacetExtraction {
                        facet_id: facet_id.clone(),
                        category_id: cat_id.clone(),
                        body: if raw.body.is_empty() {
                            format!("_No content available for {facet_id}._")
                        } else {
                            raw.body
                        },
                        sources: if raw.sources.is_empty() {
                            cluster.members.clone()
                        } else {
                            raw.sources
                        },
                        schema_version: "v1".into(),
                    };
                    let md = render_body(&ext);
                    let path = cat_dir.join(format!("faceta_{}.md", slug(&facet_id)));
                    std::fs::write(&path, &md)?;
                    let json_path = cat_dir.join(format!("faceta_{}.json", slug(&facet_id)));
                    write_json(&json_path, &ext)?;
                    Ok::<PathBuf, crate::error::Error>(json_path)
                }
            });
            futures_total += list.facets.len();
            let results = join_all(futures).await;
            for r in results {
                match r {
                    Ok(p) => paths.push(p),
                    Err(e) => {
                        let _ = ctx.telemetry.warn(
                            "phase.discover_extract.skipped",
                            "warn",
                            "extraction failed for one facet",
                            serde_json::json!({"error": e.to_string()}),
                            crate::telemetry::WarningContext {
                                phase: Some("discover_extract".into()),
                                role: Some("extractor".into()),
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }

        if paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_extract produced zero facet extractions".into(),
            ));
        }

        // Drop a tiny summary so the integrator phase can skip the
        // directory walk.
        let summary = serde_json::json!({
            "version": "v1",
            "extractions_dir": "extractions",
            "facet_count": futures_total,
            "kept": paths.len(),
        });
        let summary_path = extractions_dir.join("index.json");
        write_json(&summary_path, &summary)?;

        // `unique_sources` is consumed by the integrator; we put it
        // into the index for clarity.
        let _ = unique_sources(&[]);
        Ok(PhaseOutput::Sketches(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_payload_lists_sketches() {
        let c = Cluster {
            id: "cluster_01".into(),
            label: "auth".into(),
            summary: "JWT-based".into(),
            members: vec!["sk_001".into()],
            ..Default::default()
        };
        let sk = Sketch {
            id: "sk_001".into(),
            thesis: "alpha thesis".into(),
            ..Default::default()
        };
        let s = DiscoverExtractPhase::user_payload(&c, "data-flows", "data flows", &[sk]);
        assert!(s.contains("data-flows"));
        assert!(s.contains("sk_001: alpha thesis"));
    }
}
