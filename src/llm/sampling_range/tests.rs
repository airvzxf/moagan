//! Clamping a requested sampling value into the probed range (#995).

use super::*;

#[test]
fn a_value_inside_the_probed_range_is_sent_unchanged() {
    assert_eq!(clamp_f32(0.95, 0.05, Some(1.0)), 0.95);
    assert_eq!(clamp_f32(0.05, 0.05, Some(1.0)), 0.05);
    assert_eq!(clamp_u32(40, 1, Some(1024)), 40);
}

#[test]
fn a_value_outside_the_probed_range_is_clamped_to_the_nearest_end() {
    assert_eq!(clamp_f32(0.01, 0.05, Some(1.0)), 0.05);
    assert_eq!(clamp_f32(1.5, 0.05, Some(0.9)), 0.9);
    assert_eq!(clamp_u32(0, 1, Some(1024)), 1);
    assert_eq!(clamp_u32(5000, 1, Some(1024)), 1024);
}

#[test]
fn one_probed_point_does_not_override_the_requested_value() {
    // MiniMax-M3 rows written before #995: `top_p = 0.05`, `top_k = 1`.
    assert_eq!(clamp_f32(0.95, 0.05, None), 0.95);
    assert_eq!(clamp_f32(0.01, 0.05, None), 0.01);
    assert_eq!(clamp_u32(40, 1, None), 40);
}

#[test]
fn an_inverted_range_does_not_override_the_requested_value() {
    assert_eq!(clamp_f32(0.5, 0.9, Some(0.1)), 0.5);
    assert_eq!(clamp_u32(40, 64, Some(8)), 40);
}
