//! Synthesize phase. Phase D (V4 §5.13 + T01-06 §8.4).
//!
//! Reads every cluster produced by `ClusterProposalsPhase`, picks the
//! clusters that warrant a synthesis (default: clusters with ≥2
//! members), and asks the `Synthesizer` role to merge each cluster's
//! proposals into one `SynthesizedProposal`. The synthesized proposal
//! then competes against its sources per V4 §5.13; `RankPhase` reads
//! `synthesized/` and folds each `SynthesizedProposal` into the
//! ranking as if it were a normal proposal.
//!
//! The phase never produces a synthesized proposal for a singleton
//! cluster — synthesizing a single source is just a copy and the
//! `integrator` role would add no signal. To force synthesis on a
//! singleton set `force_singletons = true`.
//!
//! Pipeline propagation (V4 §5.13 + T01-06 §8.4): the synthesized
//! proposal "competes" — it passes gates, receives critique, is
//! evaluated, and enters the Pareto front. To make that work with
//! the existing phase pipeline (which iterates over `proposals/*.json`),
//! this phase writes two artifacts per synthesis:
//!
//! 1. `synthesized/s_<NN>.json` — the immutable lineage record
//!    carrying `source_proposals`, `cluster_id`, and `synthesis_strategy`.
//! 2. `proposals/s_<NN>.json` — a copy shaped as a `Proposal` so the
//!    downstream phases (`Gate`, `Critique`, `Repair`, `Judge`,
//!    `Rank`, `Deliver`) treat it like any other proposal.
//!
//! The `s_` prefix avoids collision with `p_<NN>` ids in `proposals/`
//! and lets `DeliverPhase` badge these as "synthesis" entries.

#[cfg(test)]
use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use futures::future::join_all;

use crate::domain::constraint::{
    HARD_INCOMPATIBILITIES, HardIncompat, detect_opt_in_hardincompat, find_conflicts,
};
use crate::domain::{MergePlan, Proposal, SynthesizedProposal};
use crate::error::Result;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::cluster_proposals::ProposalCluster;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};
use crate::preferences::integration as prefs_integration;
use crate::time::now_unix_secs;

/// Cheap whole-word substring check used by `extract_tags`. Returns
/// `true` when `tag` appears in `text` delimited by a non-alphanumeric
/// boundary on both sides (or at a string boundary). `text` is
/// expected to be lowercase and `tag` is matched lowercase.
fn word_contains(text: &str, tag: &str) -> bool {
    if tag.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len();
    let mut start = 0;
    while start + tag_len <= bytes.len() {
        // Fast substring search first.
        if &bytes[start..start + tag_len] != tag_bytes {
            start += 1;
            continue;
        }
        // Check the left boundary: non-alphanumeric or string start.
        let left_ok = start == 0 || !is_alnum(bytes[start - 1]);
        // Check the right boundary: non-alphanumeric or string end.
        let right_idx = start + tag_len;
        let right_ok = right_idx == bytes.len() || !is_alnum(bytes[right_idx]);
        if left_ok && right_ok {
            return true;
        }
        start += 1;
    }
    false
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Convert a `SynthesizedProposal` into a `Proposal` for the pipeline.
/// The synthesized proposal keeps its `s_<NN>` id and inherits the
/// approach / summary / tradeoffs / evidence from the synthesizer.
/// `source_sketch` records the cluster the synthesis came from so
/// later phases can reconstruct the lineage if they need to.
pub fn synth_to_proposal(synth: &SynthesizedProposal) -> Proposal {
    Proposal {
        id: synth.id.clone(),
        summary: synth.summary.clone(),
        approach: synth.approach.clone(),
        tradeoffs: synth.tradeoffs.clone(),
        evidence: synth.evidence.clone(),
        source_sketch: format!("syn_from_{}", synth.cluster_id),
        artifacts: Vec::new(),
        replaced_by: None,
        source_nodes: Vec::new(),
    }
}

/// Convert a `MergePlan` (the new MergeSynthesizer role output) into
/// the existing `SynthesizedProposal` shape that the downstream
/// pipeline already understands. The two structures are equivalent
/// modulo the richer `hard_constraint_check` field on the plan,
/// which we keep on the plan and surface as an extra `evidence`
/// line on the proposal (the schema for downstream phases doesn't
/// need the structured map).
pub fn merge_plan_to_synthesized(
    plan: MergePlan,
    cluster: &crate::phases::cluster_proposals::ProposalCluster,
    target_id: &str,
) -> SynthesizedProposal {
    let now = now_unix_secs();
    let sources: Vec<String> = if plan.sources.is_empty() {
        cluster.member_proposals.clone()
    } else {
        plan.sources
    };
    let evidence = if plan.hard_constraint_check.is_empty() {
        plan.evidence
    } else {
        let mut evidence = plan.evidence;
        let hard = plan
            .hard_constraint_check
            .iter()
            .map(|(k, ok)| format!("hard:{}={}", k, ok))
            .collect::<Vec<_>>()
            .join(", ");
        evidence.push(format!("hard_constraints[{hard}]"));
        evidence
    };
    SynthesizedProposal {
        id: target_id.to_string(),
        source_proposals: cluster.member_proposals.clone(),
        cluster_id: cluster.id.clone(),
        synthesis_strategy: "merge_invariants".into(),
        summary: plan.summary,
        approach: plan.approach,
        tradeoffs: plan.tradeoffs,
        evidence,
        sources,
        created_unix: now,
        schema_version: "v1".into(),
    }
}

/// Synthesize phase. For each cluster with ≥2 members, calls the
/// `synthesizer` role to merge the cluster's proposals.
pub struct SynthesizePhase {
    /// Minimum cluster size that triggers synthesis. Default 2 —
    /// synthesizing a single source has no informational value.
    pub min_cluster_size: usize,
    /// Force synthesis on singleton clusters (mostly for tests).
    pub force_singletons: bool,
}

impl Default for SynthesizePhase {
    fn default() -> Self {
        Self {
            min_cluster_size: 2,
            force_singletons: false,
        }
    }
}

impl SynthesizePhase {
    /// Build the LLM user payload. The synthesizer receives the
    /// cluster's proposals plus its id and the target `s_<NN>` it
    /// must reuse.
    fn user_payload(cluster_id: &str, target_id: &str, proposals: &[Proposal]) -> String {
        let proposals_json = serde_json::to_string(proposals).unwrap_or_else(|_| "[]".to_owned());
        format!(
            "Cluster id: {cluster_id}\n\
             Target synthesized id: {target_id}\n\n\
             Source proposals (the cluster's members):\n\n\
             {proposals_json}\n\n\
             Return the JSON object described in the system prompt.",
        )
    }

    /// Apply the opt-in preference injection to `system`. When
    /// `MOAGAN_LEARNING=true` AND `MOAGAN_USER` is set, any
    /// `${epistemic_preferences}` placeholder embedded in
    /// `system` is replaced with the user's top-3 ratings. In
    /// every other case (disabled loop, missing user, empty
    /// cache, missing placeholder) `system` is returned
    /// unchanged so anonymous / opted-out runs see no
    /// substitution. PR D.8.
    pub fn prepare_system_prompt(system: String) -> String {
        prefs_integration::inject_preferences_into_prompt(&system)
    }

    /// Extract the synthesised proposal ids from the file paths
    /// the phase writes. Each path looks like
    /// `<run_dir>/synthesized/s_<NN>.json`; the file stem (the
    /// part before `.json`) is the proposal id we feed into
    /// [`prefs_integration::auto_record_run`] at phase completion.
    /// PR D.8.
    pub fn proposal_ids_from_paths(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
            .collect()
    }

    /// Extract candidate architectural tags from a proposal's textual
    /// fields. The match is a whole-word, case-insensitive scan over
    /// the summary, approach, tradeoffs, and evidence so we never miss
    /// a tag the model wrote in the body. Returns deduplicated tags
    /// preserving first-seen order.
    ///
    /// Tag sources:
    ///
    /// 1. Every literal in [`HARD_INCOMPATIBILITIES`] — the
    ///    §D.13.15 matrix that drives the tag-pair detector.
    /// 2. The opt-in catalog markers consumed by
    ///    [`crate::domain::constraint::detect_opt_in_hardincompat`]
    ///    (`cluster_local`, `global`, `pull_based`,
    ///    `pull_required`, `push_only`, `push_endpoint`,
    ///    `stateless`, `stateful_required`). They are not in the
    ///    matrix but the opt-in detectors still need to see them
    ///    when scanning a proposal's body. Without this list, an
    ///    opt-in pair like `cluster_local` + `global` would never
    ///    be extracted because neither literal appears in
    ///    `HARD_INCOMPATIBILITIES`.
    ///
    /// The opt-in marker list is kept in this module (not in
    /// `constraint.rs`) so the conflict module stays free of
    /// phase-specific tag-extraction knowledge — the phase owns
    /// the "scan a proposal's body" responsibility.
    pub fn extract_tags(proposal: &Proposal) -> Vec<String> {
        // Build the search corpus: every public text field on the
        // proposal. Tradeoffs and evidence are joined so a tag like
        // "sql" listed in the evidence array is found.
        let mut corpus = String::new();
        corpus.push_str(&proposal.summary);
        corpus.push('\n');
        corpus.push_str(&proposal.approach);
        corpus.push('\n');
        for t in &proposal.tradeoffs {
            corpus.push_str(t);
            corpus.push('\n');
        }
        for e in &proposal.evidence {
            corpus.push_str(e);
            corpus.push('\n');
        }
        let corpus_lower = corpus.to_lowercase();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<String> = Vec::new();
        // Source 1: §D.13.15 matrix literals.
        for (a, b) in HARD_INCOMPATIBILITIES {
            for tag in [*a, *b] {
                if !seen.contains(tag) && word_contains(&corpus_lower, tag) {
                    seen.insert(tag.to_string());
                    out.push(tag.to_string());
                }
            }
        }
        // Source 2: catalog I.6 (opt-in) marker literals. The
        // list is exhaustive against the three opt-in
        // detectors so a future variant addition must update
        // both this list and the corresponding `detect_*`
        // helper in `constraint.rs`.
        const OPT_IN_MARKERS: &[&str] = &[
            "cluster_local",
            "global",
            "pull_based",
            "pull_required",
            "push_only",
            "push_endpoint",
            "stateless",
            "stateful_required",
        ];
        for tag in OPT_IN_MARKERS {
            if !seen.contains(*tag) && word_contains(&corpus_lower, tag) {
                seen.insert((*tag).to_string());
                out.push((*tag).to_string());
            }
        }
        out
    }

    /// Detect incompatible tag pairs across the cluster's proposals.
    /// Returns the offending `(tag_a, tag_b)` pair (first match
    /// wins) plus the full tag list collected from every proposal.
    pub fn cluster_conflict(proposals: &[Proposal]) -> Option<(String, String, Vec<String>)> {
        let mut all_tags: Vec<String> = Vec::new();
        for p in proposals {
            all_tags.extend(Self::extract_tags(p));
        }
        let borrowed: Vec<&str> = all_tags.iter().map(String::as_str).collect();
        if let Some((a, b)) = find_conflicts(&borrowed).into_iter().next() {
            Some((a.to_string(), b.to_string(), all_tags))
        } else {
            None
        }
    }

    /// Catalog I.6 (opt-in) detector wiring: run the three new
    /// [`HardIncompat`] detectors (`ClusterLocalInGlobal`,
    /// `PullInPushOnly`, `StatelessInStateful`) on the flattened
    /// tag set of every proposal in the cluster. Returns the
    /// first matched typed record, or `None` when none of the
    /// opt-in heuristics fire — the caller should then fall
    /// through to [`Self::cluster_conflict`] which checks the
    /// §D.13.15 tag-pair matrix.
    ///
    /// The detection is additive: a cluster that already tripped
    /// the tag-pair matrix still gets reported via the older path
    /// so the wire form (`incompatible_tags: a,b`) is preserved.
    /// The opt-in path returns a typed record whose
    /// [`HardIncompat::explain`] message is stable enough to
    /// surface in the JSON sidecar.
    pub fn cluster_opt_in_hardincompat(
        proposals: &[Proposal],
    ) -> Option<(HardIncompat, Vec<String>)> {
        let mut all_tags: Vec<String> = Vec::new();
        for p in proposals {
            all_tags.extend(Self::extract_tags(p));
        }
        let borrowed: Vec<&str> = all_tags.iter().map(String::as_str).collect();
        detect_opt_in_hardincompat(&borrowed).map(|h| (h, all_tags))
    }

    /// Persist a `synthesized/skipped_<NN>.json` sidecar in `dir`.
    /// This is the canonical filesystem-first write; the caller is
    /// expected to mirror the row into SQLite afterwards.
    fn write_skipped_in_dir(
        dir: &std::path::Path,
        cluster_id: &str,
        skipped_seq: usize,
        conflict: &(String, String, Vec<String>),
    ) -> Result<PathBuf> {
        #[derive(serde::Serialize)]
        struct SkippedCluster {
            cluster_id: String,
            skipped: bool,
            reason: String,
            tags: Vec<String>,
            schema_version: String,
        }
        let (a, b, tags) = conflict;
        let payload = SkippedCluster {
            cluster_id: cluster_id.to_string(),
            skipped: true,
            reason: format!("incompatible_tags: {a},{b}"),
            tags: tags.clone(),
            schema_version: "v1".into(),
        };
        let bytes = serde_json::to_vec_pretty(&payload)?;
        let path = dir.join(format!("skipped_{:02}.json", skipped_seq));
        crate::atomic::writer::AtomicWriter::new().write(&path, &bytes)?;
        Ok(path)
    }

    /// Catalog I.6 (opt-in) variant of
    /// [`Self::write_skipped_in_dir`]: persists a sidecar whose
    /// `reason` carries the typed [`HardIncompat`] variant name
    /// (rather than the raw tag pair). The variant's
    /// [`HardIncompat::explain`] message is serialised alongside
    /// the kind tag so downstream readers can render the
    /// human-readable description without re-deriving it. Same
    /// wire shape as the matrix-driven writer — only the
    /// `reason` and the extra `hard_incompat_kind` /
    /// `hard_incompat_explain` fields differ.
    fn write_skipped_opt_in_in_dir(
        dir: &std::path::Path,
        cluster_id: &str,
        skipped_seq: usize,
        hardincompat: &HardIncompat,
        tags: &[String],
    ) -> Result<PathBuf> {
        #[derive(serde::Serialize)]
        struct SkippedClusterOptIn {
            cluster_id: String,
            skipped: bool,
            reason: String,
            hard_incompat_kind: String,
            hard_incompat_explain: String,
            tags: Vec<String>,
            schema_version: String,
        }
        let kind = match hardincompat {
            HardIncompat::ClusterLocalInGlobal => "cluster_local_in_global",
            HardIncompat::PullInPushOnly => "pull_in_push_only",
            HardIncompat::StatelessInStateful => "stateless_in_stateful",
            // The opt-in writer is private to this module and is
            // only called from the opt-in branch; a future enum
            // variant added here must update the match above.
            _ => unreachable!("write_skipped_opt_in_in_dir called for non opt-in variant"),
        };
        let payload = SkippedClusterOptIn {
            cluster_id: cluster_id.to_string(),
            skipped: true,
            reason: format!("hard_incompat: {kind}"),
            hard_incompat_kind: kind.to_string(),
            hard_incompat_explain: hardincompat.explain(),
            tags: tags.to_vec(),
            schema_version: "v1".into(),
        };
        let bytes = serde_json::to_vec_pretty(&payload)?;
        let path = dir.join(format!("skipped_{:02}.json", skipped_seq));
        crate::atomic::writer::AtomicWriter::new().write(&path, &bytes)?;
        Ok(path)
    }

    /// Read every `cluster_proposals/cp_*.json` from disk.
    fn load_clusters(ctx: &RunContext) -> Result<Vec<ProposalCluster>> {
        let dir = ctx.run_dir().cluster_proposals_dir();
        let mut out: Vec<ProposalCluster> = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(out),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            match read_json::<ProposalCluster>(&path) {
                Ok(c) => out.push(c),
                Err(_) => continue, // skip malformed files
            }
        }
        Ok(out)
    }

    /// Load every proposal referenced by a cluster. Mirrors the
    /// revision-aware lookup in `ClusterProposalsPhase::load_proposals`
    /// so synthesis sees the latest repaired version.
    fn load_proposals_for_cluster(ctx: &RunContext, ids: &[String]) -> Result<Vec<Proposal>> {
        let proposals_dir = ctx.run_dir().proposals();
        let revisions_dir = ctx.run_dir().revisions();
        let mut out: Vec<Proposal> = Vec::with_capacity(ids.len());
        for id in ids {
            let mut picked: Option<Proposal> = None;
            for n in (0..16).rev() {
                let rev_path: PathBuf = revisions_dir.join(format!("{id}_rev_{n}.json"));
                if rev_path.exists()
                    && let Ok(p) = read_json::<Proposal>(&rev_path)
                {
                    picked = Some(p);
                    break;
                }
            }
            let proposal = match picked {
                Some(p) => p,
                None => read_json::<Proposal>(&proposals_dir.join(format!("{id}.json")))?,
            };
            out.push(proposal);
        }
        Ok(out)
    }
}

#[async_trait]
impl Phase for SynthesizePhase {
    fn name(&self) -> &'static str {
        "synthesize"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        tracing::debug!(
            min_cluster_size = self.min_cluster_size,
            force_singletons = self.force_singletons,
            "synthesize: enter"
        );
        let dir = ctx.run_dir().synthesized();
        std::fs::create_dir_all(&dir)?;
        let dir = std::sync::Arc::new(dir);

        // F3: budget gate. The synthesis merge is an LLM call
        // per cluster — the most expensive optional work in the
        // pipeline. When the budget observer reports Hard
        // pressure + Reduce policy, skip the merge entirely so
        // the remaining budget is reserved for the core
        // pipeline. The empty-eligible fast-path below already
        // covers runs that have nothing to merge; this gate
        // covers runs that *do* have clusters but cannot afford
        // to synthesise them.
        let budget_skip_synthesis = ctx
            .telemetry
            .db()
            .map(|db| {
                crate::phases::budget::BudgetObserver::new(db.clone(), ctx.run_id)
                    .should_skip_optional()
            })
            .transpose()?
            .unwrap_or(false);
        if budget_skip_synthesis {
            tracing::info!(
                run_id = %ctx.run_id,
                stage = "synthesize.skipped",
                reason = "budget_hard",
                "synthesize phase skipped: budget under Hard pressure"
            );
            return Ok(PhaseOutput::Synthesized(Vec::new()));
        }

        let clusters = Self::load_clusters(ctx)?;
        let eligible: Vec<&ProposalCluster> = clusters
            .iter()
            .filter(|c| self.force_singletons || c.member_proposals.len() >= self.min_cluster_size)
            .collect();

        if eligible.is_empty() {
            tracing::info!(
                cluster_count = clusters.len(),
                "synthesize: no eligible clusters, returning empty"
            );
            // PR D.8: even on the empty-eligible fast-path, fire
            // the auto-record so a run that synthesised zero
            // proposals still gets a neutral entry per portfolio
            // proposal. The caller supplies `run_id`; we extract
            // portfolio ids from the proposals dir so the learning
            // loop keeps observing every shipped proposal.
            let portfolio_ids = portfolio_proposal_ids(&ctx.run_dir().proposals());
            if !portfolio_ids.is_empty() {
                Self::record_phase_outcome(ctx.run_id, &portfolio_ids);
            }
            return Ok(PhaseOutput::Synthesized(Vec::new()));
        }
        tracing::info!(
            eligible_count = eligible.len(),
            "synthesize: eligible clusters identified"
        );

        let futures = eligible.iter().enumerate().map(|(idx, cluster)| {
            let cluster: ProposalCluster = (*cluster).clone();
            let ctx = ctx.clone();
            let dir = std::sync::Arc::clone(&dir);
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let target_id = format!("s_{:02}", idx);
                let proposals =
                    SynthesizePhase::load_proposals_for_cluster(&ctx, &cluster.member_proposals)?;
                if proposals.is_empty() {
                    return Ok::<Option<PathBuf>, crate::error::Error>(None);
                }
                // K.1 (proposal-03 §D.13.15): skip clusters whose
                // proposals mix hard-incompatible tags. The synthesizer
                // LLM would otherwise be asked to merge contradictory
                // decisions (e.g. monolith + microservices) which
                // produces incoherent output.
                if let Some(conflict) = SynthesizePhase::cluster_conflict(&proposals) {
                    let (a, b, _tags) = &conflict;
                    tracing::warn!(
                        cluster_id = %cluster.id,
                        tag_a = %a,
                        tag_b = %b,
                        "synthesize phase skipping cluster: incompatible tags"
                    );
                    let skipped_path = SynthesizePhase::write_skipped_in_dir(
                        &dir,
                        &cluster.id,
                        idx,
                        &conflict,
                    )?;
                    return Ok(Some(skipped_path));
                }
                // Catalog I.6 (opt-in) follow-up: the three new
                // `HardIncompat` variants (`ClusterLocalInGlobal`,
                // `PullInPushOnly`, `StatelessInStateful`) ride
                // the same skip-the-merge path but with a typed
                // reason so the sidecar carries the variant
                // name + `explain()` message instead of a raw
                // tag pair. The matrix-driven check above stays
                // first so the legacy `incompatible_tags: a,b`
                // wire form is preserved for the existing
                // dashboard analytics; this branch is purely
                // additive.
                if let Some((hardincompat, tags)) =
                    SynthesizePhase::cluster_opt_in_hardincompat(&proposals)
                {
                    tracing::warn!(
                        cluster_id = %cluster.id,
                        hard_incompat_kind = %hardincompat.explain(),
                        "synthesize phase skipping cluster: opt-in hard incompat"
                    );
                    let skipped_path = SynthesizePhase::write_skipped_opt_in_in_dir(
                        &dir,
                        &cluster.id,
                        idx,
                        &hardincompat,
                        &tags,
                    )?;
                    return Ok(Some(skipped_path));
                }
                let user = SynthesizePhase::user_payload(
                    &cluster.id,
                    &target_id,
                    &proposals,
                );
                // V1: route the intra-cluster merge through the
                // catalog role `MergeSynthesizer` (D.7.1) instead
                // of the legacy `Synthesizer`. The new role returns
                // a `MergePlan` with a stricter schema (sources
                // array, hard_constraint_check, evidence per
                // source); the phase converts it to the
                // `SynthesizedProposal` shape the downstream
                // pipeline already consumes.
                //
                // PR D.8: pass the system prompt through the
                // preference injector so the `${epistemic_preferences}`
                // placeholder in `merge_synthesizer.md` is replaced
                // with the user's top-3 ratings when the loop is
                // enabled.
                let raw_system = system_prompt(Role::MergeSynthesizer).to_owned();
                let system = SynthesizePhase::prepare_system_prompt(raw_system);
                let plan: MergePlan = ctx
                    .call_with_retry_parse(
                        Role::MergeSynthesizer,
                        system,
                        user,
                        "MergePlan: {summary, approach, tradeoffs[], evidence[], sources[], hard_constraint_check{...}, expected_validation}",
                        3,
                    )
                    .await?;
                let parsed = merge_plan_to_synthesized(plan, &cluster, &target_id);
                let path = dir.join(format!("{}.json", parsed.id));
                write_json(&path, &parsed)?;

                // Phase D propagation (V4 §5.13 + T01-06 §8.4):
                // also drop a copy into `proposals/` shaped as a
                // `Proposal` so the downstream Gate / Critique /
                // Repair / Judge / Rank / Deliver phases pick the
                // synthesis up and it enters the Pareto front.
                let proposal = synth_to_proposal(&parsed);
                let prop_path = ctx
                    .run_dir()
                    .proposals()
                    .join(format!("{}.json", proposal.id));
                write_json(&prop_path, &proposal)?;

                Ok(Some(path))
            }
        });

        let results = join_all(futures).await;
        let mut paths: Vec<PathBuf> = Vec::new();
        for r in results {
            match r {
                Ok(Some(p)) => paths.push(p),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, "synthesize: future failed");
                    return Err(e);
                }
            }
        }
        tracing::info!(
            syntheses_written = paths.len(),
            "synthesize: phase complete"
        );

        // PR D.8: on phase completion, fire the auto-record for
        // the synthesised proposals. Every synthesised `s_<NN>`
        // (already mirrored to `proposals/` above) becomes a
        // neutral `score = 0.5` rating when the learning loop is
        // opted in; an opted-out or missing-user run is a no-op
        // (the helper returns silently).
        let proposal_ids = Self::proposal_ids_from_paths(&paths);
        Self::record_phase_outcome(ctx.run_id, &proposal_ids);

        Ok(PhaseOutput::Synthesized(paths))
    }
}

impl SynthesizePhase {
    /// PR D.8 — fire the auto-record once the phase completes.
    /// Reads `MOAGAN_USER`; no-op when unset or when the
    /// learning loop is opted out (the integration helper
    /// short-circuits internally).
    fn record_phase_outcome(run_id: crate::ids::RunId, proposal_ids: &[String]) {
        if proposal_ids.is_empty() {
            return;
        }
        let Ok(user) = std::env::var("MOAGAN_USER") else {
            return;
        };
        if user.is_empty() {
            return;
        }
        prefs_integration::auto_record_run(&user, run_id, proposal_ids);
    }
}

/// Collect the proposal ids present in `<run_dir>/proposals/` at
/// the moment the synthesize phase finishes. Used by the
/// no-eligible-clusters fast-path so the learning loop still
/// observes a neutral rating for every shipped proposal. PR D.8.
fn portfolio_proposal_ids(proposals_dir: &std::path::Path) -> Vec<String> {
    let entries = match std::fs::read_dir(proposals_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.ends_with(".json") || name.ends_with(".meta.json") {
                return None;
            }
            name.strip_suffix(".json").map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RunId;

    #[test]
    fn default_min_cluster_size_is_two() {
        let phase = SynthesizePhase::default();
        assert_eq!(phase.min_cluster_size, 2);
        assert!(!phase.force_singletons);
    }

    #[test]
    fn user_payload_contains_target_and_cluster() {
        let p = Proposal::default();
        let s = SynthesizePhase::user_payload("cp_00", "s_00", &[p]);
        assert!(s.contains("cp_00"));
        assert!(s.contains("s_00"));
    }

    #[test]
    fn synth_to_proposal_preserves_id() {
        let s = SynthesizedProposal {
            id: "s_07".into(),
            cluster_id: "cp_03".into(),
            summary: "summary text".into(),
            approach: "## Approach\n\nbody".into(),
            tradeoffs: vec!["t1".into()],
            evidence: vec!["sk_001".into()],
            ..Default::default()
        };
        let p = synth_to_proposal(&s);
        assert_eq!(p.id, "s_07");
    }

    #[test]
    fn synth_to_proposal_preserves_fields() {
        let s = SynthesizedProposal {
            id: "s_00".into(),
            cluster_id: "cp_00".into(),
            summary: "s".into(),
            approach: "a".into(),
            tradeoffs: vec!["t".into()],
            evidence: vec!["e".into()],
            ..Default::default()
        };
        let p = synth_to_proposal(&s);
        assert_eq!(p.summary, "s");
        assert_eq!(p.approach, "a");
        assert_eq!(p.tradeoffs, vec!["t".to_string()]);
        assert_eq!(p.evidence, vec!["e".to_string()]);
        assert!(p.artifacts.is_empty());
    }

    #[test]
    fn synth_to_proposal_records_source_cluster() {
        let s = SynthesizedProposal {
            id: "s_02".into(),
            cluster_id: "cp_99".into(),
            ..Default::default()
        };
        let p = synth_to_proposal(&s);
        assert_eq!(p.source_sketch, "syn_from_cp_99");
    }

    /// V1: `merge_plan_to_synthesized` carries the MergePlan fields
    /// forward and merges `hard_constraint_check` into the evidence
    /// stream so downstream phases (Gate, Critique, etc.) can still
    /// see why a synthesis was rejected.
    #[test]
    fn merge_plan_to_synthesized_carries_fields_and_hard_constraints() {
        let plan = MergePlan {
            summary: "summary text".into(),
            approach: "## Approach\n\nbody".into(),
            tradeoffs: vec!["t1".into()],
            evidence: vec!["sk_001".into()],
            sources: vec!["p_001".into(), "p_002".into()],
            hard_constraint_check: BTreeMap::from([("single_binary".into(), true)]),
            expected_validation: "unit tests pass".into(),
            ..MergePlan::default()
        };
        let cluster = crate::phases::cluster_proposals::ProposalCluster {
            schema_version: "v1".into(),
            id: "cp_00".into(),
            member_proposals: vec!["p_001".into(), "p_002".into()],
            cluster_text_sample: String::new(),
            created_unix: 0,
        };
        let _ = cluster;
        let s = merge_plan_to_synthesized(plan, &cluster, "s_00");
        assert_eq!(s.id, "s_00");
        assert_eq!(s.summary, "summary text");
        assert_eq!(s.sources, vec!["p_001".to_string(), "p_002".into()]);
        assert_eq!(s.cluster_id, "cp_00");
        assert_eq!(s.synthesis_strategy, "merge_invariants");
        // hard_constraint_check is surfaced as a single extra
        // evidence line so the downstream pipeline can show it.
        assert!(
            s.evidence.iter().any(|e| e.contains("hard_constraints[")),
            "evidence should carry hard_constraints line, got {:?}",
            s.evidence
        );
    }

    /// PR D.8: `proposal_ids_from_paths` strips the `.json`
    /// extension off each synthesised path so the result feeds
    /// cleanly into `auto_record_run`. The list must be in input
    /// order so the rating log preserves the synthesis index.
    #[test]
    fn synthesize_proposal_ids_from_paths_strips_extension() {
        let paths = vec![
            PathBuf::from("/tmp/run/synthesized/s_00.json"),
            PathBuf::from("/tmp/run/synthesized/s_01.json"),
            PathBuf::from("/tmp/run/synthesized/s_02.json"),
        ];
        let ids = SynthesizePhase::proposal_ids_from_paths(&paths);
        assert_eq!(
            ids,
            vec!["s_00".to_string(), "s_01".to_string(), "s_02".to_string()]
        );
    }

    /// PR D.8: an empty path list (no eligible clusters) must
    /// produce an empty id list so the auto-record call is a
    /// no-op and never persists a phantom rating.
    #[test]
    fn synthesize_proposal_ids_from_empty_paths_is_empty() {
        let ids = SynthesizePhase::proposal_ids_from_paths(&[]);
        assert!(ids.is_empty());
    }

    /// PR D.8: `merge_synthesizer.md` must embed the
    /// `${epistemic_preferences}` placeholder so the
    /// preference injector has something to substitute. The
    /// placeholder is intentionally placed at the top of the
    /// prompt (right under the role header) so the
    /// synthesised LLM sees the user's prior ratings before
    /// any cluster proposal.
    #[test]
    fn synthesize_prompt_substitutes_preferences_placeholder() {
        // Use the same path the binary embeds via
        // `include_str!`; assert it carries the placeholder.
        const MERGE_SYNTHESIZER_PROMPT: &str = include_str!("../llm/prompts/merge_synthesizer.md");
        assert!(
            MERGE_SYNTHESIZER_PROMPT
                .contains(crate::llm::prompts::EPISTEMIC_PREFERENCES_PLACEHOLDER),
            "merge_synthesizer.md must embed the placeholder, got: {MERGE_SYNTHESIZER_PROMPT:?}"
        );

        // And the phase-level wrapper must substitute it once
        // `MOAGAN_LEARNING=true` and `MOAGAN_USER` are set and
        // the cache has at least one rating.
        let _g = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let (_keep, tmp) = unique_tmp_dir("synth_placeholder");
        unsafe {
            std::env::set_var("MOAGAN_HOME", &tmp);
            std::env::set_var("MOAGAN_LEARNING", "true");
            std::env::set_var("MOAGAN_USER", "alice");
        }
        let mut cache = crate::preferences::PreferenceCache::load("alice");
        cache.add(crate::preferences::cache::Rating {
            proposal_id: "p_alpha".into(),
            score: 0.8,
            rated_unix: crate::preferences::cache::unix_now(),
            run_id: crate::ids::RunId::new(),
        });
        cache.save().unwrap();

        let prepared = SynthesizePhase::prepare_system_prompt(MERGE_SYNTHESIZER_PROMPT.to_owned());
        assert!(
            !prepared.contains(crate::llm::prompts::EPISTEMIC_PREFERENCES_PLACEHOLDER),
            "placeholder must be replaced, got: {prepared:?}"
        );
        assert!(
            prepared.contains("p_alpha"),
            "rendered block must include the prior rating id, got: {prepared:?}"
        );

        unsafe {
            std::env::set_var("MOAGAN_LEARNING", prev_learning.unwrap_or_default());
            std::env::set_var("MOAGAN_HOME", prev_home.unwrap_or_default());
            std::env::set_var("MOAGAN_USER", prev_user.unwrap_or_default());
        }
        // `_keep` (TempDir) drops at end of test → dir cleaned up.
    }

    /// PR D.8: when the learning loop is opted out, the prompt
    /// is returned unchanged — no header, no placeholder
    /// leakage, no synthetic content.
    #[test]
    fn synthesize_prompt_returns_unchanged_when_disabled() {
        let _g = crate::TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let (_keep, tmp) = unique_tmp_dir("synth_disabled");
        unsafe {
            std::env::set_var("MOAGAN_HOME", &tmp);
            std::env::set_var("MOAGAN_LEARNING", "false");
            std::env::set_var("MOAGAN_USER", "alice");
        }

        let prompt = "Hello\n${epistemic_preferences}\nWorld";
        let out = SynthesizePhase::prepare_system_prompt(prompt.to_owned());
        assert_eq!(out, prompt, "disabled loop must yield unchanged prompt");

        unsafe {
            std::env::set_var("MOAGAN_LEARNING", prev_learning.unwrap_or_default());
            std::env::set_var("MOAGAN_HOME", prev_home.unwrap_or_default());
            std::env::set_var("MOAGAN_USER", prev_user.unwrap_or_default());
        }
        // `_keep` (TempDir) drops at end of test → dir cleaned up.
    }

    /// PR D.8: `portfolio_proposal_ids` reads every
    /// `<run_dir>/proposals/<id>.json` and returns the ids
    /// sorted so the auto-record call observes a stable
    /// ordering regardless of the on-disk layout.
    #[test]
    fn synthesize_portfolio_ids_are_sorted_and_skip_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("proposals")).unwrap();
        for id in ["s_03", "s_01", "s_02"] {
            std::fs::write(
                dir.path().join("proposals").join(format!("{id}.json")),
                b"{}",
            )
            .unwrap();
        }
        // Meta sidecars are produced by AtomicWriter and must
        // be ignored.
        std::fs::write(
            dir.path().join("proposals").join("s_01.json.meta.json"),
            b"{}",
        )
        .unwrap();

        let ids = portfolio_proposal_ids(&dir.path().join("proposals"));
        assert_eq!(
            ids,
            vec!["s_01".to_string(), "s_02".to_string(), "s_03".to_string()]
        );
    }

    /// F3: when the budget is Hard under the Reduce policy, the
    /// synthesize phase must skip the merge entirely — no
    /// `synthesized/s_<NN>.json` is written, and the phase
    /// returns an empty `Synthesized(paths)`. The clusters
    /// input is staged as two proposals that *would* trigger
    /// synthesis, so the test pins that the gate fires before
    /// the per-cluster work loop.
    #[test]
    fn synthesize_phase_skips_rationale_under_hard_budget() -> Result<()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = match ENV_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MOAGAN_HOME", tmp.path());
        }
        let home = std::sync::Arc::new(crate::fs_layout::MoaganHome::resolve()?);
        home.ensure()?;
        let run_id = RunId::new();

        // Real Db + hard budget.
        let db = crate::storage::sqlite::Db::open(&home.meta_db_path())?;
        db.register_run(run_id, "fast", "running", "0.4.0", None, None, None)?;
        db.set_budget(run_id, 1000)?;
        db.budget_record(run_id, "seed", 950)?;

        // Stage a two-proposal cluster that would otherwise
        // trigger synthesis (min_cluster_size default is 2).
        let cluster_dir = home.run_dir(run_id).cluster_proposals_dir();
        std::fs::create_dir_all(&cluster_dir)?;
        let cluster = ProposalCluster {
            schema_version: "v1".into(),
            id: "cp_00".into(),
            member_proposals: vec!["p_a".into(), "p_b".into()],
            cluster_text_sample: String::new(),
            created_unix: 0,
        };
        crate::phases::util::write_json(&cluster_dir.join("cp_00.json"), &cluster)?;

        // Stage the two source proposals so the merge would
        // have data to consume (not strictly required for the
        // gate, but documents the full happy path the gate
        // bypasses).
        for id in ["p_a", "p_b"] {
            let path = home.run_dir(run_id).proposals().join(format!("{id}.json"));
            std::fs::create_dir_all(path.parent().unwrap())?;
            crate::phases::util::write_json(
                &path,
                &Proposal {
                    id: id.into(),
                    summary: format!("{id} summary"),
                    ..Proposal::default()
                },
            )?;
        }

        // Db-backed Telemetry so the gate fires.
        let run_dir = home.run_dir(run_id);
        let telemetry = crate::telemetry::Telemetry::open(
            run_id,
            &run_dir,
            crate::redact::RedactPolicy::default(),
            Some(db.clone()),
        )?;

        let ctx = RunContext::new(
            run_id,
            home.clone(),
            std::sync::Arc::new(crate::llm::ProviderRegistry::default()),
            "mock".into(),
            "mock-model".into(),
            crate::execution::Parallelism::new(1),
            telemetry,
            String::new(),
            "fast".into(),
        )
        .with_interactive(false);

        let phase = SynthesizePhase::default();
        let out = pollster::block_on(phase.execute(&ctx))?;
        match out {
            PhaseOutput::Synthesized(paths) => assert!(
                paths.is_empty(),
                "synthesize must produce zero paths under hard budget; got {paths:?}"
            ),
            other => panic!("synthesize must return Synthesized; got {other:?}"),
        }
        // The synthesized dir must not have any `s_*.json`
        // sidecar (the gate returned early before the merge
        // loop).
        let synth_dir = home.run_dir(run_id).synthesized();
        let written: Vec<_> = std::fs::read_dir(&synth_dir)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let synth_files: Vec<_> = written
            .iter()
            .filter(|n| n.starts_with("s_") && n.ends_with(".json") && !n.ends_with(".meta.json"))
            .collect();
        assert!(
            synth_files.is_empty(),
            "no s_<NN>.json must be written under hard budget; got {synth_files:?}"
        );
        Ok(())
    }

    /// Local helper: allocate a fresh `(TempDir, path)` pair for a
    /// test that needs to mutate `MOAGAN_HOME` without acquiring
    /// the global lock used by `with_moagan_home`.
    ///
    /// Returning the [`tempfile::TempDir`] alongside the path
    /// forces the caller to bind it so the directory outlives the
    /// test scope (and is auto-removed by `Drop` on success or
    /// panic — no `/tmp/moagan-*` leak).
    fn unique_tmp_dir(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix(&format!("moagan-synth-test-{tag}-"))
            .tempdir()
            .expect("tmp dir");
        let path = tmp.path().to_path_buf();
        (tmp, path)
    }

    /// Catalog I.6 (opt-in) detector wiring: end-to-end test
    /// that `cluster_opt_in_hardincompat` runs on a flat list of
    /// synthetic proposals whose text contains the opt-in tag
    /// pair, and returns the typed `HardIncompat` variant the
    /// phase uses to skip the merge.
    ///
    /// The proposals here are deliberately constructed so
    /// [`Self::extract_tags`] (which is the whole-word, lowercase
    /// scan over `summary + approach + tradeoffs + evidence`) can
    /// recover the trigger tags. The tags `cluster_local` and
    /// `global` are not in `HARD_INCOMPATIBILITIES` so the
    /// matrix-driven `cluster_conflict` returns `None` — the
    /// opt-in detector must do all the work.
    #[test]
    fn synthesize_cluster_opt_in_hardincompat_returns_typed_record() {
        let proposals = vec![
            Proposal {
                id: "p_local".into(),
                summary: "deploy a cluster_local sticky-session cache".into(),
                approach: "in-process hashmap".into(),
                tradeoffs: vec!["memory bounded by pod".into()],
                evidence: vec!["redis sticky-session benchmarks".into()],
                source_sketch: String::new(),
                artifacts: Vec::new(),
                replaced_by: None,
                source_nodes: Vec::new(),
            },
            Proposal {
                id: "p_global".into(),
                summary: "publish a global webhook endpoint".into(),
                approach: "anycast IP + load balancer".into(),
                tradeoffs: vec!["multi-region failover".into()],
                evidence: vec!["anycast latency study".into()],
                source_sketch: String::new(),
                artifacts: Vec::new(),
                replaced_by: None,
                source_nodes: Vec::new(),
            },
        ];
        let (hardincompat, tags) = SynthesizePhase::cluster_opt_in_hardincompat(&proposals)
            .expect("opt-in detector must fire on cluster_local + global pair");
        assert_eq!(hardincompat, HardIncompat::ClusterLocalInGlobal);
        assert!(
            tags.iter().any(|t| t == "cluster_local"),
            "tags must carry the cluster_local marker, got {tags:?}"
        );
        assert!(
            tags.iter().any(|t| t == "global"),
            "tags must carry the global marker, got {tags:?}"
        );
        // The matrix-driven detector must NOT fire here: the
        // opt-in tag set is disjoint from `HARD_INCOMPATIBILITIES`.
        assert!(
            SynthesizePhase::cluster_conflict(&proposals).is_none(),
            "cluster_local + global must not match the §D.13.15 matrix"
        );
    }

    /// Catalog I.6 (opt-in) detector wiring: a cluster whose
    /// tags do NOT carry any opt-in pair (and do NOT match the
    /// `HARD_INCOMPATIBILITIES` matrix either) must return
    /// `None` from BOTH `cluster_conflict` and
    /// `cluster_opt_in_hardincompat`. This is the regression
    /// guard that the opt-in branch is silent on plain
    /// well-formed clusters.
    #[test]
    fn synthesize_cluster_opt_in_hardincompat_returns_none_on_clean_cluster() {
        let proposals = vec![
            Proposal {
                id: "p_a".into(),
                summary: "monolith sql deployment".into(),
                approach: "single binary with postgres".into(),
                tradeoffs: vec!["simpler operations".into()],
                evidence: vec!["sql benchmarks".into()],
                source_sketch: String::new(),
                artifacts: Vec::new(),
                replaced_by: None,
                source_nodes: Vec::new(),
            },
            Proposal {
                id: "p_b".into(),
                summary: "self-hosted rust service".into(),
                approach: "tokio + sqlx".into(),
                tradeoffs: vec!["operator-friendly".into()],
                evidence: vec!["rust deployment study".into()],
                source_sketch: String::new(),
                artifacts: Vec::new(),
                replaced_by: None,
                source_nodes: Vec::new(),
            },
        ];
        assert!(
            SynthesizePhase::cluster_conflict(&proposals).is_none(),
            "clean cluster must not trip the §D.13.15 matrix"
        );
        assert!(
            SynthesizePhase::cluster_opt_in_hardincompat(&proposals).is_none(),
            "clean cluster must not trip any opt-in detector"
        );
    }

    /// Catalog I.6 (opt-in) detector wiring: the matrix-driven
    /// `cluster_conflict` must keep firing on the §D.13.15 pairs
    /// even when the opt-in branch is also enabled. The two
    /// detectors are additive — a cluster that trips BOTH should
    /// be reported via the matrix path first (preserving the
    /// legacy wire form `incompatible_tags: a,b`).
    #[test]
    fn synthesize_matrix_path_wins_over_opt_in_path_on_overlap() {
        let proposals = vec![
            Proposal {
                id: "p_mono".into(),
                summary: "monolith sql deployment".into(),
                approach: "single binary".into(),
                tradeoffs: vec![],
                evidence: vec![],
                source_sketch: String::new(),
                artifacts: Vec::new(),
                replaced_by: None,
                source_nodes: Vec::new(),
            },
            Proposal {
                id: "p_micro".into(),
                summary: "microservices split across pods".into(),
                approach: "per-service discovery".into(),
                tradeoffs: vec![],
                evidence: vec![],
                source_sketch: String::new(),
                artifacts: Vec::new(),
                replaced_by: None,
                source_nodes: Vec::new(),
            },
        ];
        let matrix = SynthesizePhase::cluster_conflict(&proposals)
            .expect("matrix must fire on monolith + microservices");
        let (a, b, _tags) = matrix;
        assert!(
            (a == "monolith" && b == "microservices") || (a == "microservices" && b == "monolith"),
            "matrix result must name the §D.13.15 pair, got {a},{b}"
        );
        assert!(
            SynthesizePhase::cluster_opt_in_hardincompat(&proposals).is_none(),
            "opt-in path must stay silent on a pure §D.13.15 pair"
        );
    }
}
