//! Per-user preference cache for the optional learning loop.
//!
//! Opt-in via `MOAGAN_LEARNING=true` (default false). When
//! disabled, every operation is a no-op. Persisted as JSON to
//! `<MOAGAN_HOME>/preferences/<user>.json` with linear decay
//! (90-day half-life) and a hard cap of 1000 ratings per user.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, IoError, Result};
use crate::ids::RunId;

/// On-disk schema version. Bumped on breaking changes; on a
/// mismatch the persisted state is silently discarded (an operator
/// who upgrades gets a fresh cache rather than a half-loaded one).
pub const SCHEMA_VERSION: u32 = 1;

/// Half-life of a rating in days. Older ratings are retained with
/// linearly decreasing weight and dropped entirely once they pass
/// this age during a [`PreferenceCache::decay`] sweep.
pub const DECAY_DAYS: u64 = 90;

/// Hard cap on the number of ratings kept per user. The oldest
/// entries are drained first when the cap is exceeded.
pub const MAX_RATINGS: usize = 1000;

/// Seconds in a single day, used to convert [`DECAY_DAYS`] into the
/// same units as `SystemTime`.
const SECONDS_PER_DAY: u64 = 86_400;

/// A single user rating for one proposal within one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rating {
    /// Identifier of the rated proposal (e.g. `p_001`).
    pub proposal_id: String,
    /// Score between `0.0` and `1.0`; `0.0` = worst, `1.0` = best.
    pub score: f64,
    /// Unix timestamp (seconds) when the rating was recorded.
    pub rated_unix: i64,
    /// Run that produced the rated proposal.
    pub run_id: RunId,
}

/// Per-user preference cache. Serialized to JSON at
/// `<MOAGAN_HOME>/preferences/<user>.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PreferenceCache {
    /// Schema version — see [`SCHEMA_VERSION`].
    pub version: u32,
    /// User this cache belongs to (typically `MOAGAN_USER`).
    pub user: String,
    /// Recorded ratings, in insertion order.
    pub ratings: Vec<Rating>,
    /// Unix timestamp of the last [`PreferenceCache::decay`] sweep.
    pub last_decay_unix: i64,
}

impl PreferenceCache {
    /// Construct an empty cache for `user` stamped with the current
    /// [`SCHEMA_VERSION`].
    pub fn empty(user: String) -> Self {
        Self {
            version: SCHEMA_VERSION,
            user,
            ratings: Vec::new(),
            last_decay_unix: unix_now(),
        }
    }

    /// Whether the learning loop is opted in. Reads
    /// `MOAGAN_LEARNING` from the environment and accepts the
    /// canonical truthy spellings (`1`, `true`, `yes`, `on`,
    /// case-insensitive).
    pub fn enabled() -> bool {
        std::env::var("MOAGAN_LEARNING")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    }

    /// Load the persisted cache for `user`. Returns
    /// [`PreferenceCache::empty`] on any failure (opt-out, missing
    /// file, schema mismatch, corrupt JSON, user mismatch) so a
    /// malformed cache never blocks the rest of the pipeline.
    pub fn load(user: &str) -> Self {
        if !Self::enabled() {
            return Self::empty(user.to_string());
        }
        let Some(path) = cache_path(user) else {
            return Self::empty(user.to_string());
        };
        if !path.exists() {
            return Self::empty(user.to_string());
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(mut cache) => {
                    if cache.version != SCHEMA_VERSION {
                        return Self::empty(user.to_string());
                    }
                    if cache.user != user {
                        return Self::empty(user.to_string());
                    }
                    cache.decay();
                    cache
                }
                Err(_) => Self::empty(user.to_string()),
            },
            Err(_) => Self::empty(user.to_string()),
        }
    }

    /// Persist the cache to its canonical path. No-op when the
    /// learning loop is opted out. Writes atomically via a
    /// `tmp + rename` so a partial write never leaves a half-baked
    /// JSON on disk.
    pub fn save(&self) -> Result<()> {
        if !Self::enabled() {
            return Ok(());
        }
        let Some(path) = cache_path(&self.user) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Io(IoError::Write {
                    path: parent.to_path_buf(),
                    source: e,
                })
            })?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).map_err(|e| {
            Error::Io(IoError::Write {
                path: tmp.clone(),
                source: e,
            })
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            Error::Io(IoError::Write {
                path: path.clone(),
                source: e,
            })
        })?;
        Ok(())
    }

    /// Append a rating. No-op when opted out. When the cap
    /// ([`MAX_RATINGS`]) is exceeded the oldest entries are
    /// dropped from the front of the vector to keep the bound.
    pub fn add(&mut self, rating: Rating) {
        if !Self::enabled() {
            return;
        }
        self.ratings.push(rating);
        if self.ratings.len() > MAX_RATINGS {
            let drop = self.ratings.len() - MAX_RATINGS;
            self.ratings.drain(0..drop);
        }
    }

    /// Return the `limit` ratings with the highest linear-decay
    /// weight, sorted by weight descending. Ratings past the
    /// [`DECAY_DAYS`] horizon (weight `<= 0`) are filtered out.
    pub fn recent(&self, limit: usize) -> Vec<&Rating> {
        let now = unix_now();
        let decay_secs = DECAY_DAYS * SECONDS_PER_DAY;
        let mut scored: Vec<(f64, &Rating)> = self
            .ratings
            .iter()
            .filter_map(|r| {
                let age = (now - r.rated_unix).max(0) as f64;
                let weight = 1.0 - (age / decay_secs as f64).min(1.0);
                if weight > 0.0 {
                    Some((weight, r))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(limit).map(|(_, r)| r).collect()
    }

    /// Drop ratings older than [`DECAY_DAYS`] and stamp
    /// `last_decay_unix` with the current time.
    pub fn decay(&mut self) {
        let now = unix_now();
        let decay_secs = DECAY_DAYS * SECONDS_PER_DAY;
        self.ratings.retain(|r| {
            let age = (now - r.rated_unix).max(0) as u64;
            age < decay_secs
        });
        self.last_decay_unix = now;
    }
}

/// Resolve the on-disk path for `user`'s cache. Prefers
/// `MOAGAN_HOME`; falls back to `~/.local/share/moagan`. Returns
/// `None` when neither is set.
fn cache_path(user: &str) -> Option<PathBuf> {
    let home = std::env::var("MOAGAN_HOME").ok().or_else(|| {
        std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/.local/share/moagan"))
    })?;
    Some(
        PathBuf::from(home)
            .join("preferences")
            .join(format!("{user}.json")),
    )
}

/// Current Unix time in seconds. Returns `0` if the system clock
/// is before the epoch — the cache then treats every rating as
/// "fresh", which is a safe over-approximation. Exposed so other
/// modules (e.g. the auto-record path in
/// `preferences::integration`) can stamp `Rating::rated_unix` with
/// the same clock the cache uses internally.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_MOAGAN_HOME_LOCK;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "moagan-prefs-test-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rating(score: f64, rated_unix: i64) -> Rating {
        Rating {
            proposal_id: format!("p_{rated_unix}_{score}"),
            score,
            rated_unix,
            run_id: RunId::new(),
        }
    }

    #[test]
    fn cache_empty_initializes_with_schema() {
        let cache = PreferenceCache::empty("alice".into());
        assert_eq!(cache.version, SCHEMA_VERSION);
        assert_eq!(cache.version, 1);
        assert_eq!(cache.user, "alice");
        assert!(cache.ratings.is_empty());
        assert!(cache.last_decay_unix > 0);
    }

    #[test]
    fn cache_disabled_is_noop() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var("MOAGAN_LEARNING").ok();
        unsafe {
            std::env::remove_var("MOAGAN_LEARNING");
        }
        assert!(!PreferenceCache::enabled());

        let mut cache = PreferenceCache::empty("alice".into());
        cache.add(rating(0.5, unix_now()));
        assert!(
            cache.ratings.is_empty(),
            "add() must be a no-op when MOAGAN_LEARNING is unset"
        );

        cache.save().expect("save() must be a no-op success");
        let loaded = PreferenceCache::load("alice");
        assert!(loaded.ratings.is_empty());

        if let Some(v) = previous {
            unsafe {
                std::env::set_var("MOAGAN_LEARNING", v);
            }
        }
    }

    #[test]
    fn cache_save_then_load_round_trip() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous_home = std::env::var("MOAGAN_HOME").ok();
        let previous_learning = std::env::var("MOAGAN_LEARNING").ok();
        let tmp = unique_tmp("roundtrip");
        unsafe {
            std::env::set_var("MOAGAN_HOME", &tmp);
            std::env::set_var("MOAGAN_LEARNING", "true");
        }
        assert!(PreferenceCache::enabled());

        let mut cache = PreferenceCache::empty("alice".into());
        cache.add(rating(0.8, unix_now()));
        cache.add(rating(0.3, unix_now() - 10));
        cache.save().expect("save should succeed");

        let loaded = PreferenceCache::load("alice");
        assert_eq!(loaded.version, SCHEMA_VERSION);
        assert_eq!(loaded.user, "alice");
        assert_eq!(loaded.ratings.len(), 2);

        match previous_home {
            Some(v) => unsafe {
                std::env::set_var("MOAGAN_HOME", v);
            },
            None => unsafe {
                std::env::remove_var("MOAGAN_HOME");
            },
        }
        match previous_learning {
            Some(v) => unsafe {
                std::env::set_var("MOAGAN_LEARNING", v);
            },
            None => unsafe {
                std::env::remove_var("MOAGAN_LEARNING");
            },
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cache_recent_filters_by_decay_weight() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let now = unix_now();
        let day = SECONDS_PER_DAY as i64;
        let cache = PreferenceCache {
            version: SCHEMA_VERSION,
            user: "alice".into(),
            ratings: vec![
                rating(0.5, now),
                rating(0.9, now - 5 * day),
                rating(0.7, now - 40 * day),
            ],
            last_decay_unix: now,
        };
        let recent = cache.recent(10);
        assert_eq!(recent.len(), 3, "all three ratings are within 90 days");
        assert_eq!(recent[0].rated_unix, now);
        assert!(
            recent[0].rated_unix > recent[1].rated_unix,
            "expected recent[0] newer than recent[1]"
        );
        assert!(
            recent[1].rated_unix > recent[2].rated_unix,
            "expected recent[1] newer than recent[2]"
        );
    }

    #[test]
    fn cache_decay_removes_old_ratings() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let now = unix_now();
        let day = SECONDS_PER_DAY as i64;
        let mut cache = PreferenceCache {
            version: SCHEMA_VERSION,
            user: "alice".into(),
            ratings: vec![
                rating(0.5, now),
                rating(0.4, now - 100 * day),
                rating(0.3, now - 200 * day),
            ],
            last_decay_unix: now,
        };
        cache.decay();
        assert_eq!(cache.ratings.len(), 1, "only the fresh rating survives");
        assert_eq!(cache.ratings[0].score, 0.5);
        assert_eq!(cache.last_decay_unix, now);
    }

    #[test]
    fn cache_caps_at_max_ratings() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let previous_learning = std::env::var("MOAGAN_LEARNING").ok();
        unsafe {
            std::env::set_var("MOAGAN_LEARNING", "true");
        }

        let mut cache = PreferenceCache::empty("alice".into());
        let base = unix_now();
        for i in 0..(MAX_RATINGS + 50) {
            cache.add(rating(0.5, base + i as i64));
        }
        assert_eq!(
            cache.ratings.len(),
            MAX_RATINGS,
            "cache must hard-cap at MAX_RATINGS"
        );
        let first_kept = cache.ratings.first().unwrap().rated_unix;
        assert_eq!(
            first_kept,
            base + 50,
            "oldest 50 entries should have been drained"
        );

        match previous_learning {
            Some(v) => unsafe {
                std::env::set_var("MOAGAN_LEARNING", v);
            },
            None => unsafe {
                std::env::remove_var("MOAGAN_LEARNING");
            },
        }
    }
}
