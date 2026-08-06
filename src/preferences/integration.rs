//! Integration of [`PreferenceCache`] with the Synthesize phase and
//! the `moagan rate` sub-command. PR C.5 (K.3b).
//!
//! Three entry points live here:
//!
//! * [`render_preferences_block`] — Markdown snippet of the user's
//!   top-N recent ratings, used to fill the `${epistemic_preferences}`
//!   placeholder that synthesize prompts may embed.
//! * [`auto_record_run`] — append a neutral (`score = 0.5`) rating
//!   for every proposal of a completed run, no-op when the learning
//!   loop is opted out.
//! * [`record_user_rating`] — append a single user-provided rating
//!   from `moagan rate`.
//!
//! PR D.8 wires the Synthesize phase into this module: it reads
//! [`MOAGAN_USER`] + [`PreferenceCache::enabled`] at phase start
//! via [`inject_preferences_into_prompt`], and calls
//! [`auto_record_run`] on phase completion with the synthesised
//! proposal ids.

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

/// Substitute the `${epistemic_preferences}` placeholder in
/// `prompt` with the rendered Markdown view of the current user's
/// top-3 ratings. The user is resolved from [`MOAGAN_USER`]; when
/// the env var is unset, the learning loop is opted out, or the
/// cache is empty, `prompt` is returned unchanged so prompts
/// without usable history see no substitution at all. PR D.8.
pub fn inject_preferences_into_prompt(prompt: &str) -> String {
    let Ok(user) = std::env::var("MOAGAN_USER") else {
        return prompt.to_owned();
    };
    if user.is_empty() {
        return prompt.to_owned();
    }
    if !PreferenceCache::enabled() {
        return prompt.to_owned();
    }
    crate::llm::prompts::inject_epistemic_preferences(prompt, &user)
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

    /// PR D.8: when the learning loop is enabled AND the cache has
    /// recent ratings AND `MOAGAN_USER` is set, the helper must
    /// substitute `${epistemic_preferences}` in the prompt.
    #[test]
    fn inject_preferences_into_prompt_substitutes_when_enabled_and_user_set() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let tmp = unique_tmp("inject_enabled");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", Some("true"));
        set_env("MOAGAN_USER", Some("alice"));
        assert!(PreferenceCache::enabled());

        let run_id = RunId::new();
        let mut cache = PreferenceCache::load("alice");
        cache.add(Rating {
            proposal_id: "p_alpha".into(),
            score: 0.9,
            rated_unix: unix_now(),
            run_id,
        });
        cache.save().expect("save must succeed");

        let prompt = "Hello\n${epistemic_preferences}\nWorld";
        let out = inject_preferences_into_prompt(prompt);
        assert!(
            !out.contains("${epistemic_preferences}"),
            "placeholder must be replaced, got: {out:?}"
        );
        assert!(out.contains("p_alpha"), "block must include the rating");
        assert!(out.starts_with("Hello\n"));
        assert!(out.ends_with("\nWorld"));

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        set_env("MOAGAN_USER", prev_user.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR D.8: when `MOAGAN_USER` is unset (regardless of whether
    /// the learning loop is enabled), the helper must return the
    /// prompt unchanged so anonymous runs see no substitution.
    #[test]
    fn inject_preferences_into_prompt_returns_unchanged_without_user() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let tmp = unique_tmp("inject_no_user");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", Some("true"));
        set_env("MOAGAN_USER", None);

        let prompt = "Hello\n${epistemic_preferences}\nWorld";
        let out = inject_preferences_into_prompt(prompt);
        assert_eq!(out, prompt, "missing user must yield unchanged prompt");

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        set_env("MOAGAN_USER", prev_user.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR D.8: when the learning loop is opted out (regardless of
    /// `MOAGAN_USER`), the helper must return the prompt unchanged.
    #[test]
    fn inject_preferences_into_prompt_returns_unchanged_when_disabled() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let tmp = unique_tmp("inject_disabled");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", None);
        set_env("MOAGAN_USER", Some("alice"));

        let prompt = "Hello\n${epistemic_preferences}\nWorld";
        let out = inject_preferences_into_prompt(prompt);
        assert_eq!(out, prompt, "disabled loop must yield unchanged prompt");

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        set_env("MOAGAN_USER", prev_user.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// PR D.8: `auto_record_run` must persist a neutral
    /// (`score = 0.5`) rating for every proposal id supplied when
    /// the learning loop is enabled, and the resulting cache must
    /// round-trip through `PreferenceCache::load`.
    #[test]
    fn auto_record_run_persists_neutral_ratings_when_enabled() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let tmp = unique_tmp("auto_record");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", Some("true"));
        assert!(PreferenceCache::enabled());

        let run_id = RunId::new();
        let proposal_ids = vec![
            "p_alpha".to_string(),
            "p_beta".to_string(),
            "p_gamma".to_string(),
        ];
        auto_record_run("alice", run_id, &proposal_ids);

        let loaded = PreferenceCache::load("alice");
        assert_eq!(
            loaded.ratings.len(),
            3,
            "every supplied proposal id must be recorded"
        );
        let by_id: std::collections::HashMap<String, f64> = loaded
            .ratings
            .iter()
            .map(|r| (r.proposal_id.clone(), r.score))
            .collect();
        for pid in &proposal_ids {
            assert!(
                (by_id[pid] - 0.5).abs() < 1e-9,
                "rating for {pid} must be 0.5, got {}",
                by_id[pid]
            );
        }
        for r in &loaded.ratings {
            assert_eq!(r.run_id, run_id, "rating must carry the run id");
        }

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
