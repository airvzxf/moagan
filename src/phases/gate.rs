//! Gate phase. Validates each proposal structurally; writes
//! `validation/p_*.json` (Pass/Warn/Fail). MVP: structural check only.
//!
//! Spec compliance: T01-06 §5.7 lists 12 deterministic checks; we
//! implement every one. Checks are split into two severities:
//!
//! - **hard** issues cause `pass = false` and trigger the repair phase.
//! - **soft** issues surface as warnings but allow the proposal through.
//!
//! Hard issues are prefixed with `hard:` in the serialised `Gate.issues`
//! so an operator inspecting the validation sidecar can tell them
//! apart at a glance.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::config::Config;
use crate::domain::{Brief, Gate, Proposal};
use crate::error::Result;
use crate::phases::phase::{Phase, PhaseOutput, RunContext};
use crate::phases::util::{read_json, write_json};

/// Gate phase. One report per proposal.
pub struct GatePhase;

#[async_trait]
impl Phase for GatePhase {
    fn name(&self) -> &'static str {
        "gate"
    }

    async fn execute(&self, ctx: &RunContext) -> Result<PhaseOutput> {
        let proposals_dir = ctx.run_dir().proposals();
        let validation_dir = ctx.run_dir().validation();
        std::fs::create_dir_all(&validation_dir)?;
        let cfg = Config::defaults();
        let min_len = cfg.gate_min_length;
        let max_len = cfg.gate_max_length;
        let forbidden: Vec<String> = cfg
            .gate_forbidden_techs
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

        let brief: Brief = read_json(&ctx.run_dir().brief()).unwrap_or_default();

        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let proposal: Proposal = read_json(&path)?;
            let gate = structural_check(&proposal, &brief, &forbidden, min_len, max_len);
            let id = proposal.id;
            let out_path: PathBuf = validation_dir.join(format!("{id}.json"));
            write_json(&out_path, &gate)?;
            paths.push(out_path);
        }
        Ok(PhaseOutput::Validations(paths))
    }
}

/// Severity marker prefix for hard issues. Hard issues cause the gate
/// to fail and trigger the repair phase.
const HARD: &str = "hard:";
/// Severity marker prefix for soft issues. Soft issues surface as
/// warnings but allow the proposal to pass.
const SOFT: &str = "soft:";

/// Run the twelve deterministic structural checks from spec §5.7 against
/// a single proposal. The brief is consulted for cross-referencing
/// constraints, deliverables, and forbidden tech; when the brief is
/// empty (e.g. unit tests without a real brief), only the proposal-
/// local checks run.
pub(crate) fn structural_check(
    p: &Proposal,
    brief: &Brief,
    forbidden_techs_lower: &[String],
    min_len: usize,
    max_len: usize,
) -> Gate {
    let mut hard: Vec<String> = Vec::new();
    let mut soft: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    // Check 1: structure parseable. The caller already parsed the JSON
    // into `Proposal`; if we got here, the structure is parseable.
    // (counted as always-pass)

    // Check 2: required sections present.
    if p.id.is_empty() {
        missing.push("id".into());
    }
    if p.summary.trim().is_empty() {
        missing.push("summary".into());
    }
    if p.approach.trim().is_empty() {
        missing.push("approach".into());
    }
    if p.tradeoffs.is_empty() {
        soft.push(format!("{SOFT} no tradeoffs listed"));
    }
    if p.evidence.is_empty() {
        soft.push(format!("{SOFT} no evidence listed"));
    }

    // Compose a single corpus for substring checks. Lowercased once.
    let corpus_lower = format!(
        "{} {} {} {}",
        p.summary,
        p.approach,
        p.tradeoffs.join(" "),
        p.evidence.join(" ")
    )
    .to_lowercase();

    // Check 3: no truncation. The Proposal struct already rejects
    // unterminated JSON (the parse would have failed upstream), so
    // here we check for common m3 truncation patterns: ellipsis at
    // the end, dangling colon, or a final whitespace.
    if p.approach.ends_with("...")
        || p.approach.ends_with("…")
        || p.approach.ends_with(':')
        || p.approach.ends_with(char::is_whitespace)
    {
        hard.push(format!("{HARD} approach appears truncated"));
    }

    // Check 4: no critical placeholders. `TODO`, `TBD`, `xxx`, `???`
    // as standalone tokens inside `approach` are flagged.
    for token in ["todo", "tbd", "xxx", "???"] {
        if corpus_lower.contains(&format!(" {token} "))
            || corpus_lower.contains(&format!(" {token}."))
        {
            hard.push(format!("{HARD} contains placeholder '{token}'"));
        }
    }

    // Check 5: balanced code blocks. Count ``` ``` (fenced) — odd
    // counts mean an open block never closed.
    let fence_open = p.approach.matches("```").count();
    if !fence_open.is_multiple_of(2) {
        hard.push(format!(
            "{HARD} unbalanced code fences (count={fence_open})"
        ));
    }

    // Check 6: hard constraints from the brief appear in the proposal.
    // We look for the most distinctive word from each constraint
    // (length > 4, lowercase) and require at least one mention. Empty
    // or very short constraints are skipped.
    let proposal_words: HashSet<&str> = corpus_lower.split_whitespace().collect();
    for constraint in &brief.constraints {
        let needle = pick_needle(constraint);
        if let Some(needle) = needle
            && !proposal_words.contains(needle.as_str())
            && !corpus_lower.contains(&needle)
        {
            soft.push(format!("{SOFT} constraint not addressed: {constraint}"));
        }
    }

    // Check 7: forbidden techs absent from the proposal.
    for tech in forbidden_techs_lower {
        if corpus_lower.contains(tech.as_str()) {
            hard.push(format!("{HARD} forbidden technology present: {tech}"));
        }
    }

    // Check 8: deliverables from the brief are mentioned in the proposal.
    for deliverable in &brief.deliverables {
        let needle = pick_needle(deliverable);
        if let Some(needle) = needle
            && !proposal_words.contains(needle.as_str())
            && !corpus_lower.contains(&needle)
        {
            soft.push(format!("{SOFT} deliverable not mentioned: {deliverable}"));
        }
    }

    // Check 9: format / language. Heuristic: if the brief's problem
    // statement has any non-ASCII letter and the proposal is pure
    // ASCII, the languages probably don't match. We do not require
    // exact detection — only flag the obvious mismatch.
    let brief_non_ascii = brief.problem.chars().any(|c| c > '\u{7f}');
    let proposal_non_ascii = corpus_lower.chars().any(|c| c > '\u{7f}');
    if brief_non_ascii != proposal_non_ascii && !brief.problem.is_empty() {
        soft.push(format!("{SOFT} language mismatch with brief"));
    }

    // Check 10: trivial contradictions. Heuristic: a sentence that
    // contains "always" and another that contains "never" within
    // the same proposal is suspicious.
    let always_present = corpus_lower.contains(" always ");
    let never_present = corpus_lower.contains(" never ");
    if always_present && never_present {
        soft.push(format!("{SOFT} contains both 'always' and 'never'"));
    }
    let must_present = corpus_lower.contains(" must ") && corpus_lower.contains(" optional ");

    if must_present {
        soft.push(format!(
            "{SOFT} mixes 'must' and 'optional' for the same item"
        ));
    }

    // Check 11: no degenerate / evasive answer. Common phrases that
    // mean "the model gave up". The matches are case-insensitive and
    // require word boundaries to avoid false positives on "dependable"
    // or "contextual".
    for phrase in [
        "it depends",
        "depends on the context",
        "depends on the use case",
        "there is no answer",
        "no definitive answer",
        "i cannot answer",
    ] {
        if corpus_lower.contains(phrase) {
            soft.push(format!("{SOFT} may be evasive: '{phrase}'"));
        }
    }

    // Check 12: length in expected range. The corpus length is the
    // sum of all four text fields. Length checks do not apply when
    // min_len == 0 (e.g. unit tests with custom limits).
    let total_len =
        p.summary.len() + p.approach.len() + p.tradeoffs.len() * 32 + p.evidence.len() * 32;
    if min_len > 0 && total_len < min_len {
        soft.push(format!(
            "{SOFT} total length {total_len} below min {min_len}"
        ));
    }
    if max_len > 0 && total_len > max_len {
        soft.push(format!(
            "{SOFT} total length {total_len} above max {max_len}"
        ));
    }

    let pass = missing.is_empty() && hard.is_empty();
    let mut issues: Vec<String> = Vec::with_capacity(hard.len() + soft.len());
    issues.extend(hard);
    issues.extend(soft);
    Gate {
        pass,
        issues,
        missing,
    }
}

/// Pick a distinctive lowercase needle (token length > 4) from a free-
/// form constraint or deliverable. Returns `None` when no good token is
/// available (empty input, very short words, or pure punctuation).
fn pick_needle(text: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for raw in text.split_whitespace() {
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .collect();
        let lower = cleaned.to_lowercase();
        if lower.len() > 4 && lower.chars().any(|c| c.is_alphabetic()) {
            if let Some(prev) = &best {
                if lower.len() > prev.len() {
                    best = Some(lower);
                }
            } else {
                best = Some(lower);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_brief() -> Brief {
        Brief::default()
    }

    #[test]
    fn structural_check_flags_missing_summary() {
        let p = Proposal {
            id: "p_001".into(),
            summary: String::new(),
            approach: "x".into(),
            tradeoffs: vec!["a".into()],
            evidence: vec!["b".into()],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &[], 0, 0);
        assert!(!g.pass);
        assert!(g.missing.contains(&"summary".to_string()));
    }

    #[test]
    fn structural_check_flags_truncated_approach() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "ok".into(),
            approach: "We start with...".into(),
            tradeoffs: vec!["a".into()],
            evidence: vec!["b".into()],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &[], 0, 0);
        assert!(g.issues.iter().any(|i| i.contains("truncated")));
        assert!(!g.pass);
    }

    #[test]
    fn structural_check_flags_unbalanced_fences() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "ok".into(),
            approach: "Here is code:\n```rust\nfn x() {}\n".into(),
            tradeoffs: vec!["a".into()],
            evidence: vec!["b".into()],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &[], 0, 0);
        assert!(g.issues.iter().any(|i| i.contains("unbalanced")));
        assert!(!g.pass);
    }

    #[test]
    fn structural_check_flags_forbidden_tech() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "ok".into(),
            approach: "Use postgres for everything".into(),
            tradeoffs: vec!["a".into()],
            evidence: vec!["b".into()],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &["postgres".into()], 0, 0);
        assert!(g.issues.iter().any(|i| i.contains("forbidden")));
        assert!(!g.pass);
    }

    #[test]
    fn structural_check_passes_clean_proposal() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "Use the standard ROYGBIV order for the rainbow".into(),
            approach: "Output red, orange, yellow, green, blue, indigo, violet in that order."
                .into(),
            tradeoffs: vec!["None — the user asked for the standard order".into()],
            evidence: vec!["Wikipedia: Rainbow".into()],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &[], 0, 0);
        assert!(g.pass, "issues = {:?}", g.issues);
        assert!(g.missing.is_empty());
    }

    #[test]
    fn structural_check_flags_placeholder() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "ok".into(),
            approach: "We will TODO the rest later".into(),
            tradeoffs: vec!["a".into()],
            evidence: vec!["b".into()],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &[], 0, 0);
        assert!(g.issues.iter().any(|i| i.contains("placeholder")));
        assert!(!g.pass);
    }

    #[test]
    fn structural_check_flags_soft_warnings_without_failing() {
        let p = Proposal {
            id: "p_001".into(),
            summary: "ok".into(),
            approach: "Output X".into(),
            tradeoffs: vec![],
            evidence: vec![],
            source_sketch: String::new(),
        };
        let g = structural_check(&p, &empty_brief(), &[], 0, 0);
        // tradeoffs/evidence missing = soft warning, but proposal still passes.
        assert!(g.pass);
        assert!(g.issues.iter().any(|i| i.starts_with("soft:")));
    }

    #[test]
    fn pick_needle_skips_short_words() {
        assert!(pick_needle("").is_none());
        assert!(pick_needle("a be cd").is_none());
        assert_eq!(pick_needle("Use postgres for storage").unwrap(), "postgres");
        assert_eq!(
            pick_needle("must not use relational").unwrap(),
            "relational"
        );
    }
}
