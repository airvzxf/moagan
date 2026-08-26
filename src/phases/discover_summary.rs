//! Discovery mode — `discover_summary` phase.
//!
//! Reads every `final/cat_NN.json` and the `tags/index.json` tally
//! to produce three summary files:
//!
//! - `final/summary.md` — executive index (counts + categories by
//!   density).
//! - `final/uncategorized.md` — when ≥ 3 sketches landed in
//!   `uncategorized` (V4 §6.10). The body carries six sections:
//!   `## Resumen`, `## Sketches`, `## Ideas sueltas`,
//!   `## Temas recurrentes`, `## Contradicciones detectadas`, and
//!   `## Preguntas abiertas`. The four latter sections are populated
//!   from `Sketch` (uncategorized tally), `Cluster` (centroid
//!   summaries), `Contradiction` (inter-cluster pairs), and
//!   `FacetList` (facets without an extraction).
//! - `discovery.json` — discovery sub-manifest sealed with the
//!   human checkpoint decision (V4 §6.11 + T01-06 §9.11).
//!
//! The checkpoint fires once, at the end of discovery, with four
//! actions: `Approve | ReviewTopics | Block | ExportRaw`. The
//! response is parsed into [`Resolution`] (Approved / Rejected /
//! Modify) and the section written by this phase reflects whichever
//! action fired:
//!
//! - `Approved` — `discovery.approved = true`,
//!   `discovery.human_checkpoint.decision = "approve"`.
//! - `Rejected` (the `block` token) — `discovery.approved = false`,
//!   `discovery.human_checkpoint.decision = "block"`, and the
//!   phase returns [`Error::Cancelled`] so the CLI surfaces the
//!   abort to the operator (no point continuing past a blocked
//!   discovery — V4 §6.11 explicit).
//! - `Modify` (anything else, including the `review` / `export`
//!   tokens with optional arguments) — the verbatim text is
//!   persisted via [`crate::checkpoint::persist_modify_note`] so
//!   the next `moagan discover` cycle can surface the operator's
//!   intent. `approved` stays `false` (the user did not bless the
//!   output) and the section's decision mirrors the raw text so
//!   the audit trail records what was actually typed.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::checkpoint::{
    Checkpoint, CheckpointKind, CheckpointOpts, Resolution, persist_modify_note,
};
use crate::domain::{
    CategoryDoc, Cluster, Contradiction, DiscoverySection, DiscoverySummary, FacetList,
    HumanCheckpointDecision, Sketch, UncategorizedDoc,
};
use crate::error::{Error, Result};
// `RunId` is referenced in the unit tests; the import is dead in the
// production build but the test module re-exports it.
#[allow(unused_imports)]
use crate::ids::RunId;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::time::now_unix_secs;

/// Filename of the discovery sub-manifest written by this phase.
/// Lives at the run root (next to `manifest.json`, `brief.json`,
/// etc.) so a `moagan inspect <run>` lookup does not need to know
/// about discovery-specific directory layout.
const DISCOVERY_SECTION_FILE: &str = "discovery.json";

/// Maximum number of clusters to surface under `## Temas recurrentes`.
/// Keeps the section bounded; operators read it for orientation, not
/// for exhaustive coverage. Mirrors the cap used by
/// `discover_contradict` for the same reason.
const TOP_CLUSTERS: usize = 5;

/// Maximum number of contradictions to surface under
/// `## Contradicciones detectadas`. Mirrors `discover_contradict`'s
/// `MAX_PAIRS` so the summary stays at parity with the source.
const TOP_CONTRADICTIONS: usize = 16;

/// Path constants used by the summary phase. Captured for
/// diagnostics and to keep the JSON index self-describing.
#[allow(dead_code)]
const SKETCHES_DIR: &str = "sketches";
#[allow(dead_code)]
const TAGS_DIR: &str = "tags";

/// Sibling summary phase. Reads the artifacts dropped by the
/// earlier phases and emits the user-facing executive index.
pub struct DiscoverSummaryPhase;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct TagIndex {
    #[serde(default)]
    tally: Vec<TagTally>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct TagTally {
    sketch_id: String,
    primary: String,
    subcategory: String,
    difficulty: String,
}

impl DiscoverSummaryPhase {
    /// Read `tags/index.json` if it exists.
    fn read_tag_index(ctx: &RunContext) -> Result<TagIndex> {
        let path = ctx.run_dir().tags().join("index.json");
        if !path.exists() {
            return Ok(TagIndex::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;
        // The tag phase writes the tally under `tally`. We accept
        // either shape gracefully.
        let tally: Vec<TagTally> = if let Some(arr) = v.get("tally").and_then(|x| x.as_array()) {
            arr.iter()
                .filter_map(|t| serde_json::from_value(t.clone()).ok())
                .collect()
        } else {
            Vec::new()
        };
        Ok(TagIndex { tally })
    }

    /// Collect every `final/cat_NN.json` (excluding the index JSON).
    fn read_category_docs(ctx: &RunContext) -> Result<Vec<CategoryDoc>> {
        let mut docs: Vec<CategoryDoc> = Vec::new();
        for entry in std::fs::read_dir(ctx.run_dir().final_dir())?.filter_map(|r| r.ok()) {
            let path = entry.path();
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if !name.starts_with("cat_")
                || !name.ends_with(".json")
                || name == "cat_index.json"
                || name.ends_with(".meta.json")
            {
                continue;
            }
            let doc: CategoryDoc = match crate::phases::util::read_json(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            docs.push(doc);
        }
        docs.sort_by(|a, b| {
            b.density
                .partial_cmp(&a.density)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(docs)
    }

    /// Count the facet lists persisted under `facets/` by
    /// `discover_facet`. Each list is one `<cat_id>_facets.json`,
    /// so we count `.json` siblings of the dir. Missing / empty
    /// directories collapse to `0` so the roll-up is honest on
    /// truncated runs.
    fn count_facet_lists(facets_dir: &Path) -> usize {
        match std::fs::read_dir(facets_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let p = e.path();
                    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    // Exclude `.meta.json` sidecars and any other sidecar that
                    // happens to share the `.json` extension suffix.
                    !name.ends_with(".meta.json")
                        && (p.extension().and_then(|s| s.to_str()) == Some("json"))
                })
                .count(),
            Err(_) => 0,
        }
    }

    /// Read `contradictions/contradictions.json` and return the
    /// number of `Contradiction` rows it carries. Missing file /
    /// parse failure collapse to `0` — the count is for the
    /// human-checkpoint prompt, not the audit trail.
    fn count_contradictions(contradictions_dir: &Path) -> usize {
        let path = contradictions_dir.join("contradictions.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                match serde_json::from_slice::<Vec<crate::domain::Contradiction>>(&bytes) {
                    Ok(items) => items.len(),
                    Err(_) => 0,
                }
            }
            Err(_) => 0,
        }
    }

    /// Build the question text the operator sees at the discovery
    /// checkpoint. Mirrors V4 §6.11 / T01-06 §9.11 — the four
    /// actions are listed verbatim so a user who has not read
    /// the docs can still pick one.
    fn build_question(cat_count: usize, facet_count: usize, contradictions: usize) -> String {
        format!(
            "discovered {cat_count} categor{}, {facet_count} facet{}, \
             {contradictions} contradiction{}; next action? \
             [approve|review|block|export]",
            if cat_count == 1 { "y" } else { "ies" },
            if facet_count == 1 { "" } else { "s" },
            if contradictions == 1 { "" } else { "s" },
        )
    }

    /// Persist the [`DiscoverySection`] sidecar at
    /// `<run_dir>/discovery.json`. The atomic writer keeps the
    /// on-disk shape identical to every other Phase D sidecar
    /// so the dashboard / inspect CLI can treat them
    /// uniformly.
    fn write_discovery_section(run_root: &Path, section: &DiscoverySection) -> Result<PathBuf> {
        let path = run_root.join(DISCOVERY_SECTION_FILE);
        write_json(&path, section)?;
        Ok(path)
    }

    /// Render the executive summary as markdown.
    fn render_summary_markdown(s: &DiscoverySummary) -> String {
        let mut out = String::new();
        out.push_str("# Discovery summary\n\n");
        out.push_str(&format!("Run id: `{}`\n\n", s.run_id));
        out.push_str(&format!("Total sketches: **{}**\n", s.total_sketches));
        out.push_str(&format!("Categories: **{}**\n", s.category_count));
        out.push_str(&format!("Uncategorized: **{}**\n\n", s.uncategorized_count));
        out.push_str("## Categories by density\n\n");
        for (i, id) in s.categories_by_density.iter().enumerate() {
            out.push_str(&format!("{}. `{}`\n", i + 1, id));
        }
        out.push_str("\n## Executive summary\n\n");
        out.push_str(&s.executive_summary);
        out.push('\n');
        out
    }

    /// Read every `clusters/cluster_NN.json` (skipping `index.json`).
    fn read_clusters(ctx: &RunContext) -> Result<Vec<Cluster>> {
        let clusters_dir = ctx.run_dir().clusters();
        if !clusters_dir.exists() {
            return Ok(Vec::new());
        }
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
            let cluster: Cluster = match read_json(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            clusters.push(cluster);
        }
        Ok(clusters)
    }

    /// Read `contradictions/contradictions.json` if it exists. The
    /// phase writes the file as a top-level JSON array of
    /// `Contradiction`; absent or malformed files are tolerated
    /// because the contradiction phase may not have run yet.
    fn read_contradictions(ctx: &RunContext) -> Result<Vec<Contradiction>> {
        let path = ctx.run_dir().contradictions().join("contradictions.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        match read_json::<Vec<Contradiction>>(&path) {
            Ok(v) => Ok(v),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Read every `facets/<category>_facets.json` (`FacetList`). A
    /// missing directory returns an empty vec; malformed files are
    /// skipped so the summary never aborts on a single bad payload.
    fn read_facet_lists(ctx: &RunContext) -> Result<Vec<FacetList>> {
        let facets_dir = ctx.run_dir().facets();
        if !facets_dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&facets_dir)?
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
        paths.sort();
        let mut lists: Vec<FacetList> = Vec::with_capacity(paths.len());
        for path in &paths {
            if let Ok(list) = read_json::<FacetList>(path) {
                lists.push(list);
            }
        }
        Ok(lists)
    }

    /// Collect the set of `(category_id, facet_id)` tuples for which
    /// `extractions/<cat>/faceta_<slug>.json` exists. Used by
    /// `render_uncategorized` to compute which facets are
    /// unanswered (in `facets/` but missing in `extractions/`).
    fn read_extraction_ids(
        ctx: &RunContext,
    ) -> Result<std::collections::HashSet<(String, String)>> {
        let mut out: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let extractions_dir = ctx.run_dir().extractions();
        if !extractions_dir.exists() {
            return Ok(out);
        }
        for cat_entry in std::fs::read_dir(&extractions_dir)?.filter_map(|r| r.ok()) {
            let cat_path = cat_entry.path();
            if !cat_path.is_dir() {
                continue;
            }
            let category_id = match cat_path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            for ext_entry in std::fs::read_dir(&cat_path).into_iter().flatten().flatten() {
                let ext_path = ext_entry.path();
                let name = match ext_path.file_name().and_then(|s| s.to_str()) {
                    Some(s) => s,
                    None => continue,
                };
                // Files are emitted as `faceta_<slug>.json` per
                // `discover_extract.rs::DiscoverExtractPhase`. The
                // `.md` mirror carries the same `<slug>` but is
                // not a stable contract surface, so the JSON
                // sidecar is the source of truth.
                let stem = match name
                    .strip_prefix("faceta_")
                    .and_then(|s| s.strip_suffix(".json"))
                {
                    Some(s) => s,
                    None => continue,
                };
                out.insert((category_id.clone(), stem.to_string()));
            }
        }
        Ok(out)
    }

    /// Read each uncategorized sketch by id and return its `thesis`
    /// for the `## Ideas sueltas` section. Missing or malformed
    /// sketches are skipped silently — the tally is the canonical
    /// source of ids and the file is a thin projection.
    fn read_uncategorized_theses(
        ctx: &RunContext,
        sketch_ids: &[String],
    ) -> Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::with_capacity(sketch_ids.len());
        for id in sketch_ids {
            let path = ctx.run_dir().sketches().join(format!("{id}.json"));
            if let Ok(sk) = read_json::<Sketch>(&path) {
                out.push((id.clone(), sk.thesis));
            } else {
                out.push((id.clone(), String::new()));
            }
        }
        Ok(out)
    }

    /// Render the `uncategorized.md` body with the five sections
    /// V4 §6.10 prescribes. The `tag_index.tally` provides the
    /// canonical set of uncategorized sketch ids; `clusters`,
    /// `contradictions`, `facet_lists`, and `extraction_ids` are
    /// loaded separately because they live in sibling
    /// sub-directories. Sections are emitted in the spec order
    /// even when their data sources are empty so downstream
    /// parsers can rely on a stable heading sequence.
    fn render_uncategorized(ctx: &RunContext, tag_index: &TagIndex) -> Result<String> {
        let uncategorized_sketch_ids: Vec<String> = tag_index
            .tally
            .iter()
            .filter(|t| t.primary == "uncategorized")
            .map(|t| t.sketch_id.clone())
            .collect();
        let uncategorized_count = uncategorized_sketch_ids.len();

        let clusters = DiscoverSummaryPhase::read_clusters(ctx)?;
        let contradictions = DiscoverSummaryPhase::read_contradictions(ctx)?;
        let facet_lists = DiscoverSummaryPhase::read_facet_lists(ctx)?;
        let extraction_ids = DiscoverSummaryPhase::read_extraction_ids(ctx)?;
        let theses =
            DiscoverSummaryPhase::read_uncategorized_theses(ctx, &uncategorized_sketch_ids)?;

        let mut body = String::new();
        body.push_str("# Categoría: uncategorized\n\n");
        body.push_str(&format!(
            "## Resumen\n\n{uncategorized_count} sketches no categorizados. Contenido heterogéneo.\n\n"
        ));
        body.push_str("## Sketches\n\n");
        for id in &uncategorized_sketch_ids {
            body.push_str(&format!("- `{id}`\n"));
        }
        body.push('\n');

        // ## Ideas sueltas — every uncategorized sketch's thesis.
        body.push_str("## Ideas sueltas\n\n");
        if theses.is_empty() {
            body.push_str("_No hay sketches sueltos._\n\n");
        } else {
            for (id, thesis) in &theses {
                if thesis.is_empty() {
                    body.push_str(&format!("- `{id}`: _(thesis no disponible)_\n"));
                } else {
                    body.push_str(&format!("- `{id}`: {thesis}\n"));
                }
            }
            body.push('\n');
        }

        // ## Temas recurrentes — top-K clusters by member count,
        // labelled with the LLM refinement pass summary.
        body.push_str("## Temas recurrentes\n\n");
        let mut sorted_clusters = clusters.clone();
        sorted_clusters.sort_by(|a, b| {
            b.members
                .len()
                .cmp(&a.members.len())
                .then_with(|| a.id.cmp(&b.id))
        });
        let top_clusters: Vec<&Cluster> = sorted_clusters.iter().take(TOP_CLUSTERS).collect();
        if top_clusters.is_empty() {
            body.push_str("_No hay clusters consolidados todavía._\n\n");
        } else {
            for cluster in &top_clusters {
                let label = if cluster.label.is_empty() {
                    cluster.id.clone()
                } else {
                    cluster.label.clone()
                };
                let summary = if cluster.summary.is_empty() {
                    String::from("(sin resumen)")
                } else {
                    cluster.summary.clone()
                };
                body.push_str(&format!(
                    "- **{label}** (`{}`, {} sketches, cohesión {:.2}): {summary}\n",
                    cluster.id,
                    cluster.members.len(),
                    cluster.cohesion,
                ));
            }
            body.push('\n');
        }

        // ## Contradicciones detectadas — read from the
        // contradiction phase sidecar, sorted by severity then
        // topic so the most consequential surface first.
        body.push_str("## Contradicciones detectadas\n\n");
        let mut sorted_contradictions = contradictions.clone();
        sorted_contradictions.sort_by(|a, b| {
            severity_rank_desc(&a.severity)
                .cmp(&severity_rank_desc(&b.severity))
                .then_with(|| a.topic.cmp(&b.topic))
        });
        let top_contradictions: Vec<&Contradiction> = sorted_contradictions
            .iter()
            .take(TOP_CONTRADICTIONS)
            .collect();
        if top_contradictions.is_empty() {
            body.push_str("_No se detectaron contradicciones._\n\n");
        } else {
            for c in &top_contradictions {
                let topic = if c.topic.is_empty() {
                    String::from("(sin tema)")
                } else {
                    c.topic.clone()
                };
                let desc = if c.description.is_empty() {
                    String::from("(sin descripción)")
                } else {
                    c.description.clone()
                };
                let severity = if c.severity.is_empty() {
                    String::from("unknown")
                } else {
                    c.severity.clone()
                };
                body.push_str(&format!(
                    "- `{a}` vs `{b}` — **{topic}** ({severity}): {desc}\n",
                    a = c.cluster_a,
                    b = c.cluster_b,
                ));
            }
            body.push('\n');
        }

        // ## Preguntas abiertas — facets in `facets/` that the
        // extraction phase never produced a sidecar for. The
        // Facet struct does not carry an `unanswered` flag of its
        // own, so the absence of a matching
        // `extractions/<cat>/faceta_<slug>.json` is the natural
        // signal.
        body.push_str("## Preguntas abiertas\n\n");
        let mut unanswered: Vec<(String, String, String)> = Vec::new();
        for list in &facet_lists {
            for facet in &list.facets {
                let key = (list.category_id.clone(), facet.id.clone());
                if !extraction_ids.contains(&key) {
                    let desc = if facet.description.is_empty() {
                        String::from("(sin descripción)")
                    } else {
                        facet.description.clone()
                    };
                    unanswered.push((list.category_id.clone(), facet.id.clone(), desc));
                }
            }
        }
        if unanswered.is_empty() {
            body.push_str("_Todas las facetas tienen extracción._\n\n");
        } else {
            for (cat_id, facet_id, desc) in &unanswered {
                body.push_str(&format!("- `{cat_id}/{facet_id}`: {desc}\n"));
            }
            body.push('\n');
        }

        Ok(body)
    }
}

/// Severity rank used to sort contradictions for the
/// `## Contradicciones detectadas` section. Mirrors the rank used
/// inside `discovery::contradiction::severity_rank` so the order is
/// stable across phases.
fn severity_rank_desc(severity: &str) -> u8 {
    match severity {
        "high" => 0,
        "medium" => 1,
        "low" => 2,
        _ => 3,
    }
}

#[async_trait]
impl Phase for DiscoverSummaryPhase {
    fn name(&self) -> &'static str {
        "discover_summary"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!("discover_summary: enter");
        let final_dir = ctx.run_dir().final_dir();
        let _ = std::fs::create_dir_all(&final_dir);
        let _ = std::fs::create_dir_all(ctx.run_dir().tags());
        let _ = std::fs::create_dir_all(ctx.run_dir().sketches());
        let _ = std::fs::create_dir_all(ctx.run_dir().clusters());

        let docs = DiscoverSummaryPhase::read_category_docs(ctx)?;
        let tag_index = DiscoverSummaryPhase::read_tag_index(ctx)?;

        let total_sketches = std::fs::read_dir(ctx.run_dir().sketches())?
            .filter_map(|r| r.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s == "json")
                    .unwrap_or(false)
            })
            .count();

        let uncategorized_count = tag_index
            .tally
            .iter()
            .filter(|t| t.primary == "uncategorized")
            .count();

        let summary = DiscoverySummary {
            run_id: ctx.run_id,
            total_sketches,
            category_count: docs.len(),
            uncategorized_count,
            categories_by_density: docs.iter().map(|d| d.category_id.clone()).collect(),
            executive_summary: docs
                .iter()
                .take(3)
                .map(|d| format!("- `{}` (density {:.2})", d.category_id, d.density))
                .collect::<Vec<_>>()
                .join("\n"),
            schema_version: "v1".into(),
        };

        let _ = summary.run_id;
        let summary_md = DiscoverSummaryPhase::render_summary_markdown(&summary);
        let md_path = final_dir.join("summary.md");
        std::fs::write(&md_path, &summary_md)?;
        let json_path = final_dir.join("summary.json");
        write_json(&json_path, &summary)?;

        // uncategorized.md is emitted when there are >= 3 untagged
        // sketches (V4 §6.10). The body is built by
        // `render_uncategorized`, which populates the four missing
        // sections from clusters, contradictions, and facets.
        let mut uncategorized_paths: Vec<PathBuf> = Vec::new();
        if uncategorized_count >= 3 {
            let body = DiscoverSummaryPhase::render_uncategorized(ctx, &tag_index)?;

            let doc = UncategorizedDoc {
                count: uncategorized_count,
                body: body.clone(),
                schema_version: "v1".into(),
            };
            let uncat_md = final_dir.join("uncategorized.md");
            std::fs::write(&uncat_md, &body)?;
            let uncat_json = final_dir.join("uncategorized.json");
            write_json(&uncat_json, &doc)?;
            uncategorized_paths.push(uncat_md);
        }

        // V4 §6.11 / T01-06 §9.11 — fire the single human
        // checkpoint at the end of discovery. We collect the
        // roll-up counts before the prompt so the user sees an
        // honest "discovered N categories, M facets, K
        // contradictions" framing instead of an opaque
        // "approve? [Y/n]" line.
        let cat_count = docs.len();
        let facet_count = DiscoverSummaryPhase::count_facet_lists(&ctx.run_dir().facets());
        let contradictions_count =
            DiscoverSummaryPhase::count_contradictions(&ctx.run_dir().contradictions());
        let question =
            DiscoverSummaryPhase::build_question(cat_count, facet_count, contradictions_count);
        let cp = Checkpoint::new(
            CheckpointKind::Discovery {
                cat_count,
                facet_count,
                contradictions: contradictions_count,
            },
            question.clone(),
            true,
        );
        let opts = CheckpointOpts {
            interactive: ctx.interactive,
            // Allow tests (and CI scripts) to pre-canned a
            // response without standing up a TTY. The
            // `human.rs::ask` helper threads this through
            // `parse_resolution` so the same y/yes/approve
            // vocabulary works in both modes.
            stdin_override: None,
            telemetry: Some(ctx.telemetry.clone()),
        };
        let checkpoint_id = cp.id.clone();
        let resolution = crate::checkpoint::ask(&cp, &ctx.run_dir().checkpoints(), &opts)?;
        // Non-interactive runs (`--non-interactive`,
        // `Mode::Batch`) short-circuit through
        // [`crate::checkpoint::skip`] which persists a
        // `<skipped:non_interactive>` marker on the
        // checkpoint sidecar. We surface the same marker on
        // the discovery sub-manifest so a dashboard query
        // can distinguish "the operator pressed approve" from
        // "the prompt was suppressed by the run mode". The
        // `approved` flag stays `false` in that case: the
        // operator never pressed approve, the run just did
        // not have a human in the loop.
        let (approved, decision) = if !ctx.interactive {
            (false, "<skipped:non_interactive>".to_owned())
        } else {
            match &resolution {
                Resolution::Approved => (true, "approve".to_owned()),
                Resolution::Rejected => (false, "block".to_owned()),
                Resolution::Modify(text) => (false, text.clone()),
            }
        };
        // Persist the operator's verbatim text (anything that
        // was not approve/block) as a modify note so the next
        // discovery cycle can re-feed it into the LLM prompts
        // (D.22.1 / catalog decision on modify-note plumbing).
        if let Resolution::Modify(text) = &resolution {
            persist_modify_note(ctx.run_dir().root(), "discover_summary", text)?;
        }
        let human_checkpoint = Some(HumanCheckpointDecision {
            decision: decision.clone(),
            at_unix: now_unix_secs(),
            checkpoint_id: checkpoint_id.clone(),
        });
        let section = DiscoverySection {
            cat_count,
            facet_count,
            contradictions: contradictions_count,
            human_checkpoint,
            approved,
            schema_version: "v1".into(),
        };
        let discovery_path =
            DiscoverSummaryPhase::write_discovery_section(ctx.run_dir().root(), &section)?;

        // V4 §6.11 explicit: a blocked discovery cannot
        // continue. Surface the abort to the caller so the CLI
        // exits non-zero and the operator sees the decision in
        // the log. We still wrote the sidecar above so the
        // audit trail records the block even when the run
        // terminates here.
        if let Resolution::Rejected = &resolution {
            return Err(Error::Cancelled(
                "user blocked the discovery checkpoint".into(),
            ));
        }

        let mut outputs: Vec<PathBuf> = vec![md_path, discovery_path];
        outputs.extend(uncategorized_paths);

        if outputs.is_empty() {
            tracing::error!("discover_summary: zero outputs produced");
            return Err(Error::InvalidState(
                "discover_summary produced zero outputs".into(),
            ));
        }
        tracing::info!(
            output_count = outputs.len(),
            "discover_summary: phase complete"
        );

        Ok(PhaseOutput::Sketches(outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_summary_includes_counts() {
        let s = DiscoverySummary {
            run_id: RunId::new(),
            total_sketches: 80,
            category_count: 4,
            uncategorized_count: 2,
            categories_by_density: vec!["cat_01".into(), "cat_02".into()],
            executive_summary: "Top three categories: ...".into(),
            schema_version: "v1".into(),
        };
        let m = DiscoverSummaryPhase::render_summary_markdown(&s);
        assert!(m.contains("Total sketches: **80**"));
        assert!(m.contains("Categories: **4**"));
        assert!(m.contains("cat_01"));
        assert!(m.contains("Top three"));
    }

    #[test]
    fn render_summary_orders_by_density() {
        let s = DiscoverySummary {
            run_id: RunId::new(),
            total_sketches: 10,
            category_count: 2,
            uncategorized_count: 0,
            categories_by_density: vec!["cat_01".into(), "cat_02".into()],
            executive_summary: "- cat_01".into(),
            schema_version: "v1".into(),
        };
        let m = DiscoverSummaryPhase::render_summary_markdown(&s);
        let pos_1 = m.find("cat_01").unwrap();
        let pos_2 = m.find("cat_02").unwrap();
        assert!(pos_1 < pos_2);
    }

    #[test]
    fn read_tag_index_defaults_when_missing() {
        crate::test_support::with_moagan_home(
            "discover_summary_read_tag_index_defaults",
            |_home| {
                let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
                let ctx = test_ctx(home, crate::ids::RunId::new());
                let idx = DiscoverSummaryPhase::read_tag_index(&ctx).unwrap();
                assert!(idx.tally.is_empty());
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
    fn empty_run_yields_zero_documents() {
        crate::test_support::with_moagan_home(
            "discover_summary_empty_run_zero_documents",
            |_home| {
                let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
                let run_id = crate::ids::RunId::new();
                let run_dir = home.run_dir(run_id);
                run_dir.ensure().unwrap();
                let ctx = test_ctx(home, run_id);
                let docs = DiscoverSummaryPhase::read_category_docs(&ctx).unwrap();
                assert!(docs.is_empty());
            },
        );
    }

    /// Snapshot test for PR-21: V4 §6.10 requires `uncategorized.md`
    /// to carry `## Ideas sueltas`, `## Temas recurrentes`,
    /// `## Contradicciones detectadas`, and `## Preguntas abiertas`
    /// in addition to the existing `## Resumen` and `## Sketches`.
    /// The test builds a minimal run directory containing one of
    /// each source artefact and pins the section order so the
    /// contract is enforceable without an LLM call.
    #[test]
    fn render_uncategorized_includes_v4_six_ten_sections() {
        crate::test_support::with_moagan_home(
            "discover_summary_render_uncategorized_six_ten",
            |_home| {
                let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
                let run_id = crate::ids::RunId::new();
                let run_dir = home.run_dir(run_id);
                run_dir.ensure().unwrap();

                // Three uncategorized sketch files. The tally below drives
                // `## Sketches` and `## Ideas sueltas`; the file content
                // drives `## Ideas sueltas`'s thesis text.
                // `RunDir::ensure` does not include `sketches/` (the
                // directory is created lazily by the sketch phase), so we
                // create it explicitly here.
                std::fs::create_dir_all(run_dir.sketches()).unwrap();
                for (id, thesis) in [
                    ("sk_aaa", "alpha: minimal viable backend"),
                    ("sk_bbb", "beta: serverless-first approach"),
                    ("sk_ccc", "gamma: durable execution layer"),
                ] {
                    let path = run_dir.sketches().join(format!("{id}.json"));
                    std::fs::write(
                        &path,
                        format!(
                            r#"{{"id":"{id}","thesis":"{thesis}","key_decisions":[],"architecture_outline":"","assumptions":[],"strengths":[],"weaknesses":[],"hard_constraint_check":{{}},"expected_validation":"","angle":""}}"#
                        ),
                    )
                    .unwrap();
                }

                // One cluster so `## Temas recurrentes` has content to
                // project. Members reference the uncategorized sketches so
                // the cluster centroid is non-empty even if irrelevant to
                // the test — we only care about the section's presence
                // and ordering.
                let cluster = crate::domain::Cluster {
                    id: "cluster_01".into(),
                    label: "deployment".into(),
                    summary: "Focuses on rollout strategy".into(),
                    members: vec!["sk_aaa".into(), "sk_bbb".into()],
                    cohesion: 0.81,
                    ..Default::default()
                };
                write_json(&run_dir.clusters().join("cluster_01.json"), &cluster).unwrap();

                // One contradiction so `## Contradicciones detectadas` is
                // populated. The fixture is hand-rolled; the
                // `discover_contradict` phase writes the same shape.
                let contradiction = crate::domain::Contradiction {
                    id: "c_01".into(),
                    cluster_a: "cluster_01".into(),
                    cluster_b: "cluster_02".into(),
                    topic: "consistency".into(),
                    description: "Linearizable vs eventual".into(),
                    severity: "high".into(),
                    ..Default::default()
                };
                let contradictions = vec![contradiction];
                std::fs::create_dir_all(run_dir.contradictions()).unwrap();
                write_json(
                    &run_dir.contradictions().join("contradictions.json"),
                    &contradictions,
                )
                .unwrap();

                // One facet list with two facets; only the first has an
                // extraction so `## Preguntas abiertas` lists the second
                // (unanswered) one.
                let facet_list = crate::domain::FacetList {
                    category_id: "cat_01".into(),
                    cluster_id: "cluster_01".into(),
                    facets: vec![
                        crate::domain::Facet {
                            id: "data-flows".into(),
                            description: "Sequence of data through the system".into(),
                            required: true,
                        },
                        crate::domain::Facet {
                            id: "failure-modes".into(),
                            description: "What breaks first under load".into(),
                            required: true,
                        },
                    ],
                    ..Default::default()
                };
                write_json(&run_dir.facets().join("cat_01_facets.json"), &facet_list).unwrap();

                // Extraction for `data-flows` only — `failure-modes`
                // remains unanswered.
                let ext_dir = run_dir.extractions().join("cat_01");
                std::fs::create_dir_all(&ext_dir).unwrap();
                let extraction = crate::domain::FacetExtraction {
                    facet_id: "data-flows".into(),
                    category_id: "cat_01".into(),
                    body: "_No content available for data-flows._".into(),
                    sources: vec!["sk_aaa".into()],
                    schema_version: "v1".into(),
                };
                write_json(&ext_dir.join("faceta_data-flows.json"), &extraction).unwrap();

                // Tag index: three uncategorized sketches. Other tags are
                // present so the filter must skip them.
                let tag_index = TagIndex {
                    tally: vec![
                        TagTally {
                            sketch_id: "sk_aaa".into(),
                            primary: "uncategorized".into(),
                            subcategory: String::new(),
                            difficulty: "low".into(),
                        },
                        TagTally {
                            sketch_id: "sk_bbb".into(),
                            primary: "uncategorized".into(),
                            subcategory: String::new(),
                            difficulty: "low".into(),
                        },
                        TagTally {
                            sketch_id: "sk_ccc".into(),
                            primary: "uncategorized".into(),
                            subcategory: String::new(),
                            difficulty: "low".into(),
                        },
                        TagTally {
                            sketch_id: "sk_ddd".into(),
                            primary: "auth".into(),
                            subcategory: String::new(),
                            difficulty: "low".into(),
                        },
                    ],
                };

                let ctx = test_ctx(home, run_id);
                let body = DiscoverSummaryPhase::render_uncategorized(&ctx, &tag_index).unwrap();

                // Section order is the contract — V4 §6.10 enumerates the
                // six headings in this sequence.
                let pos = |needle: &str| {
                    body.find(needle)
                        .unwrap_or_else(|| panic!("missing `{needle}`"))
                };
                let resumen = pos("## Resumen");
                let sketches = pos("## Sketches");
                let ideas = pos("## Ideas sueltas");
                let temas = pos("## Temas recurrentes");
                let contradicciones = pos("## Contradicciones detectadas");
                let preguntas = pos("## Preguntas abiertas");
                assert!(resumen < sketches, "## Resumen must precede ## Sketches");
                assert!(
                    sketches < ideas,
                    "## Sketches must precede ## Ideas sueltas"
                );
                assert!(
                    ideas < temas,
                    "## Ideas sueltas must precede ## Temas recurrentes"
                );
                assert!(
                    temas < contradicciones,
                    "## Temas recurrentes must precede ## Contradicciones detectadas"
                );
                assert!(
                    contradicciones < preguntas,
                    "## Contradicciones detectadas must precede ## Preguntas abiertas"
                );

                // Sanity: each section surfaces at least one entry from
                // its source so the test would catch a regression where
                // the wiring breaks but the headings survive.
                assert!(
                    body.contains("minimal viable backend"),
                    "## Ideas sueltas must include the sketch thesis"
                );
                assert!(
                    body.contains("cluster_01") && body.contains("deployment"),
                    "## Temas recurrentes must project the cluster label and id"
                );
                assert!(
                    body.contains("`cluster_01` vs `cluster_02`"),
                    "## Contradicciones detectadas must include the cluster pair"
                );
                assert!(
                    body.contains("failure-modes"),
                    "## Preguntas abiertas must include the unanswered facet"
                );
                // Resolved facet must NOT appear in the unanswered list.
                assert!(
                    !body.contains("`cat_01/data-flows`:"),
                    "## Preguntas abiertas must not list facets that have an extraction"
                );
            },
        );
    }
}
