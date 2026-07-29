//! Pipeline phases. v0.2 ships a non-discovery pipeline with optional
//! sketches
//! (intake → clarify → route → sketch? → propose → gate → critique →
//! repair → judge → rank → deliver) per V4 §13.6. The sketch step is
//! gated by `Mode::runs_sketches()`; `fast` skips it.

pub mod clarify;
pub mod critique;
pub mod deliver;
pub mod discover_cluster;
pub mod discover_matrix;
pub mod discover_tag;
pub mod gate;
pub mod intake;
pub mod judge;
pub mod phase;
pub mod pipe;
pub mod propose;
pub mod rank;
pub mod repair;
pub mod route;
pub mod sketch_phase;
pub mod util;
pub mod validate;

pub use clarify::ClarifyPhase;
pub use critique::CritiquePhase;
pub use deliver::DeliverPhase;
pub use discover_cluster::DiscoverClusterPhase;
pub use discover_matrix::DiscoverMatrixPhase;
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
pub use validate::ValidatePhase;
