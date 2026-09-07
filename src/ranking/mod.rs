//! Ranking helpers. Pareto front + SimHash clustering + crowding
//! distance.
//!
//! The module is split into seven sub-modules so each algorithm can
//! unit-tested independently:
//!
//! - [`pareto`] — multi-criterion dominance filter.
//! - [`cluster`] — SimHash-based proposal clustering (lightweight; no
//!   embedding downloads).
//! - [`diversity`] — crowding-distance pick for top-`k` selection.
//! - [`rubric`] — six-criterion rubric anchors consumed by the rank
//!   phase to score each proposal.
//! - [`adversary_patterns`] — D.22.1 + D.12.5: twelve pattern
//!   detectors that map a metric to a boolean verdict and a
//!   free-form detail string; the judge phase runs them to decide
//!   whether a proposal needs a refine action. The original seven
//!   patterns from PR-11 / v0.5 are augmented by the five D.12.5
//!   add-on patterns (`shared_blind_spots`,
//!   `unanimous_claims_without_evidence`, `hidden_assumptions`,
//!   `omitted_risks`, `unverified_claims`).
//! - [`refine_action`] — D.22.2: seven-variant [`RefineAction`]
//!   enum the refine loop dispatches on; one action is picked per
//!   fired [`AdversaryPattern`].
//! - [`stability`] — Phase H: perturb the
//!   per-criterion weights and measure how often each proposal keeps
//!   its rank.
//!
//! Spec compliance: Pareto + cluster + diversity
//! before the weighted ranking. The v0.1 MVP runs the same five steps
//! (the last two being weighted sort and winner selection); the
//! stability check is a Phase H addition that the rank phase wires
//! in as a step 5.6. The four spec-mandated sub-modules plus the
//! three judge-phase additions (`rubric`, `adversary_patterns`,
//! `refine_action`) are listed in declaration order in the source
//! below.

pub mod adversary_patterns;
pub mod cluster;
pub mod diversity;
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
