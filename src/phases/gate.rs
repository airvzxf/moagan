//! Gate phase. Validates each proposal structurally; writes
//! `validation/p_*.json` (Pass/Warn/Fail). MVP: structural check only.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::domain::{Gate, Proposal};
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
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&proposals_dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !file_name.ends_with(".json") || file_name.ends_with(".meta.json") {
                continue;
            }
            let proposal: Proposal = read_json(&path)?;
            let gate = structural_check(&path, &proposal);
            let id = proposal.id;
            let out_path: PathBuf = validation_dir.join(format!("{id}.json"));
            write_json(&out_path, &gate)?;
            paths.push(out_path);
        }
        Ok(PhaseOutput::Validations(paths))
    }
}

fn structural_check(path: &Path, p: &Proposal) -> Gate {
    let mut issues = Vec::new();
    let mut missing = Vec::new();
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
        issues.push("no tradeoffs listed".into());
    }
    if p.evidence.is_empty() {
        issues.push("no evidence listed".into());
    }
    let pass = missing.is_empty() && issues.is_empty();
    let _ = path;
    Gate {
        pass,
        issues,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_check_flags_missing_summary() {
        let p = Proposal {
            id: "p_001".into(),
            summary: String::new(),
            approach: "x".into(),
            tradeoffs: vec!["a".into()],
            evidence: vec!["b".into()],
        };
        let g = structural_check(std::path::Path::new("/x"), &p);
        assert!(!g.pass);
        assert!(g.missing.contains(&"summary".to_string()));
    }
}
