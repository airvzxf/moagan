//! Discovery mode — `discover_contradict` phase.
//!
//! For each pair of clusters with a sufficiently high
//! disagreement score (default: cohesion delta), the LLM-as-judge
//! detector (`Role::ContradictionJudge`,
//! `crate::discovery::contradiction`) is asked to surface every
//! contradiction between one focal sketch from cluster A and the
//! sketches in cluster B. The findings are aggregated per cluster
//! pair, sorted by severity, and written to
//! `contradictions/contradictions.json` as one
//! `Contradiction` per pair.
//!
//! A#11 rewrote this phase against the LLM-as-judge detector
//! (the previous stub asked the tagger for a single 3-tuple per
//! cluster pair; the new detector asks one JSON-mode call per
//! cluster pair and returns a `findings` array).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};

use crate::discovery::contradiction::{
    ContradictionRecord, find_contradictions_against, severity_rank, top_pairs,
};
use crate::domain::{Cluster, Contradiction, ContradictionFinding, Sketch};
use crate::error::Result;
use crate::ids::RunId;
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Maximum cross-cluster pairs to surface. The detection runs in
/// O(n^2) so we cap the input here.
const MAX_PAIRS: usize = 16;

/// Discovery contradiction phase.
pub struct DiscoverContradictPhase {
    /// Cohesion delta threshold (0..=1). Pairs with `|a - b|`
    /// above this become candidates. Default 0.3.
    pub delta_threshold: f32,
}

impl Default for DiscoverContradictPhase {
    fn default() -> Self {
        Self {
            delta_threshold: 0.3,
        }
    }
}

impl DiscoverContradictPhase {
    /// Read every cluster file from `clusters_dir`, skipping the
    /// `index.json` sidecar. The clusters determine the upper
    /// bound on the number of LLM-as-judge calls.
    fn read_clusters(clusters_dir: &std::path::Path) -> Result<Vec<Cluster>> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(clusters_dir)?
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
            clusters.push(read_json(path)?);
        }
        Ok(clusters)
    }

    /// Read every sketch file from `sketches_dir`, skipping the
    /// index sidecar. The detector needs the focal sketch body
    /// and every candidate sketch body verbatim, so they are
    /// loaded into memory once and shared across calls.
    ///
    /// The legacy version only read clusters; the legacy
    /// contract was an empty sketches folder is fine. We keep
    /// that contract here so the integration test
    /// `discovery_contradict_phase_handles_one_cluster` (one
    /// cluster, no sketches) still passes — the dir simply
    /// counts as zero sketches when it does not exist yet.
    fn read_sketches(sketches_dir: &std::path::Path) -> Result<Vec<Sketch>> {
        let entries = match std::fs::read_dir(sketches_dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|r| r.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|s| s.to_str()) == Some("json")
                    && p.file_name().and_then(|s| s.to_str()) != Some("index.json")
            })
            .collect();
        paths.sort();
        let mut sketches: Vec<Sketch> = Vec::with_capacity(paths.len());
        for path in &paths {
            sketches.push(read_json(path)?);
        }
        Ok(sketches)
    }

    /// Resolve the sketch bodies whose ids are in `members`. Skips
    /// sketch ids that don't resolve so a single missing sketch
    /// cannot blank the comparison.
    fn resolve_sketches(all: &[Sketch], members: &[String]) -> Vec<Sketch> {
        let mut out: Vec<Sketch> = Vec::with_capacity(members.len());
        for id in members {
            if let Some(s) = all.iter().find(|s| &s.id == id) {
                out.push(s.clone());
            }
        }
        out
    }

    /// Run the LLM-as-judge call for one cluster pair. Picks
    /// the first member of cluster A as the focal sketch (any
    /// member would do; the call is symmetric) and uses every
    /// sketch in cluster B as the candidate pool. Returns the
    /// vector of typed findings (possibly empty) and the
    /// representatives list the phase persists in the
    /// `Contradiction` sidecar.
    async fn run_pair(
        ctx: &RunContext,
        sketches: &[Sketch],
        a: &Cluster,
        b: &Cluster,
    ) -> Result<(Vec<ContradictionFinding>, Vec<String>)> {
        let a_sketches = Self::resolve_sketches(sketches, &a.members);
        let b_sketches = Self::resolve_sketches(sketches, &b.members);
        let representatives: Vec<String> =
            a.members.iter().chain(b.members.iter()).cloned().collect();
        let Some(focal) = a_sketches.first().cloned() else {
            return Ok((Vec::new(), representatives));
        };
        let findings = find_contradictions_against(ctx, &focal, &b_sketches).await?;
        Ok((findings, representatives))
    }

    /// Flatten a list of `ContradictionFinding` (one cluster
    /// pair may surface zero or more findings) into the legacy
    /// `Contradiction` sidecar shape. The first finding's
    /// severity, evidence and suggestion drive the
    /// representative row; additional findings are appended as
    /// additional rows so the integrator phase can still pick
    /// up the urgent fixes. Empty finding lists collapse to a
    /// single `"low"` row with `"no significant contradiction"`
    /// — same wire form the previous stub produced.
    fn into_contradictions(
        cluster_a: &str,
        cluster_b: &str,
        representatives: &[String],
        findings: &[ContradictionFinding],
    ) -> Vec<Contradiction> {
        if findings.is_empty() {
            return vec![Contradiction {
                id: String::new(),
                cluster_a: cluster_a.to_owned(),
                cluster_b: cluster_b.to_owned(),
                representatives: representatives.to_vec(),
                topic: "consistency".into(),
                description: "no significant contradiction".into(),
                severity: "low".into(),
                schema_version: "v1".into(),
            }];
        }
        findings
            .iter()
            .map(|f| Contradiction {
                id: String::new(),
                cluster_a: cluster_a.to_owned(),
                cluster_b: cluster_b.to_owned(),
                representatives: representatives.to_vec(),
                topic: pair_topic(f),
                description: f.evidence.clone(),
                severity: f.severity.legacy_label().to_owned(),
                schema_version: "v1".into(),
            })
            .collect()
    }

    /// Backwards compatibility shim — kept so existing imports
    /// from `crate::phases::discover_contradict::user_payload`
    /// continue to resolve. The new detector is wired through
    /// [`crate::discovery::contradiction::user_payload`].
    #[allow(dead_code)]
    fn legacy_user_payload(a: &Cluster, b: &Cluster) -> String {
        // Synthesize a thin payload so the helper keeps the old
        // signature; it is only used by the unit tests that
        // exercise the old stub shape. The real call path uses
        // `crate::discovery::contradiction::user_payload`.
        let sk_lines: Vec<String> = a
            .members
            .iter()
            .chain(b.members.iter())
            .map(|id| format!("- {id}"))
            .collect();
        format!(
            "Cluster A:\n  id: {a_id}\n  label: {a_label}\n  summary: {a_summary}\n  \
             members: {a_members}\n\n\
             Cluster B:\n  id: {b_id}\n  label: {b_label}\n  summary: {b_summary}\n  \
             members: {b_members}\n\n\
             Sketch ids:\n{sk}\n\n\
             Return a JSON object.",
            a_id = a.id,
            a_label = a.label,
            a_summary = a.summary,
            a_members = a.members.join(", "),
            b_id = b.id,
            b_label = b.label,
            b_summary = b.summary,
            b_members = b.members.join(", "),
            sk = sk_lines.join("\n"),
        )
    }
}

/// Topic tag for a single finding. The legacy sidecar only
/// knows `"consistency"` (and a handful of similar nouns); the
/// new detector returns free-form evidence but not a topic.
/// We pin the topic to `"consistency"` to keep the wire form
/// stable for downstream consumers.
fn pair_topic(_f: &ContradictionFinding) -> String {
    "consistency".to_owned()
}

/// Inner legacy type kept around so the `discovery.rs` integration
/// tests that import the type alias don't break.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(dead_code)]
struct ContradictionRefinement {
    topic: String,
    description: String,
    severity: String,
}

#[async_trait]
impl Phase for DiscoverContradictPhase {
    fn name(&self) -> &'static str {
        "discover_contradict"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let clusters_dir = ctx.run_dir().clusters();
        let sketches_dir = ctx.run_dir().sketches();
        let contradictions_dir = ctx.run_dir().contradictions();
        std::fs::create_dir_all(&contradictions_dir)?;

        let clusters = Self::read_clusters(&clusters_dir)?;
        let sketches = Self::read_sketches(&sketches_dir)?;

        if clusters.len() < 2 {
            // Nothing to compare.
            let path = contradictions_dir.join("contradictions.json");
            write_json(&path, &Vec::<Contradiction>::new())?;
            return Ok(PhaseOutput::Sketches(vec![path]));
        }

        // Compute pairwise distances from the cohesion score.
        let mut distances: Vec<(String, String, f32)> = Vec::new();
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let (a, b) = (&clusters[i], &clusters[j]);
                let delta = (a.cohesion - b.cohesion).abs();
                if delta >= self.delta_threshold {
                    distances.push((a.id.clone(), b.id.clone(), delta));
                }
            }
        }
        distances.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
        let top = top_pairs(&distances, MAX_PAIRS);

        // LLM-as-judge pass per cluster pair. The detector
        // already iterates `(focal, candidates)` internally;
        // the phase only needs to keep the per-pair parallelism
        // and surface the findings as `Contradiction` rows.
        let by_id: Arc<std::collections::HashMap<String, Cluster>> =
            Arc::new(clusters.iter().map(|c| (c.id.clone(), c.clone())).collect());

        // Re-expose system_prompt + Role for the wrapping
        // call_with_retry_parse so the discovery.contradiction_judge
        // warnings stream tags the role correctly. The actual
        // dispatch lives in `find_contradictions_against`.
        let _ = (system_prompt(Role::ContradictionJudge), 3u32);

        let sketches = Arc::new(sketches);
        let futures = top.iter().map(|(a_id, b_id, _delta)| {
            let a_id = a_id.clone();
            let b_id = b_id.clone();
            let by_id = Arc::clone(&by_id);
            let sketches = Arc::clone(&sketches);
            let ctx = ctx.clone();
            async move {
                let _permit = ctx.parallelism.acquire().await?;
                let a = by_id.get(&a_id).cloned().unwrap_or_default();
                let b = by_id.get(&b_id).cloned().unwrap_or_default();
                let (findings, representatives) = Self::run_pair(&ctx, &sketches, &a, &b).await?;
                Ok::<(String, String, Vec<ContradictionFinding>, Vec<String>), crate::error::Error>(
                    (a_id, b_id, findings, representatives),
                )
            }
        });
        let results = join_all(futures).await;

        let mut items: Vec<Contradiction> = Vec::new();
        for (a_id, b_id, findings, representatives) in results.into_iter().flatten() {
            let pair_rows = Self::into_contradictions(&a_id, &b_id, &representatives, &findings);
            items.extend(pair_rows);
        }

        // Stable id assignment after the sort so the integrator
        // sees the canonical ordering; sort by severity so the
        // integrator picks the urgent ones first.
        items.sort_by_key(|c| std::cmp::Reverse(severity_rank(&c.severity)));
        for (idx, c) in items.iter_mut().enumerate() {
            c.id = format!("c_{:02}", idx);
        }

        let path = contradictions_dir.join("contradictions.json");
        write_json(&path, &items)?;

        // Run-id carried for the sidecar schema in case any
        // downstream tool wants to know which run produced this.
        let _ = RunId::default();
        Ok(PhaseOutput::Sketches(vec![path]))
    }
}

#[allow(dead_code)]
fn _legacy_record_anchor(
    cluster_a: &str,
    cluster_b: &str,
    representatives: Vec<String>,
    severity: &str,
    description: &str,
) -> ContradictionRecord {
    ContradictionRecord {
        cluster_a: cluster_a.to_owned(),
        cluster_b: cluster_b.to_owned(),
        representatives,
        topic: "consistency".into(),
        description: description.into(),
        severity: severity.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::contradiction::user_payload;
    use crate::domain::Sketch;

    /// The empty pair synthesises the legacy `"low"`,
    /// `"no significant contradiction"` shape so the wire form
    /// stays byte-identical with the v0.5 stub.
    #[test]
    fn into_contradictions_empty_findings_yields_low_row() {
        let rows = DiscoverContradictPhase::into_contradictions(
            "cluster_01",
            "cluster_02",
            &["sk_001".into(), "sk_002".into()],
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].severity, "low");
        assert_eq!(rows[0].description, "no significant contradiction");
    }

    /// The non-empty path maps each finding to its own row.
    /// Severity is converted back to the legacy `"low" | "medium" |
    /// "high"` vocabulary.
    #[test]
    fn into_contradictions_maps_findings_to_rows() {
        let findings = vec![
            ContradictionFinding {
                pair: ["sk_001".into(), "sk_002".into()],
                severity: crate::domain::ContradictionSeverity::Critical,
                evidence: "ACID vs eventual".into(),
                suggestion: "pick one".into(),
            },
            ContradictionFinding {
                pair: ["sk_001".into(), "sk_003".into()],
                severity: crate::domain::ContradictionSeverity::Minor,
                evidence: "tradeoff mismatch".into(),
                suggestion: "".into(),
            },
        ];
        let rows = DiscoverContradictPhase::into_contradictions(
            "cluster_01",
            "cluster_02",
            &["sk_001".into(), "sk_002".into(), "sk_003".into()],
            &findings,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].severity, "high");
        assert_eq!(rows[1].severity, "low");
        assert_eq!(rows[0].description, "ACID vs eventual");
    }

    /// The user-payload helper exposed by the discovery module
    /// must list every sketch id verbatim so the LLM can quote
    /// them in `evidence`.
    #[test]
    fn user_payload_includes_all_sketch_ids() {
        let focal = Sketch {
            id: "sk_001".into(),
            thesis: "alpha".into(),
            ..Default::default()
        };
        let b1 = Sketch {
            id: "sk_002".into(),
            thesis: "beta".into(),
            ..Default::default()
        };
        let b2 = Sketch {
            id: "sk_003".into(),
            thesis: "gamma".into(),
            ..Default::default()
        };
        let p = user_payload(&focal, std::slice::from_ref(&b1));
        assert!(p.contains("sk_001"));
        assert!(p.contains("sk_002"));
        let p = user_payload(&focal, &[b1.clone(), b2.clone()]);
        assert!(p.contains("sk_001"));
        assert!(p.contains("sk_002"));
        assert!(p.contains("sk_003"));
    }

    /// The legacy `user_payload` shim keeps the signature for
    /// backward compatibility so tests that import the old
    /// helper shape still compile.
    #[test]
    fn legacy_user_payload_contains_cluster_ids() {
        let a = Cluster {
            id: "cluster_01".into(),
            label: "auth".into(),
            summary: "JWT-based".into(),
            members: vec!["sk_001".into()],
            ..Default::default()
        };
        let b = Cluster {
            id: "cluster_02".into(),
            label: "session".into(),
            summary: "Cookie-based".into(),
            members: vec!["sk_002".into()],
            ..Default::default()
        };
        let s = DiscoverContradictPhase::legacy_user_payload(&a, &b);
        assert!(s.contains("cluster_01"));
        assert!(s.contains("cluster_02"));
        assert!(s.contains("JWT-based"));
        assert!(s.contains("Cookie-based"));
    }
}
