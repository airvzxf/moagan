//! Discovery module — Plan B sub-phase B.
//!
//! This is the home of the discovery-specific building blocks:
//! matrix construction, tagging, clustering, contradiction detection,
//! facet derivation, per-facet extraction, and hybrid integration.
//!
//! The actual phases that orchestrate these builders live in
//! `src/phases/discover_*.rs`. The split mirrors `src/ranking/` (pure
//! helpers) vs `src/phases/rank.rs` (the phase that wires them
//! together).

pub mod clusterer;
pub mod contradiction;
pub mod coordinator;
pub mod epistemic_legacy;
pub mod extractor;
pub mod facet;
pub mod facet_cache;
pub mod integrator;
pub mod matrix;
pub mod pause;
pub mod resume;
pub mod state;
pub mod tagger;

pub use coordinator::{DiscoveryCoordinator, DiscoveryOutcome};
