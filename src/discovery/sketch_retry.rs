//! D.34.1: per-sketch retry helper. Wraps a sketch extraction
//! call with exponential backoff. Caller is `DiscoverPhase`;
//! the implementer chooses how many retries (default 2).

use crate::error::Result;
use std::time::Duration;

/// Run a sketch-extraction closure with bounded retries and
/// exponential backoff.
///
/// `op` is invoked up to `max_retries + 1` times. The first
/// successful call short-circuits; persistent failures surface as
/// the final `Err` after the retry budget is exhausted. Backoff
/// grows as `100 * 2^attempt` ms (clamped to avoid overflow).
pub async fn retry_sketch_extraction<F, Fut, T>(max_retries: u32, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    tracing::debug!(max_retries, "sketch_retry: enter (async)");
    let mut attempt: u32 = 0;
    loop {
        tracing::trace!(attempt, "sketch_retry: attempting op");
        match op().await {
            Ok(v) => {
                tracing::debug!(attempt, "sketch_retry: ok");
                return Ok(v);
            }
            Err(e) if attempt >= max_retries => {
                tracing::error!(
                    attempt,
                    max_retries,
                    error = %e,
                    "sketch_retry: exhausted budget; surfacing error"
                );
                return Err(e);
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "sketch extraction failed; retrying");
                attempt = attempt.saturating_add(1);
                let backoff_ms = 100u64.saturating_mul(1u64 << attempt.min(20));
                tracing::trace!(
                    attempt,
                    backoff_ms,
                    "sketch_retry: sleeping before next attempt"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retry_sketch_extraction_succeeds_after_first_failure() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_inner = Arc::clone(&calls);
        let result: Result<&'static str> = retry_sketch_extraction(2, move || {
            let calls = Arc::clone(&calls_inner);
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(Error::Provider {
                        message: "transient".into(),
                        http_status: None,
                    })
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_sketch_extraction_gives_up_after_max_retries() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_inner = Arc::clone(&calls);
        let result: Result<&'static str> = retry_sketch_extraction(1, move || {
            let calls = Arc::clone(&calls_inner);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::Provider {
                    message: "permanent".into(),
                    http_status: None,
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
