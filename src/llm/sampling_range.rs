//! What a probed sampling table sends for a requested value (#995).
//!
//! The `top_p` / `top_k` auto-probes walk a fixed candidate set and
//! record the smallest accepted value and, since #995, the largest.
//! With both ends known, a requested value is clamped into that range.
//! With only one probed point (rows written before #995) nothing is
//! known about the other values, so the request goes out unchanged:
//! one probed point never overrides what a role asks for.

/// The `top_p` to send for `requested`, given the smallest accepted
/// value `min` and, when the probe recorded it, the largest `max`:
/// `requested` clamped into `[min, max]`. `requested` is returned
/// unchanged when `max` is `None` or the range is inverted.
pub fn clamp_f32(requested: f32, min: f32, max: Option<f32>) -> f32 {
    match max {
        Some(max) if min <= max => requested.clamp(min, max),
        _ => requested,
    }
}

/// The `top_k` counterpart of [`clamp_f32`], same rules.
pub fn clamp_u32(requested: u32, min: u32, max: Option<u32>) -> u32 {
    match max {
        Some(max) if min <= max => requested.clamp(min, max),
        _ => requested,
    }
}

#[cfg(test)]
mod tests;
