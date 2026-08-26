//! Discovery mode — `discover_facet` phase.
//!
//! For each cluster the LLM is asked to derive a list of facets the
//! category document should cover. When the cross-run facet cache
//! is enabled (opt-in via the `--cache-facets` CLI flag), the
//! result is persisted to the cross-run facet cache keyed by
//! `sha256(brief + category_id)` (V4 §6.8 + catalog decision
//! D.13.13) so a second run with the same brief and category id
//! is a no-op for the LLM. The TTL is `DEFAULT_TTL_SECS` (7 days)
//! and is configurable via `MOAGAN_FACET_CACHE_TTL_SECS`.

use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::discovery::facet_cache::{DEFAULT_TTL_SECS, FacetCache};
use crate::domain::{Cluster, FacetList};
use crate::error::{Error, Result};
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::read_json;

/// Resolve the facet cache TTL at construction time. The env-var
/// override (`MOAGAN_FACET_CACHE_TTL_SECS=0` disables TTL;
/// `MOAGAN_FACET_CACHE_TTL_SECS=N` sets a custom TTL).
fn facet_cache_ttl_secs() -> Option<u64> {
    match std::env::var("MOAGAN_FACET_CACHE_TTL_SECS") {
        Ok(v) if v.trim() == "0" => None,
        Ok(v) => v.parse::<u64>().ok().filter(|n| *n > 0),
        Err(_) => Some(DEFAULT_TTL_SECS),
    }
}

/// Discovery facet phase.
///
/// `cache_enabled` controls whether the cross-run facet cache is
/// used. The default is `false` so the operator must opt in via
/// the `--cache-facets` CLI flag; this preserves the
/// "LLM-every-run" baseline the catalog decisions describe and
/// makes the cache surface explicit.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoverFacetPhase {
    /// When `true`, every cluster goes through
    /// [`FacetCache::get_or_compute`] so a cache hit skips the
    /// LLM call. When `false` (the default), the LLM is invoked
    /// on every cluster and the result is only written to the
    /// run dir.
    pub cache_enabled: bool,
}

impl DiscoverFacetPhase {
    /// Build a phase with the cache flag set explicitly.
    pub fn with_cache(cache_enabled: bool) -> Self {
        Self { cache_enabled }
    }
}

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
        tracing::debug!(cache_enabled = self.cache_enabled, "discover_facet: enter");
        let clusters_dir = ctx.run_dir().clusters();
        let facets_dir = ctx.run_dir().facets();
        let _ = std::fs::create_dir_all(&facets_dir);

        let brief: serde_json::Value = read_json(&ctx.run_dir().brief())?;
        let brief_text = serde_json::to_string(&brief).map_err(Error::from)?;

        // Walk clusters_dir for primary `cluster_NN.json` files. The
        // filter drops both `index.json` (the tally sidecar
        // emitted by `discover_cluster`) and every `*.meta.json`
        // sealed sidecar. Without the `.meta.json` filter, each
        // sidecar is read as a phantom cluster file with empty
        // members; the tagger pass runs once per sidecar and the
        // facet list ends up referencing `cluster_NN` ids that
        // do not survive the `discover_extract` round
        // (`cluster.members.is_empty()` skips them). Verified on
        // run-real-600/.runs/01a0228d-…: the same 579 phantom
        // clusters cascaded into 579 empty `FacetList`s in
        // `facets/` and one valid `FacetList` pointing at the
        // only real cluster.
        let mut cluster_paths: Vec<PathBuf> =
            crate::phases::util::primary_json_paths(&clusters_dir)?
                .into_iter()
                .filter(|p| p.file_name().and_then(|s| s.to_str()) != Some("index.json"))
                .collect();
        cluster_paths.sort();

        if cluster_paths.is_empty() {
            tracing::error!("discover_facet: zero clusters found");
            return Err(Error::InvalidState(
                "discover_facet found zero clusters".into(),
            ));
        }
        tracing::debug!(
            cluster_count = cluster_paths.len(),
            "discover_facet: clusters enumerated"
        );

        let mut paths: Vec<PathBuf> = Vec::new();
        // Open the cross-run facet cache. The cache is keyed by
        // sha256(brief + category_id) and persisted under
        // MOAGAN_HOME/cache/facets/. `cache_enabled` gates
        // whether the phase consults the cache at all; the
        // structure is still constructed (cheap — just a
        // PathBuf + Arc counters) so the hit/miss/store paths
        // share the same `get_or_compute` plumbing.
        let cache = FacetCache::new(ctx.home.cross_run_facet_cache_dir(), facet_cache_ttl_secs());
        let cache_enabled = self.cache_enabled;

        let futures = cluster_paths.iter().enumerate().map(|(idx, path)| {
            let path = path.clone();
            let brief = brief_text.clone();
            let ctx = ctx.clone();
            let mut cache = cache.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let cluster: Cluster = read_json(&path)?;
                let cat_id = DiscoverFacetPhase::category_id_for(idx);
                let synthetic_key = crate::discovery::facet::cache_key(&brief, &cat_id);

                // The cache hit path always writes the list to
                // the run dir; the compute path runs the LLM
                // first and writes the freshly-derived list.
                // `get_or_compute` collapses the
                // lookup → compute → store triple into one
                // call (catalog D.13.13). The inner closure
                // gets a fresh `RunContext` clone (so the outer
                // `ctx` stays usable for the run-dir write)
                // plus cloned `cluster` and `cat_id` so the
                // outer `&cat_id` reference below keeps
                // compiling.
                let list: FacetList = if cache_enabled {
                    let inner_ctx = ctx.clone();
                    let cluster_for_payload = cluster.clone();
                    let cat_id_for_payload = cat_id.clone();
                    let brief_for_payload = brief.clone();
                    cache
                        .get_or_compute(&synthetic_key, || async move {
                            derive_facets(
                                &inner_ctx,
                                &cluster_for_payload,
                                &cat_id_for_payload,
                                &brief_for_payload,
                            )
                            .await
                        })
                        .await?
                } else {
                    derive_facets(&ctx, &cluster, &cat_id, &brief).await?
                };

                let out_path = std::path::Path::new(&ctx.run_dir().facets())
                    .join(format!("{cat_id}_facets.json"));
                crate::phases::util::write_json(&out_path, &list)?;
                Ok::<PathBuf, crate::error::Error>(out_path)
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
            tracing::error!("discover_facet: zero facet lists produced");
            return Err(Error::InvalidState(
                "discover_facet produced zero facet lists".into(),
            ));
        }
        tracing::info!(
            facet_lists_written = paths.len(),
            "discover_facet: phase complete"
        );

        Ok(PhaseOutput::Sketches(paths))
    }
}

/// Run the LLM facet derivation for a single cluster and build
/// the [`FacetList`] payload. Extracted from the per-cluster
/// future so the same closure can be passed to
/// [`FacetCache::get_or_compute`] without duplicating the
/// retry/parse/parse-or-default pipeline.
async fn derive_facets(
    ctx: &RunContext,
    cluster: &Cluster,
    cat_id: &str,
    brief: &str,
) -> Result<FacetList> {
    let user = DiscoverFacetPhase::user_payload(cluster, brief);
    let raw: FacetDerivation = ctx
        .call_with_retry_parse(
            crate::llm::Role::FacetDeriver,
            crate::llm::prompts::system_prompt(crate::llm::Role::FacetDeriver).to_owned(),
            user,
            crate::llm::prompts::system_prompt(crate::llm::Role::FacetDeriver),
            3,
        )
        .await
        // PR: fix(discover-facet) — surface silent fallback. The previous
        // `.unwrap_or_default()` swallowed the LLM error so operators had no
        // visibility into which clusters produced empty facet lists; a fully
        // silent failure surfaces downstream as
        // `discover_extract produced zero facet extractions` after the fact,
        // with no way to correlate it to the upstream provider. Emitting a
        // `tracing::warn!` per cluster keeps the existing tolerance policy
        // (continue-on-empty is the documented behaviour) while letting the
        // operator grep `facet derivation failed` to count how systemic the
        // failure is. Verified locally against the 2026-08-20 run-comparison
        // mini-m1 (`facets=0` across 705 facet lists) to confirm the warn
        // fire happens exactly when the upstream LLM returns a schema-violating
        // response after 3 retries.
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                role = "facet-deriver",
                cluster_id = %cluster.id,
                category_id = %cat_id,
                "facet derivation failed after retries; defaulting to empty facet list"
            );
            FacetDerivation::default()
        });
    let triples: Vec<(String, String, bool)> = raw
        .facets
        .into_iter()
        .map(|t| (t.name, t.description, t.required))
        .collect();
    Ok(FacetList::from_triples(
        cat_id,
        &cluster.id,
        brief,
        crate::time::now_unix_secs(),
        triples,
    ))
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

    #[test]
    fn phase_default_has_cache_disabled() {
        let p = DiscoverFacetPhase::default();
        assert!(
            !p.cache_enabled,
            "default DiscoverFacetPhase must keep the cache off so the LLM-every-run baseline is preserved"
        );
    }

    #[test]
    fn with_cache_constructor_sets_flag() {
        let p = DiscoverFacetPhase::with_cache(true);
        assert!(p.cache_enabled);
        let p = DiscoverFacetPhase::with_cache(false);
        assert!(!p.cache_enabled);
    }
}
