//! D.13.9: configurable threshold for the tagger.

/// Default minimum similarity for the tagger to accept a tag.
/// Anything below this falls into [`crate::discovery::tag_decision::TagDecision::Uncategorized`].
pub const DEFAULT_TAGGER_THRESHOLD: f32 = 0.6;

/// Wraps a tagger threshold so configuration can validate the
/// value before it reaches the tagger.
#[derive(Debug, Clone, Copy)]
pub struct TaggerThreshold {
    /// Threshold value in `[0.0, 1.0]`.
    pub value: f32,
}

impl Default for TaggerThreshold {
    fn default() -> Self {
        Self {
            value: DEFAULT_TAGGER_THRESHOLD,
        }
    }
}

impl TaggerThreshold {
    /// Read a threshold from an optional config value. Out-of-range
    /// (`< 0`, `> 1`) and `None` both fall back to the default; this
    /// keeps the tagger safe when the config schema is missing or
    /// partially populated.
    pub fn from_config_value(v: Option<f32>) -> Self {
        match v {
            Some(v) if (0.0..=1.0).contains(&v) => Self { value: v },
            _ => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagger_threshold_default_is_06() {
        let t = TaggerThreshold::default();
        assert!((t.value - DEFAULT_TAGGER_THRESHOLD).abs() < 1e-6);
        assert!((t.value - 0.6).abs() < 1e-6);
    }

    #[test]
    fn tagger_threshold_from_config_validates_range() {
        let t = TaggerThreshold::from_config_value(Some(0.42));
        assert!((t.value - 0.42).abs() < 1e-6);

        let t = TaggerThreshold::from_config_value(Some(0.0));
        assert!((t.value - 0.0).abs() < 1e-6);

        let t = TaggerThreshold::from_config_value(Some(1.0));
        assert!((t.value - 1.0).abs() < 1e-6);

        let t = TaggerThreshold::from_config_value(Some(-0.1));
        assert!((t.value - DEFAULT_TAGGER_THRESHOLD).abs() < 1e-6);

        let t = TaggerThreshold::from_config_value(Some(1.5));
        assert!((t.value - DEFAULT_TAGGER_THRESHOLD).abs() < 1e-6);

        let t = TaggerThreshold::from_config_value(None);
        assert!((t.value - DEFAULT_TAGGER_THRESHOLD).abs() < 1e-6);
    }
}
