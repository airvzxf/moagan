//! Pacing for the auto-probe requests (#1068).
//!
//! A model with no cached probe values gets the `max_tokens` search
//! (rounds of 20 parallel requests) plus the temperature, `top_p` and
//! `top_k` sweeps, all started at once. On a token-plan provider that
//! burst trips the rate limit (MiniMax answers HTTP 429, code 2062),
//! and every throttled request used to count as a rejection: a
//! `max_tokens` search cut short stores a ceiling far too low, which
//! requests are then capped at (#1059), and a throttled temperature
//! is stored as unsupported.
//!
//! [`ProbeGate`] caps how many probe requests one run has in flight,
//! across every probe kind and pair. [`ProbePacer`] sends one probe
//! request through the gate and retries it with backoff while the
//! provider throttles; when it gives up it remembers that, so the
//! caller stores nothing measured under throttling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::{error::Elapsed, timeout};

use crate::error::{Error, Result};
use crate::llm::client::{LlmClient, LlmRequest, LlmResponse};

/// Probe requests one run keeps in flight at most.
pub const PROBE_CONCURRENCY: usize = 4;

/// Retries of a throttled probe request before the pacer gives up.
pub const PROBE_THROTTLE_RETRIES: u32 = 5;

/// First backoff after a throttled probe request without
/// `Retry-After`; it doubles on each retry up to
/// [`PROBE_THROTTLE_MAX_WAIT`].
const PROBE_THROTTLE_BASE_WAIT: Duration = Duration::from_secs(2);

/// Longest single wait between retries of a throttled probe request.
const PROBE_THROTTLE_MAX_WAIT: Duration = Duration::from_secs(60);

/// Limit on the probe requests in flight, shared by every probe
/// transport of one run.
#[derive(Debug, Clone)]
pub struct ProbeGate {
    permits: Arc<Semaphore>,
}

impl ProbeGate {
    /// A gate that lets `concurrency` probe requests run at once
    /// (at least one).
    pub fn new(concurrency: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }
}

impl Default for ProbeGate {
    fn default() -> Self {
        Self::new(PROBE_CONCURRENCY)
    }
}

/// Sends the probe requests of one transport: through the run's
/// [`ProbeGate`] when it has one, retrying while the provider
/// throttles.
#[derive(Debug, Clone, Default)]
pub struct ProbePacer {
    gate: Option<ProbeGate>,
    gave_up: Arc<AtomicBool>,
}

impl ProbePacer {
    /// A pacer whose requests wait for a permit of `gate`.
    pub fn with_gate(gate: ProbeGate) -> Self {
        Self {
            gate: Some(gate),
            gave_up: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether a probe request ended throttled after every retry (or
    /// on an exhausted plan). A probe whose pacer gave up must not
    /// store its result.
    pub fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::SeqCst)
    }

    /// Send `req` as a probe, each attempt bounded by `per_try`.
    /// A throttled answer (`Error::Throttled` or HTTP 429) is retried
    /// after the provider's `Retry-After`, else after a doubling
    /// backoff; an exhausted plan is not retried. The last answer is
    /// returned as it came.
    pub async fn send(
        &self,
        client: &dyn LlmClient,
        req: &LlmRequest,
        per_try: Duration,
    ) -> std::result::Result<Result<LlmResponse>, Elapsed> {
        let mut attempt: u32 = 0;
        loop {
            let res = {
                // A closed semaphore cannot happen (it is never
                // closed); sending without a permit is the safe
                // fallback.
                let _permit = match self.gate.as_ref() {
                    Some(g) => g.permits.acquire().await.ok(),
                    None => None,
                };
                timeout(per_try, client.send_probe(req)).await
            };
            let wait = match throttle_of(&res) {
                Throttle::None => return res,
                Throttle::PlanExhausted => {
                    self.give_up(client, "plan exhausted");
                    return res;
                }
                Throttle::RetryAfter(wait) => wait,
            };
            if attempt >= PROBE_THROTTLE_RETRIES {
                self.give_up(client, "still throttled after every retry");
                return res;
            }
            let wait = wait.unwrap_or_else(|| backoff(attempt));
            attempt += 1;
            tracing::warn!(
                provider = %client.name(),
                model = %client.model(),
                attempt,
                wait_ms = wait.as_millis() as u64,
                "probe_gate: provider throttled the probe; retrying"
            );
            tokio::time::sleep(wait).await;
        }
    }

    fn give_up(&self, client: &dyn LlmClient, why: &str) {
        self.gave_up.store(true, Ordering::SeqCst);
        tracing::warn!(
            provider = %client.name(),
            model = %client.model(),
            why,
            "probe_gate: giving up the probe; its result will not be stored"
        );
    }
}

/// How a probe answer relates to the provider's rate limit.
enum Throttle {
    /// Not throttled.
    None,
    /// Throttled; retry after the provider's wait when it gave one.
    RetryAfter(Option<Duration>),
    /// The plan has no quota left; retrying cannot help.
    PlanExhausted,
}

fn throttle_of(res: &std::result::Result<Result<LlmResponse>, Elapsed>) -> Throttle {
    match res {
        Ok(Err(Error::Throttled { retry_after_ms, .. })) => Throttle::RetryAfter(
            retry_after_ms
                .map(Duration::from_millis)
                .map(|w| w.min(PROBE_THROTTLE_MAX_WAIT)),
        ),
        Ok(Err(Error::PlanExhausted { .. })) => Throttle::PlanExhausted,
        Ok(Err(e)) if e.http_status() == Some(429) => Throttle::RetryAfter(None),
        Ok(Ok(resp)) if resp.http_status == 429 => Throttle::RetryAfter(None),
        _ => Throttle::None,
    }
}

fn backoff(attempt: u32) -> Duration {
    PROBE_THROTTLE_BASE_WAIT
        .saturating_mul(1u32 << attempt.min(16))
        .min(PROBE_THROTTLE_MAX_WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::{LlmCapabilities, ScriptedLlmClient, ScriptedLlmResponse};
    use crate::llm::role::Role;
    use std::sync::atomic::AtomicUsize;

    fn req() -> LlmRequest {
        LlmRequest::new(Role::Thesis, "s".into(), "u".into())
    }

    fn throttled() -> Error {
        Error::Throttled {
            retry_after_ms: Some(1),
            message: "Token Plan rate limit reached (2062)".into(),
            http_status: Some(429),
        }
    }

    /// A throttled probe is retried, and the answer after the throttle
    /// is what the caller gets; the pacer did not give up.
    #[tokio::test]
    async fn a_throttled_probe_is_retried_until_it_answers() {
        let mut client = ScriptedLlmClient::empty();
        client.push_err(throttled());
        client.push_err(throttled());
        client.push_ok(ScriptedLlmResponse::accepted("ok"));
        let pacer = ProbePacer::with_gate(ProbeGate::new(1));
        let res = pacer.send(&client, &req(), Duration::from_secs(5)).await;
        assert!(matches!(res, Ok(Ok(_))), "{res:?}");
        assert_eq!(client.call_count(), 3);
        assert!(!pacer.gave_up());
    }

    /// Still throttled after every retry: the pacer gives up, so the
    /// probe stores nothing.
    #[tokio::test]
    async fn the_pacer_gives_up_after_every_retry() {
        let mut client = ScriptedLlmClient::empty();
        for _ in 0..=PROBE_THROTTLE_RETRIES {
            client.push_err(throttled());
        }
        let pacer = ProbePacer::default();
        let res = pacer.send(&client, &req(), Duration::from_secs(5)).await;
        assert!(matches!(res, Ok(Err(Error::Throttled { .. }))), "{res:?}");
        assert_eq!(client.call_count(), PROBE_THROTTLE_RETRIES as usize + 1);
        assert!(pacer.gave_up());
    }

    /// An exhausted plan is not retried.
    #[tokio::test]
    async fn an_exhausted_plan_is_not_retried() {
        let mut client = ScriptedLlmClient::empty();
        client.push_err(Error::PlanExhausted {
            message: "http 429: plan exhausted".into(),
            http_status: Some(429),
        });
        let pacer = ProbePacer::default();
        let _ = pacer.send(&client, &req(), Duration::from_secs(5)).await;
        assert_eq!(client.call_count(), 1);
        assert!(pacer.gave_up());
    }

    /// A client that records how many probe requests are in flight.
    struct Slow {
        now: AtomicUsize,
        peak: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmClient for Slow {
        fn sdk_type(&self) -> &'static str {
            "mock"
        }
        fn name(&self) -> &str {
            "slow"
        }
        fn model(&self) -> &str {
            "slow"
        }
        fn endpoint(&self) -> &str {
            "mock://slow"
        }
        fn capabilities(&self) -> LlmCapabilities {
            LlmCapabilities(crate::llm::capabilities::ProviderCapabilities::for_mock())
        }
        async fn send_once(&self, _req: &LlmRequest) -> Result<LlmResponse> {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            Err(Error::MockExhausted)
        }
        fn body_sha256(&self, _req: &LlmRequest) -> Result<String> {
            Ok(String::new())
        }
    }

    /// Pacers sharing one gate never have more requests in flight than
    /// the gate allows, however many probes fire at once.
    #[tokio::test]
    async fn pacers_sharing_a_gate_respect_its_limit() {
        let client = Arc::new(Slow {
            now: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let gate = ProbeGate::new(2);
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let pacer = ProbePacer::with_gate(gate.clone());
            let client = Arc::clone(&client);
            tasks.push(tokio::spawn(async move {
                let _ = pacer
                    .send(client.as_ref(), &req(), Duration::from_secs(5))
                    .await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(client.peak.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(1), Duration::from_secs(4));
        assert_eq!(backoff(4), Duration::from_secs(32));
        assert_eq!(backoff(5), PROBE_THROTTLE_MAX_WAIT);
        assert_eq!(backoff(30), PROBE_THROTTLE_MAX_WAIT);
    }
}
