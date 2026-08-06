//! Execution layer. Concurrency control and timeouts. Phases obtain a
//! [`Parallelism`] permit before launching any LLM call.

pub mod parallelism;
pub mod per_provider_semaphores;

pub use parallelism::{Parallelism, Permit, PermitsGuard};
pub use per_provider_semaphores::PerProviderSemaphores;
