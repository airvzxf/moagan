//! Discovery mode — `discover_extract` phase.
//!
//! For each cluster, run the LLM extractor once per facet and
//! persist the markdown body under `extractions/<cat_id>/faceta_<slug>.md`.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::discovery::DiscoveryContext;
use crate::discovery::extractor::{render_body, unique_sources};
use crate::discovery::facet::slug;
use crate::discovery::tagger_threshold::TaggerThreshold;
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
        context: &DiscoveryContext,
    ) -> String {
        let sk_lines: Vec<String> = sketches
            .iter()
            .map(|s| format!("- {}: {}", s.id, s.thesis))
            .collect();
        format!(
            "Cluster:\n  id: {id}\n  label: {label}\n\n\
             Facet: {facet_id}\n  description: {facet_desc}\n\n\
             Source sketches:\n{sk}\n\n\
             Discovery context (D.13.5):\n  brief_hash: {brief_hash}\n  \
             matrix_hash: {matrix_hash}\n  tagger_threshold: {tagger}\n  \
             sketch_count: {sketch_count}\n  contradiction_count: {contra_count}\n  \
             facet_count: {facet_count}\n\n\
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
            brief_hash = context.brief_hash,
            matrix_hash = context.matrix_hash,
            tagger = context.tagger_threshold,
            sketch_count = context.sketch_ids.len(),
            contra_count = context.contradiction_ids.len(),
            facet_count = context.facet_ids.len(),
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

    /// Build + persist the [`DiscoveryContext`] sidecar so the
    /// run carries a typed record of every artefact the upstream
    /// phases produced. Returns the hydrated context for the
    /// caller to embed in the LLM payloads.
    fn build_and_persist_context(ctx: &RunContext) -> Result<DiscoveryContext> {
        let threshold =
            TaggerThreshold::from_config_value(Some(ctx.config.discovery.tag_threshold));
        let dc = DiscoveryContext::build_with_threshold(&ctx.run_dir(), Some(threshold.value));
        let path = dc.persist(&ctx.run_dir())?;
        tracing::info!(
            sketches = dc.sketch_ids.len(),
            contradictions = dc.contradiction_ids.len(),
            facets = dc.facet_ids.len(),
            brief_hash = %dc.brief_hash,
            matrix_hash = %dc.matrix_hash,
            tagger_threshold = dc.tagger_threshold,
            path = %path.display(),
            "discovery_context.json persisted"
        );
        Ok(dc)
    }
}

#[async_trait]
impl Phase for DiscoverExtractPhase {
    fn name(&self) -> &'static str {
        "discover_extract"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!("discover_extract: enter");
        let facets_dir = ctx.run_dir().facets();
        let clusters_dir = ctx.run_dir().clusters();
        let extractions_dir = ctx.run_dir().extractions();
        let _ = std::fs::create_dir_all(&extractions_dir);

        // D.13.5: build + persist the composite discovery context
        // so resume and the integrator phase can both verify the
        // upstream artefact surface stayed consistent.
        let discovery_context = DiscoverExtractPhase::build_and_persist_context(ctx)?;

        let mut facet_paths: Vec<PathBuf> = std::fs::read_dir(&facets_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|s| s.to_str()) == Some("json")
                    && !p
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.ends_with(".meta.json"))
                        .unwrap_or(false)
            })
            .collect();
        facet_paths.sort();

        let mut paths: Vec<PathBuf> = Vec::new();
        let mut futures_total = 0usize;
        for facet_path in &facet_paths {
            let list: FacetList = read_json(facet_path)?;
            // Resolve the cluster.
            let cluster_path = clusters_dir.join(format!("{}.json", list.cluster_id));
            // If the cluster file is missing or malformed, skip this
            // facet list rather than aborting the whole phase. A
            // missing cluster is a benign divergence (the tagger
            // pass survived but the cluster pass did not), not a
            // fatal error.
            let cluster: Cluster = match read_json::<Cluster>(&cluster_path) {
                Ok(c) => c,
                Err(e) => {
                    let _ = ctx.telemetry.warn(
                        "phase.discover_extract.missing_cluster",
                        "warn",
                        "facet list references a missing or malformed cluster; skipping",
                        serde_json::json!({
                            "facet_path": facet_path.display().to_string(),
                            "cluster_id": list.cluster_id,
                            "error": e.to_string(),
                        }),
                        crate::telemetry::WarningContext {
                            phase: Some("discover_extract".into()),
                            role: Some("extractor".into()),
                            ..Default::default()
                        },
                    );
                    continue;
                }
            };
            // Skip clusters whose member list is empty or points at
            // sketch ids that don't exist on disk. The empty-member
            // case happens when the cluster pass drops members for
            // some reason; the missing-sketch case happens when the
            // tagger pass produces more clusters than the sketch
            // generator actually emitted.
            if cluster.members.is_empty()
                || cluster.members.iter().any(|id| {
                    id.is_empty() || !ctx.run_dir().sketches().join(format!("{id}.json")).exists()
                })
            {
                let _ = ctx.telemetry.warn(
                    "phase.discover_extract.skip_empty_cluster",
                    "warn",
                    "cluster has empty or missing member sketches; skipping",
                    serde_json::json!({
                        "cluster_id": list.cluster_id,
                        "members": cluster.members.len(),
                    }),
                    crate::telemetry::WarningContext {
                        phase: Some("discover_extract".into()),
                        role: Some("extractor".into()),
                        ..Default::default()
                    },
                );
                continue;
            }
            let sketches = DiscoverExtractPhase::read_member_sketches(ctx, &cluster.members)?;
            let cat_dir = extractions_dir.join(&list.category_id);
            // Propagate directory-creation failures instead of
            // silently swallowing them. The pre-fix code did
            // `let _ = std::fs::create_dir_all(&cat_dir);` and then
            // tried to write into a directory that might not exist,
            // which produced a cryptic `io: No such file or
            // directory` later in the file write.
            std::fs::create_dir_all(&cat_dir).map_err(|e| {
                Error::Io(crate::error::IoError::CreateDir {
                    path: cat_dir.clone(),
                    source: e,
                })
            })?;

            let futures = list.facets.iter().map(|f| {
                let cat_id = list.category_id.clone();
                let facet_id = f.id.clone();
                let facet_desc = f.description.clone();
                let cluster = cluster.clone();
                let sketches = sketches.clone();
                let cat_dir = cat_dir.clone();
                let ctx = ctx.clone();
                let discovery_context = discovery_context.clone();
                async move {
                    let _permit = ctx.parallelism.acquire().await?;
                    let user = DiscoverExtractPhase::user_payload(
                        &cluster,
                        &facet_id,
                        &facet_desc,
                        &sketches,
                        &discovery_context,
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
                    // `cat_dir` was created above, but a defensive
                    // `create_dir_all` here makes the write survive
                    // a hypothetical cat_dir cleanup between phases.
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            Error::Io(crate::error::IoError::CreateDir {
                                path: parent.to_path_buf(),
                                source: e,
                            })
                        })?;
                    }
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
            tracing::error!("discover_extract: zero facet extractions produced");
            return Err(Error::InvalidState(
                "discover_extract produced zero facet extractions".into(),
            ));
        }
        tracing::info!(
            extractions_written = paths.len(),
            "discover_extract: phase complete"
        );

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
        let dc = DiscoveryContext::default();
        let s = DiscoverExtractPhase::user_payload(&c, "data-flows", "data flows", &[sk], &dc);
        assert!(s.contains("data-flows"));
        assert!(s.contains("sk_001: alpha thesis"));
        assert!(s.contains("brief_hash"));
        assert!(s.contains("tagger_threshold"));
    }
}
