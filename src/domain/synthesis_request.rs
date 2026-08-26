//! D.22.4: SynthesisRequest with prohibited_decisions.
//!
//! Caller can hint at decisions the synthesizer must NOT take
//! (e.g. "don't switch to Postgres" when DBA forbids it).

use serde::Serialize;

/// Request constraints passed to synthesis.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SynthesisRequest {
    /// Decisions the synthesizer must avoid.
    pub prohibited_decisions: Vec<String>,
}

impl SynthesisRequest {
    /// Construct an unconstrained synthesis request.
    pub fn new() -> Self {
        tracing::trace!("domain::synthesis_request::SynthesisRequest::new");
        Self::default()
    }
    /// Add a prohibited decision.
    pub fn forbid(mut self, decision: &str) -> Self {
        tracing::trace!(
            decision,
            total = self.prohibited_decisions.len(),
            "domain::synthesis_request::SynthesisRequest::forbid"
        );
        self.prohibited_decisions.push(decision.to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::SynthesisRequest;
    #[test]
    fn synthesis_request_forbid_accumulates() {
        let request = SynthesisRequest::new()
            .forbid("switch to Postgres")
            .forbid("remove SQLite");
        assert_eq!(
            request.prohibited_decisions,
            vec!["switch to Postgres", "remove SQLite"]
        );
    }
    #[test]
    fn synthesis_request_empty_is_default() {
        assert_eq!(
            SynthesisRequest::new().prohibited_decisions,
            SynthesisRequest::default().prohibited_decisions
        );
    }
}
