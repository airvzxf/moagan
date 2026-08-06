//! Ranking helpers. Pareto front + SimHash clustering + crowding
//! distance, per T01-06 §16.12.
//!
//! The module is split into four sub-modules so each algorithm can be
//! unit-tested independently:
//!
//! - [`pareto`] — multi-criterion dominance filter.
//! - [`cluster`] — SimHash-based proposal clustering (lightweight; no
//!   embedding downloads).
//! - [`diversity`] — crowding-distance pick for top-`k` selection.
//! - [`stability`] — Phase H (V4 §5.12 paso 6): perturb the
//!   per-criterion weights and measure how often each proposal keeps
//!   its rank.
//!
//! Spec compliance: §16.12 calls for Pareto + cluster + diversity
//! before the weighted ranking. The v0.1 MVP runs the same five steps
//! (the last two being weighted sort and winner selection); the
//! stability check is a Phase H addition that the rank phase wires
//! in as a step 5.6.

pub mod adversary_patterns;
pub mod cluster;
pub mod diversity;
pub mod invalidate_downstream;
pub mod pareto;
pub mod refine_action;
pub mod rubric;
pub mod stability;

pub use adversary_patterns::{AdversaryPattern, PatternVerdict, run_all_patterns};
pub use cluster::{cluster_by_simhash, jaccard_distance};
pub use diversity::pick_with_crowding;
pub use pareto::pareto_front;
pub use refine_action::RefineAction;
pub use rubric::{Criterion, RUBRIC_ANCHORS, Rubric, render_rubric_block};
