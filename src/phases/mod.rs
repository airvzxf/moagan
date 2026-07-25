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
pub mod util;

pub use phase::{Phase, PhaseOutput, RunContext};
pub use pipe::Pipeline;
