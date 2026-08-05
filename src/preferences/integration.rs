//! Integration of [`PreferenceCache`] with the Synthesize phase and
//! the `moagan rate` sub-command. PR C.5 (K.3b).
//!
//! Two entry points live here:
//!
//! * [`render_preferences_block`] — Markdown snippet of the user's
//!   top-N recent ratings, used to fill the `${epistemic_preferences}`
//!   placeholder that synthesize prompts may embed.
//! * [`auto_record_run`] — append a neutral (`score = 0.5`) rating
//!   for every proposal of a completed run, no-op when the learning
//!   loop is opted out.
//! * [`record_user_rating`] — append a single user-provided rating
//!   from `moagan rate`.

use crate::ids::RunId;
use crate::preferences::cache::{PreferenceCache, Rating, unix_now};

/// Returns a Markdown snippet of the user's top-N recent ratings for
/// prompt injection. Returns an empty string when the learning loop
/// is disabled or the cache holds no ratings, so prompts without a
/// usable history see no substitution at all.
pub fn render_preferences_block(user: &str, limit: usize) -> String {
    if !PreferenceCache::enabled() {
        return String::new();
    }
    let cache = PreferenceCache::load(user);
    if cache.ratings.is_empty() {
        return String::new();
    }
    let mut s = String::from("# User preferences\n\n");
    s.push_str("Recent ratings (weighted by recency):\n");
    for r in cache.recent(limit) {
        s.push_str(&format!(
            "- {} (score {:.2}, run {})\n",
            r.proposal_id, r.score, r.run_id
        ));
    }
    s
}

/// Auto-record a neutral (`score = 0.5`) rating for every proposal
/// of a completed run. No-op when the learning loop is opted out so
/// the synthesis path stays side-effect-free by default.
pub fn auto_record_run(user: &str, run_id: RunId, proposal_ids: &[String]) {
    if !PreferenceCache::enabled() {
        return;
    }
    let mut cache = PreferenceCache::load(user);
    let now = unix_now();
    for pid in proposal_ids {
        cache.add(Rating {
            proposal_id: pid.clone(),
            score: 0.5,
            rated_unix: now,
            run_id,
        });
    }
    let _ = cache.save();
}

/// Manual user rating via `moagan rate`. Surfaces persistence errors
/// so the CLI can exit non-zero instead of swallowing a write
/// failure.
pub fn record_user_rating(user: &str, rating: Rating) -> Result<(), crate::error::Error> {
    let mut cache = PreferenceCache::load(user);
    cache.add(rating);
    cache.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_MOAGAN_HOME_LOCK;
    use crate::ids::RunId;
    use std::path::PathBuf;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "moagan-prefs-integ-test-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn set_env(key: &str, value: Option<&str>) {
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    /// Reading the preferences block when the learning loop is
    /// opted out must yield an empty string regardless of any
    /// persisted cache on disk.
    #[test]
    fn render_preferences_block_returns_empty_when_disabled() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let tmp = unique_tmp("disabled");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", None);
        assert!(!PreferenceCache::enabled());

        let block = render_preferences_block("alice", 3);
        assert!(block.is_empty(), "disabled loop must yield empty block");

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// When learning is enabled but the cache is empty, the block
    /// must also be empty so a brand-new user gets no synthetic
    /// noise in their prompt.
    #[test]
    fn render_preferences_block_returns_empty_when_no_ratings() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let tmp = unique_tmp("empty");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", Some("true"));

        let block = render_preferences_block("alice", 3);
        assert!(
            block.is_empty(),
            "empty cache must yield empty block, got: {block:?}"
        );

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Three ratings → the top three should appear in the rendered
    /// block, with the proposal id and the score formatted to two
    /// decimals.
    #[test]
    fn render_preferences_block_includes_top_ratings() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let tmp = unique_tmp("top");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", Some("true"));
        assert!(PreferenceCache::enabled());

        let run_id = RunId::new();
        let mut cache = PreferenceCache::load("alice");
        let now = unix_now();
        cache.add(Rating {
            proposal_id: "p_alpha".into(),
            score: 0.9,
            rated_unix: now,
            run_id,
        });
        cache.add(Rating {
            proposal_id: "p_beta".into(),
            score: 0.4,
            rated_unix: now,
            run_id,
        });
        cache.add(Rating {
            proposal_id: "p_gamma".into(),
            score: 0.7,
            rated_unix: now,
            run_id,
        });
        cache
            .save()
            .expect("save must succeed under TEST_MOAGAN_HOME_LOCK");

        let block = render_preferences_block("alice", 3);
        assert!(
            block.contains("# User preferences"),
            "block must contain the heading, got: {block:?}"
        );
        assert!(block.contains("p_alpha"));
        assert!(block.contains("p_beta"));
        assert!(block.contains("p_gamma"));
        assert!(block.contains("0.90"), "score must format to 2 decimals");

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `auto_record_run` must be a no-op when learning is disabled —
    /// no file should be written and no error should bubble up.
    #[test]
    fn auto_record_run_is_noop_when_disabled() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let tmp = unique_tmp("noop");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", None);
        assert!(!PreferenceCache::enabled());

        auto_record_run("alice", RunId::new(), &["p_one".into(), "p_two".into()]);

        let prefs_dir = tmp.join("preferences");
        assert!(
            !prefs_dir.exists(),
            "auto_record_run must not create a preferences dir when disabled"
        );

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
