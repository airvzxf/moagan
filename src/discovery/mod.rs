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
pub mod context;
pub mod contradiction;
pub mod coordinator;
pub mod epistemic_legacy;
pub mod extractor;
pub mod facet;
pub mod facet_cache;
pub mod id;
pub mod integrator;
pub mod matrix;
pub mod matrix_spec;
pub mod outlier;
pub mod pause;
pub mod persona_angle;
pub mod resume;
pub mod saturation;
pub mod sketch_retry;
pub mod state;
pub mod stop_policy;
pub mod tagger;
pub mod tagger_threshold;

pub use context::DiscoveryContext;
pub use coordinator::{DiscoveryCoordinator, DiscoveryOutcome};
pub use id::{ContradictionId, FacetId, SketchId};
pub use matrix_spec::{DerivedDimensions, DimensionSpec, FacetSpec, MatrixSpec};
pub use outlier::{detect_outliers, detect_outliers_with_threshold};
pub use stop_policy::{
    BlockReason, DEFAULT_DISCOVERY_HARD_CAP, DEFAULT_MAX_SKETCHES, DEFAULT_MIN_SKETCHES,
    DEFAULT_OUTLIER_DISTANCE, DEFAULT_RESERVE_RATIO, DEFAULT_SATURATION_THRESHOLD, StopDecision,
    StopPolicy, StopReason,
};
