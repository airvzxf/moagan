//! Discovery mode — `discover_tag` phase.
//!
//! For each sketch produced by `DiscoverMatrixPhase`, ask the LLM
//! to classify it into a primary category with subcategory,
//! difficulty, and a similarity score. The output is one
//! `tags/sk_<id>_tags.json` per sketch (per V4 §6.5), plus a
//! `tags/index.json` that powers the cluster phase.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;

use crate::discovery::tagger::{UNCATEGORIZED_THRESHOLD, sanitise, uncategorized_ratio};
use crate::domain::{Sketch, SketchTags};
use crate::error::{Error, Result};
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Discovery tag phase. Reads every sketch under `sketches/`,
/// classifies them with the LLM, and writes the per-sketch tags
/// plus an index file.
pub struct DiscoverTagPhase;

impl DiscoverTagPhase {
    /// Build the user payload for the tagger. The model receives
    /// the full sketch (so it can use the thesis + key_decisions)
    /// and is asked to return a `SketchTags` JSON object.
    fn user_payload(sketch: &Sketch) -> String {
        serde_json::to_string(sketch).unwrap_or_else(|_| String::new())
    }
}

#[async_trait]
impl Phase for DiscoverTagPhase {
    fn name(&self) -> &'static str {
        "discover_tag"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let sketches_dir = ctx.run_dir().sketches();
        let tags_dir = ctx.run_dir().tags();
        std::fs::create_dir_all(&tags_dir)?;

        let mut paths: Vec<PathBuf> = std::fs::read_dir(&sketches_dir)?
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        paths.sort();

        if paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_tag found zero sketches".into(),
            ));
        }

        let system = Arc::new(system_prompt(Role::Tagger).to_owned());
        let futures = paths.iter().map(|path| {
            let path = path.clone();
            let system = Arc::clone(&system);
            let ctx = ctx.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let sketch: Sketch = read_json(&path)?;
                let user = DiscoverTagPhase::user_payload(&sketch);
                let mut tags: SketchTags = ctx
                    .call_with_retry_parse(
                        Role::Tagger,
                        system.as_str().to_owned(),
                        user,
                        system_prompt(Role::Tagger),
                        3,
                    )
                    .await?;
                if tags.sketch_id.is_empty() {
                    tags.sketch_id = sketch.id.clone();
                }
                sanitise(&mut tags);
                Ok::<(PathBuf, SketchTags), crate::error::Error>((path, tags))
            }
        });

        let results = join_all(futures).await;
        let mut kept = Vec::new();
        let mut all_tags: Vec<SketchTags> = Vec::new();
        for r in results {
            let (_sketch_path, tags) = match r {
                Ok(v) => v,
                Err(e) => {
                    let _ = ctx.telemetry.warn(
                        "phase.discover_tag.skipped",
                        "warn",
                        "tagging failed for one sketch",
                        serde_json::json!({"error": e.to_string()}),
                        crate::telemetry::WarningContext {
                            phase: Some("discover_tag".into()),
                            role: Some("tagger".into()),
                            ..Default::default()
                        },
                    );
                    continue;
                }
            };
            let sketch_id = tags.sketch_id.clone();
            let tag_path = tags_dir.join(format!("{sketch_id}_tags.json"));
            write_json(&tag_path, &tags)?;
            kept.push(tag_path);
            all_tags.push(tags);
        }

        if kept.is_empty() {
            return Err(Error::InvalidState(
                "discover_tag produced zero tags".into(),
            ));
        }

        // Index file: maps each sketch path to its tag path, plus a
        // tally of (sketch path, primary, subcategory).
        let index: serde_json::Value = serde_json::json!({
            "version": "v1",
            "tags_dir": "tags",
            "sketches_dir": "sketches",
            "uncategorized_threshold": UNCATEGORIZED_THRESHOLD,
            "uncategorized_ratio": uncategorized_ratio(&all_tags),
            "tally": all_tags.iter().map(|t| serde_json::json!({
                "sketch_id": t.sketch_id,
                "primary": t.primary,
                "subcategory": t.subcategory,
                "difficulty": t.difficulty,
            })).collect::<Vec<_>>(),
        });
        let index_path = tags_dir.join("index.json");
        write_json(&index_path, &index)?;

        let ratio = uncategorized_ratio(&all_tags);
        if ratio > 0.3 {
            let _ = ctx.telemetry.warn(
                "phase.discover_tag.uncategorized_exceeded",
                "warn",
                "more than 30% of sketches were classified as uncategorized",
                serde_json::json!({"ratio": ratio, "total": all_tags.len()}),
                crate::telemetry::WarningContext {
                    phase: Some("discover_tag".into()),
                    role: Some("tagger".into()),
                    ..Default::default()
                },
            );
        }

        Ok(PhaseOutput::Sketches(kept))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_payload_round_trips_sketch() {
        let s = Sketch {
            id: "sk_001".into(),
            thesis: "Use Rust + SQLite + a single binary for the orchestration layer.".into(),
            key_decisions: vec!["single binary".into(), "SQLite only".into()],
            ..Default::default()
        };
        let p = DiscoverTagPhase::user_payload(&s);
        assert!(p.contains("sk_001"));
        assert!(p.contains("single binary"));
    }
}
