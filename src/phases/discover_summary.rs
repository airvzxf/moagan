//! Discovery mode — `discover_summary` phase.
//!
//! Reads every `final/cat_NN.json` and the `tags/index.json` tally
//! to produce two summary files:
//!
//! - `final/summary.md` — executive index (counts + categories by
//!   density).
//! - `final/uncategorized.md` — when ≥ 3 sketches landed in
//!   `uncategorized` (V4 §6.10).

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::{CategoryDoc, DiscoverySummary, UncategorizedDoc};
use crate::error::{Error, Result};
// `RunId` is referenced in the unit tests; the import is dead in the
// production build but the test module re-exports it.
#[allow(unused_imports)]
use crate::ids::RunId;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::write_json;

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
            if !name.starts_with("cat_") || !name.ends_with(".json") || name == "cat_index.json" {
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
}

#[async_trait]
impl Phase for DiscoverSummaryPhase {
    fn name(&self) -> &'static str {
        "discover_summary"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
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
        // sketches (V4 §6.10).
        let mut uncategorized_paths: Vec<PathBuf> = Vec::new();
        if uncategorized_count >= 3 {
            let uncategorized_sketch_ids: Vec<String> = tag_index
                .tally
                .iter()
                .filter(|t| t.primary == "uncategorized")
                .map(|t| t.sketch_id.clone())
                .collect();

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

        let mut outputs: Vec<PathBuf> = vec![md_path];
        outputs.extend(uncategorized_paths);

        if outputs.is_empty() {
            return Err(Error::InvalidState(
                "discover_summary produced zero outputs".into(),
            ));
        }

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
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
        let ctx = test_ctx(home, crate::ids::RunId::new());
        let idx = DiscoverSummaryPhase::read_tag_index(&ctx).unwrap();
        assert!(idx.tally.is_empty());
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
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve().unwrap());
        let run_id = crate::ids::RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().unwrap();
        let ctx = test_ctx(home, run_id);
        let docs = DiscoverSummaryPhase::read_category_docs(&ctx).unwrap();
        assert!(docs.is_empty());
    }
}
