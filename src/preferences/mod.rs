//! Per-user preference cache (opt-in learning loop). See
//! [`PreferenceCache`] for the on-disk shape and the opt-in gate.

pub mod cache;
pub use cache::{PreferenceCache, Rating};
