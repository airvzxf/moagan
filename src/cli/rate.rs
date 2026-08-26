//! `moagan rate <run_id> <proposal_id> <score>` — manually rate a
//! proposal. PR C.5 (K.3b), companion to the preference cache
//! integration in `preferences::integration`.

use std::str::FromStr;

use tracing::{debug, info, warn};

use crate::cli::RateArgs;
use crate::error::{Error, Result};
use crate::ids::RunId;
use crate::preferences::cache::Rating;
use crate::preferences::integration;

/// Execute the `rate` sub-command. Resolves the run id, parses and
/// bounds-checks the score, then forwards the rating to the
/// integration layer for persistence.
pub fn run(args: RateArgs) -> Result<i32> {
    debug!(
        run_id = %args.run_id,
        proposal_id = %args.proposal_id,
        score = %args.score,
        "rate::run: enter"
    );
    let user = std::env::var("MOAGAN_USER").unwrap_or_else(|_| "default".into());
    let run_id = RunId::from_str(&args.run_id)
        .map_err(|e| Error::InvalidArgs(format!("invalid run_id '{}': {e}", args.run_id)))?;
    let score: f64 = args
        .score
        .parse()
        .map_err(|e| Error::InvalidArgs(format!("invalid score '{}': {e}", args.score)))?;
    if !(0.0..=1.0).contains(&score) {
        warn!(score, "rate: score out of range");
        return Err(Error::InvalidArgs(format!(
            "score must be in [0.0, 1.0], got {score}"
        )));
    }
    let rating = Rating {
        proposal_id: args.proposal_id.clone(),
        score,
        rated_unix: crate::preferences::cache::unix_now(),
        run_id,
    };
    integration::record_user_rating(&user, rating)?;
    println!(
        "rated {} = {:.2} for run {}",
        args.proposal_id, score, run_id
    );
    info!(run_id = %run_id, proposal_id = %args.proposal_id, score, "rate: recorded");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_MOAGAN_HOME_LOCK;
    use crate::ids::RunId;
    use crate::preferences::PreferenceCache;
    use std::path::PathBuf;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "moagan-rate-test-{}-{}",
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

    /// A valid (run_id, proposal_id, score) tuple round-trips
    /// through the cache: a subsequent `PreferenceCache::load`
    /// exposes the new rating.
    #[test]
    fn cli_rate_persists_rating() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let tmp = unique_tmp("persist");
        set_env("MOAGAN_HOME", Some(tmp.to_str().unwrap()));
        set_env("MOAGAN_LEARNING", Some("true"));
        set_env("MOAGAN_USER", Some("alice"));

        let run_id = RunId::new();
        let args = RateArgs {
            run_id: run_id.to_string(),
            proposal_id: "p_alpha".into(),
            score: "0.75".into(),
        };
        let code = run(args).unwrap();
        assert_eq!(code, 0);

        let cache = PreferenceCache::load("alice");
        assert_eq!(cache.ratings.len(), 1);
        assert_eq!(cache.ratings[0].proposal_id, "p_alpha");
        assert!(
            (cache.ratings[0].score - 0.75).abs() < 1e-9,
            "score must round-trip as 0.75, got {}",
            cache.ratings[0].score
        );
        assert_eq!(cache.ratings[0].run_id, run_id);

        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
        set_env("MOAGAN_USER", prev_user.as_deref());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Scores outside `[0.0, 1.0]` must surface as
    /// `Error::InvalidArgs` so the CLI exits with code 2 instead of
    /// silently storing garbage.
    #[test]
    fn cli_rate_rejects_score_out_of_range() {
        let _g = TEST_MOAGAN_HOME_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_user = std::env::var("MOAGAN_USER").ok();
        let prev_learning = std::env::var("MOAGAN_LEARNING").ok();
        let prev_home = std::env::var("MOAGAN_HOME").ok();
        set_env("MOAGAN_USER", Some("alice"));
        set_env("MOAGAN_LEARNING", Some("true"));
        set_env("MOAGAN_HOME", None);

        let run_id = RunId::new();
        let above = RateArgs {
            run_id: run_id.to_string(),
            proposal_id: "p_alpha".into(),
            score: "1.5".into(),
        };
        match run(above) {
            Err(Error::InvalidArgs(msg)) => {
                assert!(
                    msg.contains("[0.0, 1.0]"),
                    "error must mention the valid range, got: {msg}"
                );
            }
            other => panic!("expected InvalidArgs above 1.0, got {other:?}"),
        }

        let below = RateArgs {
            run_id: run_id.to_string(),
            proposal_id: "p_alpha".into(),
            score: "-0.1".into(),
        };
        assert!(matches!(run(below), Err(Error::InvalidArgs(_))));

        let garbage = RateArgs {
            run_id: run_id.to_string(),
            proposal_id: "p_alpha".into(),
            score: "not-a-number".into(),
        };
        assert!(matches!(run(garbage), Err(Error::InvalidArgs(_))));

        set_env("MOAGAN_USER", prev_user.as_deref());
        set_env("MOAGAN_LEARNING", prev_learning.as_deref());
        set_env("MOAGAN_HOME", prev_home.as_deref());
    }
}
