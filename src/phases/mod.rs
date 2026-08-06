//! Pipeline phases. v0.2 ships a non-discovery pipeline with optional
//! sketches
//! (intake → clarify → route → sketch? → propose → gate → validate →
//! critique → repair → judge → rank → deliver) per V4 §13.6. The
//! sketch step is gated by `Mode::runs_sketches()`; `fast` skips it.
//! Phase D adds `cluster_proposals` + `synthesize` between critique
//! and judge, and an adversary branch inside `judge` (V4 §5.13).

pub mod budget;
pub mod budget_cascade;
pub mod cardinality;
pub mod clarify;
pub mod cluster_proposals;
pub mod critique;
pub mod decompose;
pub mod deliver;
pub mod discover_cluster;
pub mod discover_contradict;
pub mod discover_extract;
pub mod discover_facet;
pub mod discover_integrate;
pub mod discover_matrix;
pub mod discover_summary;
pub mod discover_tag;
pub mod gate;
pub mod intake;
pub mod judge;
pub mod phase;
pub mod pipe;
pub mod propose;
pub mod rank;
pub mod repair;
pub mod replace;
pub mod route;
pub mod sketch_phase;
pub mod synthesize;
pub mod util;
pub mod validate;

pub use budget::{BudgetObserver, BudgetPolicy, PressureLevel};
pub use budget_cascade::cascade_reduce;
pub use clarify::ClarifyPhase;
pub use cluster_proposals::ClusterProposalsPhase;
pub use critique::CritiquePhase;
pub use decompose::DecomposePhase;
pub use deliver::DeliverPhase;
pub use discover_cluster::DiscoverClusterPhase;
pub use discover_contradict::DiscoverContradictPhase;
pub use discover_extract::DiscoverExtractPhase;
pub use discover_facet::DiscoverFacetPhase;
pub use discover_integrate::DiscoverIntegratePhase;
pub use discover_matrix::DiscoverMatrixPhase;
pub use discover_summary::DiscoverSummaryPhase;
pub use discover_tag::DiscoverTagPhase;
pub use gate::GatePhase;
pub use intake::IntakePhase;
pub use judge::JudgePhase;
pub use phase::{Phase, PhaseOutput, RunContext};
pub use pipe::Pipeline;
pub use propose::ProposePhase;
pub use rank::RankPhase;
pub use repair::RepairPhase;
pub use route::RoutePhase;
pub use sketch_phase::SketchPhase;
pub use synthesize::SynthesizePhase;
pub use validate::ValidatePhase;
