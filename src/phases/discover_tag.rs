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

use crate::discovery::tagger::{sanitise, uncategorized_ratio};
use crate::discovery::tagger_threshold::TaggerThreshold;
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

        // Read every primary sketch. Without the `.meta.json` filter,
        // every sealed sidecar was passed to the tagger — 578
        // phantom `SketchTags` written to `tags/` per run with empty
        // `sketch_id`. Filter mirrors `discover_cluster::execute`.
        let paths: Vec<PathBuf> = crate::phases::util::primary_json_paths(&sketches_dir)?;

        if paths.is_empty() {
            return Err(Error::InvalidState(
                "discover_tag found zero sketches".into(),
            ));
        }

        let system = Arc::new(system_prompt(Role::Tagger).to_owned());
        let threshold =
            TaggerThreshold::from_config_value(Some(ctx.config.discovery.tag_threshold));
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
                sanitise(&mut tags, &threshold);
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
            // Degraded mode: every tagging call failed (typically
            // because the LLM is unreachable or the breaker is
            // open). A hard failure here used to abort the entire
            // run; we now keep the pipeline going on raw sketches
            // (no tags) so the cluster / integrator / summary
            // phases still produce useful output. The
            // `tags/index.json` with `"degraded": true` is the
            // signal downstream consumers see; the warning is
            // the signal operators see.
            let _ = ctx.telemetry.warn(
                "phase.discover_tag.zero_tags",
                "warn",
                "discover_tag produced zero tags; continuing in degraded mode without per-sketch tags",
                serde_json::json!({"total_sketches": paths.len(), "kept": 0}),
                crate::telemetry::WarningContext {
                    phase: Some("discover_tag".into()),
                    role: Some("tagger".into()),
                    ..Default::default()
                },
            );
            let index: serde_json::Value = serde_json::json!({
                "version": "v1",
                "degraded": true,
                "tags_dir": "tags",
                "sketches_dir": "sketches",
                "uncategorized_threshold": threshold.value,
                "uncategorized_ratio": uncategorized_ratio(&all_tags),
                "tally": serde_json::Value::Array(Vec::new()),
            });
            let index_path = tags_dir.join("index.json");
            write_json(&index_path, &index)?;
            return Ok(PhaseOutput::Sketches(Vec::new()));
        }

        // Index file: maps each sketch path to its tag path, plus a
        // tally of (sketch path, primary, subcategory). The
        // `uncategorized_threshold` mirrors the value that
        // `TaggerThreshold::from_config_value` actually applied so
        // a downstream phase reading the index sees the effective
        // cutoff (the configured value, or the default if the config
        // field was absent / out-of-range).
        let index: serde_json::Value = serde_json::json!({
            "version": "v1",
            "tags_dir": "tags",
            "sketches_dir": "sketches",
            "uncategorized_threshold": threshold.value,
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
    use crate::execution::Parallelism;
    use crate::fs_layout::MoaganHome;
    use crate::ids::RunId;
    use crate::llm::provider::{Provider, ProviderRegistry};
    use crate::llm::wire::Request;
    use crate::telemetry::Telemetry;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Provider that always returns `Err(Error::Provider(...))` —
    /// the retry-parse layer treats this as a non-retriable error
    /// (it maps to `ErrorCode::InvalidResponse`, outside the
    /// retriable set), so `discover_tag` sees one failure per
    /// sketch and ends up with `kept == Vec::new()`.
    struct AlwaysErrorProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for AlwaysErrorProvider {
        fn name(&self) -> &str {
            "always-error"
        }
        fn model(&self) -> &str {
            "always-error-model"
        }
        fn endpoint(&self) -> &str {
            "mock://always-error"
        }
        async fn send(&self, _req: &Request) -> Result<(u16, crate::llm::wire::Response)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::Provider {
                message: "forced upstream failure".into(),
                http_status: None,
            })
        }
    }

    /// When every tagging call fails, the phase must NOT abort the
    /// run. Instead it writes an empty `tags/index.json` with
    /// `"degraded": true` and returns `Ok(PhaseOutput::Sketches(Vec::new()))`
    /// so the cluster / integrator / summary phases can still
    /// produce useful output. Pinned by the breaker-fix worktree so
    /// a future refactor that re-introduces the hard error surfaces
    /// as a failing test rather than a runtime `Err` aborting the
    /// pipeline.
    #[tokio::test]
    async fn discover_tag_with_zero_successful_tags_returns_degraded_index() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = Arc::new(MoaganHome::at(temp.path().to_path_buf()));
        home.ensure().expect("ensure home");
        let run_id = RunId::new();
        let run_dir = home.run_dir(run_id);
        run_dir.ensure().expect("ensure run dir");

        // Stage one sketch so the empty-sketches short-circuit at
        // the top of `execute` does not fire — we want to exercise
        // the all-tagging-calls-failed branch instead.
        let sketches_dir = run_dir.sketches();
        std::fs::create_dir_all(&sketches_dir).expect("create sketches dir");
        let sketch = Sketch {
            id: "sk_zero_tag_test".into(),
            thesis: "degraded-mode probe sketch".into(),
            key_decisions: vec!["only sketch for this test".into()],
            ..Default::default()
        };
        write_json(&sketches_dir.join("sk_zero_tag_test.json"), &sketch).expect("write sketch");

        let registry = {
            let inner = Arc::new(AlwaysErrorProvider {
                calls: AtomicUsize::new(0),
            });
            let mut r = ProviderRegistry::default();
            r.insert("always-error".into(), inner);
            Arc::new(r)
        };
        let telemetry = Telemetry::open(
            run_id,
            &run_dir,
            crate::redact::RedactPolicy::default(),
            None,
        )
        .expect("telemetry open");
        let ctx = RunContext::new(
            run_id,
            home.clone(),
            registry,
            "always-error".into(),
            "always-error-model".into(),
            Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        );

        let phase = DiscoverTagPhase;
        let result = phase
            .execute(&ctx)
            .await
            .expect("degraded mode must return Ok, never Err");

        // Empty sketches list — the pipeline continues on raw
        // sketches downstream.
        match result {
            PhaseOutput::Sketches(paths) => {
                assert!(
                    paths.is_empty(),
                    "degraded mode must return an empty sketches list, got {paths:?}"
                );
            }
            other => panic!("expected PhaseOutput::Sketches, got {other:?}"),
        }

        // `tags/index.json` must exist with `"degraded": true`. The
        // cluster / integrator / summary phases read this file to
        // decide whether to fall back to the raw-sketch path; an
        // absent or malformed index would break them.
        let tags_dir = run_dir.tags();
        let index_path = tags_dir.join("index.json");
        assert!(
            index_path.exists(),
            "tags/index.json must be written even in degraded mode (path: {index_path:?})"
        );
        let index: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&index_path).expect("read index"))
                .expect("index is valid JSON");
        assert_eq!(
            index.get("degraded").and_then(|v| v.as_bool()),
            Some(true),
            "tags/index.json must carry \"degraded\": true in degraded mode, got {index}"
        );
        assert_eq!(
            index
                .get("tally")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(0),
            "degraded mode must carry an empty tally, got {index}"
        );
    }
}
