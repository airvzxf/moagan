//! Discovery mode — `discover_integrate` phase.
//!
//! For each cluster, the LLM integrator joins the per-facet
//! extractions into a coherent category document. Falls back to the
//! local `integator::local_join` when the LLM call fails.
//!
//! Output: `final/cat_NN.md` (one per category) plus a
//! `final/cat_index.json` used by the optional summary phase.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::discovery::DiscoveryContext;
use crate::discovery::extractor::join_markdown;
use crate::discovery::integrator::{build_doc, local_join, meets_safeguards};
use crate::discovery::tagger_threshold::TaggerThreshold;
use crate::domain::{CategoryDoc, Cluster, FacetExtraction, FacetList};
use crate::error::{Error, Result};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Discovery integration phase.
pub struct DiscoverIntegratePhase;

impl DiscoverIntegratePhase {
    /// Build the LLM user payload. The integrator receives the
    /// per-facet markdown and the cluster summary, and is asked to
    /// return a `CategoryDoc` JSON object.
    fn user_payload(label: &str, joined: &str, context: &DiscoveryContext) -> String {
        format!(
            "Cluster label: {label}\n\n\
             Per-facet extractions (already joined in display order):\n\n\
             {joined}\n\n\
             Discovery context (D.13.5):\n  brief_hash: {brief_hash}\n  \
             matrix_hash: {matrix_hash}\n  tagger_threshold: {tagger}\n  \
             sketch_count: {sketch_count}\n  contradiction_count: {contra_count}\n  \
             facet_count: {facet_count}\n\n\
             Return a JSON object with one field:\n\
             - \"body\": the integrated markdown document.\n\n\
             Preserve every citation from the per-facet extracts. \
             Do not invent new content.\n\n\
             Respond only with JSON.",
            label = label,
            joined = joined,
            brief_hash = context.brief_hash,
            matrix_hash = context.matrix_hash,
            tagger = context.tagger_threshold,
            sketch_count = context.sketch_ids.len(),
            contra_count = context.contradiction_ids.len(),
            facet_count = context.facet_ids.len(),
        )
    }

    /// Read every facet extraction for `category_id` from disk.
    fn load_extractions(ctx: &RunContext, category_id: &str) -> Result<Vec<FacetExtraction>> {
        let dir = ctx.run_dir().extractions().join(category_id);
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|s| s.to_str()) == Some("json")
                    && p.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.starts_with("faceta_"))
                        .unwrap_or(false)
            })
            .collect();
        paths.sort();
        let mut out = Vec::new();
        for p in paths {
            out.push(read_json(&p)?);
        }
        Ok(out)
    }

    /// Build + persist the [`DiscoveryContext`] sidecar. The
    /// integrator phase runs after `discover_extract`, so by the
    /// time it executes the context sidecar already exists; we
    /// re-emit it so a resume that landed directly on the
    /// integrator (skipping the extract phase) still carries a
    /// fresh context record.
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
            "discovery_context.json persisted from integrator phase"
        );
        Ok(dc)
    }
}

#[async_trait]
impl Phase for DiscoverIntegratePhase {
    fn name(&self) -> &'static str {
        "discover_integrate"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!("discover_integrate: enter");
        let facets_dir = ctx.run_dir().facets();
        let clusters_dir = ctx.run_dir().clusters();
        let final_dir = ctx.run_dir().final_dir();
        let _ = std::fs::create_dir_all(&final_dir);

        // D.13.5: build + persist the composite discovery context
        // so resume and the summary phase can verify the upstream
        // artefact surface stayed consistent.
        let discovery_context = DiscoverIntegratePhase::build_and_persist_context(ctx)?;

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

        if facet_paths.is_empty() {
            tracing::error!("discover_integrate: zero facet lists found");
            return Err(Error::InvalidState(
                "discover_integrate found zero facet lists".into(),
            ));
        }
        tracing::debug!(
            facet_list_count = facet_paths.len(),
            "discover_integrate: facet lists enumerated"
        );

        // Compute max cluster members so density is consistent.
        let max_members: usize = {
            let mut m = 0usize;
            for entry in std::fs::read_dir(&clusters_dir)?.filter_map(|r| r.ok()) {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json")
                    && path.file_name().and_then(|s| s.to_str()) != Some("index.json")
                {
                    let c: Cluster = read_json(&path)?;
                    if c.members.len() > m {
                        m = c.members.len();
                    }
                }
            }
            m.max(1)
        };

        let mut paths: Vec<PathBuf> = Vec::new();
        let futures = facet_paths.iter().map(|facet_path| {
            let facet_path = facet_path.clone();
            let ctx = ctx.clone();
            let discovery_context = discovery_context.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let list: FacetList = read_json(&facet_path)?;
                // Skip facet lists whose category has no
                // extractions — happens when `discover_extract`
                // skipped the corresponding cluster (empty / missing
                // member sketches). The integrator would otherwise
                // bubble up an `io: No such file or directory` on
                // its `read_dir(extractions/<cat_id>)` call.
                let extractions_dir = ctx.run_dir().extractions().join(&list.category_id);
                if !extractions_dir.exists() {
                    let _ = ctx.telemetry.warn(
                        "phase.discover_integrate.skip_empty_category",
                        "warn",
                        "category has no extractions; skipping integration",
                        serde_json::json!({
                            "category_id": list.category_id,
                            "extractions_dir": extractions_dir.display().to_string(),
                        }),
                        crate::telemetry::WarningContext {
                            phase: Some("discover_integrate".into()),
                            role: Some("integrator".into()),
                            ..Default::default()
                        },
                    );
                    // Sentinel path under a directory that the
                    // phase never creates. The post-loop filter
                    // uses `path.exists()` to discard it.
                    return Ok::<PathBuf, crate::error::Error>(
                        ctx.run_dir().final_dir().join("_SKIPPED/skip.md"),
                    );
                }
                let cluster_path = ctx
                    .run_dir()
                    .clusters()
                    .join(format!("{}.json", list.cluster_id));
                let cluster: Cluster = if cluster_path.exists() {
                    read_json(&cluster_path).unwrap_or_default()
                } else {
                    Cluster::default()
                };
                let extractions =
                    DiscoverIntegratePhase::load_extractions(&ctx, &list.category_id)?;
                let joined = join_markdown(&extractions);
                let user = DiscoverIntegratePhase::user_payload(
                    &cluster.label,
                    &joined,
                    &discovery_context,
                );
                let raw: Result<CategoryDoc> = ctx
                    .call_with_retry_parse(
                        crate::llm::Role::Integrator,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Integrator).to_owned(),
                        user,
                        crate::llm::prompts::system_prompt(crate::llm::Role::Integrator),
                        3,
                    )
                    .await;
                let md = match raw {
                    Ok(mut raw) => {
                        if raw.body.is_empty() {
                            raw.body = local_join(&list.category_id, &cluster.label, &extractions);
                        }
                        // Catalog decision 42 + V4 §6.10: the
                        // integrator must not dilute the content. If
                        // the LLM-joined body fails the coverage or
                        // citation safeguard, revert to the local
                        // join. The safeguard verdict is reported as
                        // a structured warning so the integrator run
                        // is auditable.
                        let local_body = local_join(&list.category_id, &cluster.label, &extractions);
                        let body = match meets_safeguards(&local_body, &raw.body) {
                            Ok(()) => raw.body.clone(),
                            Err(verdict) => {
                                let _ = ctx.telemetry.warn(
                                    "phase.discover_integrate.safeguard_revert",
                                    "warn",
                                    "LLM integrator failed the content-dilution safeguard; reverting to local_join",
                                    serde_json::json!({
                                        "category_id": list.category_id,
                                        "cluster_id": cluster.id,
                                        "verdict": verdict,
                                    }),
                                    crate::telemetry::WarningContext {
                                        phase: Some("discover_integrate".into()),
                                        role: Some("integrator".into()),
                                        ..Default::default()
                                    },
                                );
                                local_body
                            }
                        };
                        let sources = if raw.sources.is_empty() {
                            cluster.members.clone()
                        } else {
                            raw.sources
                        };
                        build_doc(
                            &list.category_id,
                            &cluster.id,
                            cluster.members.len(),
                            max_members,
                            sources,
                            body,
                        )
                    }
                    Err(_) => {
                        let body = local_join(&list.category_id, &cluster.label, &extractions);
                        build_doc(
                            &list.category_id,
                            &cluster.id,
                            cluster.members.len(),
                            max_members,
                            cluster.members.clone(),
                            body,
                        )
                    }
                };
                let md_path = ctx
                    .run_dir()
                    .final_dir()
                    .join(format!("{}.md", md.category_id));
                std::fs::write(&md_path, &md.body)?;
                let json_path = ctx
                    .run_dir()
                    .final_dir()
                    .join(format!("{}.json", md.category_id));
                write_json(&json_path, &md)?;
                Ok::<PathBuf, crate::error::Error>(md_path)
            }
        });
        let results = join_all(futures).await;
        for r in results {
            match r {
                Ok(p) if p.exists() => paths.push(p),
                Ok(_) => { /* skipped: integration did not produce artifacts */ }
                Err(e) => {
                    let _ = ctx.telemetry.warn(
                        "phase.discover_integrate.skipped",
                        "warn",
                        "integration failed for one category",
                        serde_json::json!({"error": e.to_string()}),
                        crate::telemetry::WarningContext {
                            phase: Some("discover_integrate".into()),
                            role: Some("integrator".into()),
                            ..Default::default()
                        },
                    );
                }
            }
        }

        if paths.is_empty() {
            tracing::error!("discover_integrate: zero category docs produced");
            return Err(Error::InvalidState(
                "discover_integrate produced zero category docs".into(),
            ));
        }
        tracing::info!(
            category_docs = paths.len(),
            "discover_integrate: phase complete"
        );

        // Index for the summary phase.
        let mut entries: Vec<serde_json::Value> = Vec::new();
        let mut densities: BTreeMap<String, f32> = BTreeMap::new();
        for path in &paths {
            let json_path = path.with_extension("json");
            let doc: CategoryDoc = read_json(&json_path)?;
            entries.push(serde_json::json!({
                "category_id": doc.category_id,
                "cluster_id": doc.cluster_id,
                "density": doc.density,
                "sources": doc.sources,
            }));
            densities.insert(doc.category_id.clone(), doc.density);
        }
        let index = serde_json::json!({
            "version": "v1",
            "final_dir": "final",
            "categories": entries,
        });
        let index_path = final_dir.join("cat_index.json");
        write_json(&index_path, &index)?;

        // Surface the densities for the summary phase.
        let _ = densities;

        Ok(PhaseOutput::Sketches(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::facet::slug;

    #[test]
    fn user_payload_contains_label() {
        let dc = DiscoveryContext::default();
        let s = DiscoverIntegratePhase::user_payload("auth", "## body", &dc);
        assert!(s.contains("auth"));
        assert!(s.contains("## body"));
        assert!(s.contains("brief_hash"));
        assert!(s.contains("tagger_threshold"));
    }

    #[test]
    fn load_extractions_handles_missing_dir() {
        crate::test_support::with_moagan_home(
            "discover_integrate_load_extractions_missing_dir",
            |_home| {
                let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
                let ctx = test_ctx(home.clone(), crate::ids::RunId::new());
                let r = DiscoverIntegratePhase::load_extractions(&ctx, "cat_99");
                assert!(r.is_err());
            },
        );
    }

    fn test_ctx(
        home: std::sync::Arc<crate::fs_layout::MoaganHome>,
        run_id: crate::ids::RunId,
    ) -> crate::phases::RunContext {
        let registry = std::sync::Arc::new(crate::llm::ProviderRegistry::default());
        crate::phases::RunContext::new(
            run_id,
            home,
            registry,
            "mock".into(),
            "mock-model".into(),
            crate::execution::Parallelism::new(1),
            crate::telemetry::Telemetry::noop(),
            String::new(),
            "discover".into(),
        )
    }

    #[test]
    fn slug_uses_kebab_case() {
        assert_eq!(slug("Data Flows"), "data-flows");
    }
}
