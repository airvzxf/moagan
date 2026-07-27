//! Ranking helpers. Pareto front + SimHash clustering + crowding
//! distance, per T01-06 §16.12.
//!
//! The module is split into three sub-modules so each algorithm can be
//! unit-tested independently:
//!
//! - [`pareto`] — multi-criterion dominance filter.
//! - [`cluster`] — SimHash-based proposal clustering (lightweight; no
//!   embedding downloads).
//! - [`diversity`] — crowding-distance pick for top-`k` selection.
//!
//! Spec compliance: §16.12 calls for Pareto + cluster + diversity
//! before the weighted ranking. The v0.1 MVP runs the same five steps
//! (the last two being weighted sort and winner selection).

pub mod cluster;
pub mod diversity;
pub mod pareto;

pub use cluster::{cluster_by_simhash, jaccard_distance};
pub use diversity::pick_with_crowding;
pub use pareto::pareto_front;
