//! Execution layer. Concurrency control and timeouts. Phases obtain a
//! [`Parallelism`] permit before launching any LLM call.

pub mod parallelism;

pub use parallelism::{Parallelism, Permit, PermitsGuard};
