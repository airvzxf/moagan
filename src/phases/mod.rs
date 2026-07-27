//! Pipeline phases. The MVP ships a non-discovery pipeline
//! (intake → clarify → route → propose → gate → critique → repair →
//! judge → rank → deliver) per V4 §13.6 MVP definition.

pub mod clarify;
pub mod critique;
pub mod deliver;
pub mod gate;
pub mod intake;
pub mod judge;
pub mod phase;
pub mod pipe;
pub mod propose;
pub mod rank;
pub mod repair;
pub mod route;
pub mod util;

pub use clarify::ClarifyPhase;
pub use critique::CritiquePhase;
pub use deliver::DeliverPhase;
pub use gate::GatePhase;
pub use intake::IntakePhase;
pub use judge::JudgePhase;
pub use phase::{Phase, PhaseOutput, RunContext};
pub use pipe::Pipeline;
pub use propose::ProposePhase;
pub use rank::RankPhase;
pub use repair::RepairPhase;
pub use route::RoutePhase;
