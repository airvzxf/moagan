//! Token budget. Pre-flight estimation via `tiktoken-rs` (cl100k_base)
//! when the provider does not report exact counts.

use std::sync::OnceLock;

use tiktoken_rs::{CoreBPE, cl100k_base};

use crate::error::{Error, Result};

static ENCODER: OnceLock<std::sync::Mutex<Option<CoreBPE>>> = OnceLock::new();

fn encoder() -> Option<CoreBPE> {
    let cell = ENCODER.get_or_init(|| std::sync::Mutex::new(cl100k_base().ok()));
    cell.lock().ok().and_then(|mut g| g.take())
}

/// Estimate the number of tokens in `text` using cl100k_base. Returns
/// the heuristic `(len_bytes / 4)` if the tokenizer is unavailable
/// (e.g. asset download failed).
pub fn estimate_tokens(text: &str) -> Result<u64> {
    if let Some(bpe) = encoder() {
        let n = bpe.encode_ordinary(text).len() as u64;
        // Put the encoder back so subsequent calls reuse it.
        let cell = ENCODER.get_or_init(|| std::sync::Mutex::new(None));
        if let Ok(mut g) = cell.lock() {
            *g = Some(bpe);
        }
        Ok(n)
    } else {
        Ok(text.len().div_ceil(4) as u64)
    }
}

/// Token budget for a run. Tracks consumption per provider.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Total tokens allowed. `None` = unlimited.
    pub total: Option<u64>,
    /// Tokens consumed so far.
    pub consumed: u64,
}

impl Budget {
    /// Build a new budget with a total cap.
    pub fn with_total(total: u64) -> Self {
        Self {
            total: Some(total),
            consumed: 0,
        }
    }

    /// Build an unlimited budget.
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// Reserve `n` tokens. Returns `Err(PlanExhausted)` if the cap would
    /// be exceeded.
    pub fn consume(&mut self, n: u64) -> Result<()> {
        self.consumed = self.consumed.saturating_add(n);
        if let Some(t) = self.total
            && self.consumed > t
        {
            return Err(Error::PlanExhausted(format!(
                "budget exceeded: {} > {}",
                self.consumed, t
            )));
        }
        Ok(())
    }

    /// Remaining tokens; `None` if unlimited.
    pub fn remaining(&self) -> Option<u64> {
        self.total.map(|t| t.saturating_sub(self.consumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_runs() {
        let n = estimate_tokens("hello world, this is a test").unwrap();
        assert!(n > 0);
    }

    #[test]
    fn unlimited_budget_never_exhausts() {
        let mut b = Budget::unlimited();
        for _ in 0..1000 {
            b.consume(1_000_000).unwrap();
        }
        assert_eq!(b.remaining(), None);
    }

    #[test]
    fn bounded_budget_rejects_overflow() {
        let mut b = Budget::with_total(100);
        b.consume(50).unwrap();
        b.consume(50).unwrap();
        assert!(b.consume(1).is_err());
    }
}
