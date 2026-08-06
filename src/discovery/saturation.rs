//! D.13.2: simple saturation tracker. Counts completed sketches
//! and emits a tagged event when coverage reaches the threshold.

use crate::discovery::state::SketchLoopState;

/// Tracks sketch completion against a target. Callers
/// (`DiscoveryCoordinator::run`) update `completed` after each
/// successful sketch and inspect `is_saturated()` to decide
/// whether the loop has finished.
pub struct SaturationTracker {
    /// Number of sketches that have been completed.
    pub completed: usize,
    /// Planned total number of sketches for this run.
    pub target: usize,
}

impl SaturationTracker {
    /// Build a tracker with `completed = 0` and the given target.
    pub fn new(target: usize) -> Self {
        Self {
            completed: 0,
            target,
        }
    }

    /// Snapshot a tracker from the current `SketchLoopState`. The
    /// state's `completed_sketches` vector is the source of truth.
    pub fn from_state(state: &SketchLoopState, target: usize) -> Self {
        Self {
            completed: state.completed_sketches.len(),
            target,
        }
    }

    /// Fraction in `[0.0, 1.0]`. A `target` of `0` reports full
    /// coverage to avoid division-by-zero traps in the caller's
    /// decision logic.
    pub fn coverage(&self) -> f32 {
        if self.target == 0 {
            1.0
        } else {
            self.completed as f32 / self.target as f32
        }
    }

    /// `true` when coverage has reached (or exceeded) 100%.
    pub fn is_saturated(&self) -> bool {
        self.coverage() >= 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_tracker_zero_target_is_saturated() {
        let t = SaturationTracker::new(0);
        assert_eq!(t.coverage(), 1.0);
        assert!(t.is_saturated());
    }

    #[test]
    fn saturation_tracker_computes_coverage() {
        let t = SaturationTracker {
            completed: 3,
            target: 4,
        };
        assert!((t.coverage() - 0.75).abs() < 1e-6);
        assert!(!t.is_saturated());
    }

    #[test]
    fn saturation_tracker_from_state_reads_completed_count() {
        let mut state = SketchLoopState::new("default".to_string());
        state.record_completion("sk_0001".to_string());
        state.record_completion("sk_0002".to_string());
        let t = SaturationTracker::from_state(&state, 2);
        assert_eq!(t.completed, 2);
        assert!(t.is_saturated());
    }
}
