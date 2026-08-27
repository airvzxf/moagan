//! Reusable `serde` helpers that preserve operator-visible precision
//! across every JSON / TOML boundary in the crate.
//!
//! The only occupant today is [`clean_f32`] — Ryu-based
//! shortest-round-trip serialisation for `f32` (so a `Vec<f32>`
//! carrying `0.1` round-trips as `0.1`, not the
//! `0.10000000149011612` form `Display::fmt` produces by default).

pub mod clean_f32;
