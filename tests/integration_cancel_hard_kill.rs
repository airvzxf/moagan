//! Integration test for `CancelTier::Hard` killing the spawned
//! sandbox child. Verifies the full path: `Sandbox::with_cancel` →
//! `pre_exec` (`setpgid(0, 0)`) → `Cancel::register_child` →
//! `cancel_with_tier(Hard)` → `killpg(SIGTERM)` → child exits.
//!
//! The test sleeps for 2.5s (`HARD_KILL_GRACE` + slack) which is the
//! worst-case wall clock for a Hard cancel that catches the child
//! still alive after SIGTERM. If Hard worked, the child must be
//! dead well before `sleep 60` finishes.

use std::time::{Duration, Instant};

use moagan::cancel::{Cancel, CancelReason, CancelTier};
use moagan::sandbox::{Sandbox, SandboxConfig, SandboxStatus};
use tempfile::TempDir;

const HARD_KILL_OBSERVED_BUDGET: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_cancel_terminates_inflight_sandbox_child() {
    let cancel = Cancel::new();
    let sandbox = Sandbox::new(SandboxConfig::new().with_timeout(Duration::from_secs(120)))
        .expect("sandbox builds")
        .with_cancel(cancel.clone());

    let work = TempDir::new().expect("tempdir");
    let started = Instant::now();
    let cancel_for_assert = cancel.clone();

    // Run the kill on a side task so the main task can block in
    // `sandbox.run` until the child exits. The Hard signal goes
    // out 1.5s after spawn; we expect the child to be reaped by
    // ~3.5s after spawn (1.5s + 2s grace + slack).
    let killer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        cancel_for_assert.cancel_with_tier(CancelReason::UserInterrupt, CancelTier::Hard);
    });

    // `sleep 60` would normally run for 60s; Hard cancel must
    // truncate that. `timeout` is the sandbox-level deadline
    // (120s above), well above what Hard takes. `sleep` is
    // not in the default allowlist, so we wrap it in `sh -c`
    // which IS allowlisted; the spawned sh process becomes the
    // pgid leader via `pre_exec`, and the inner `sleep` inherits
    // the pgid, so `killpg` reaches the whole subtree.
    let result = sandbox
        .run_in(work.path(), "sh", &["-c", "exec sleep 60"])
        .await
        .expect("run_in returns Ok result");
    let elapsed = started.elapsed();
    killer.await.expect("killer task");

    // The child was killed by signal, not by exit. `tokio::process::Child`
    // surfaces that as `None` exit code, which `SandboxResult::new`
    // maps to `-1`. The status field is what `Timeout` would set on
    // a sandbox-level timeout; here we got signal-killed so the
    // status is either `Fail` (non-zero exit mapping for a signal)
    // or the natural-collapse path. The strict invariant is that
    // the wall clock is below the 60s budget.
    assert!(
        elapsed < HARD_KILL_OBSERVED_BUDGET,
        "Hard cancel should have killed the child in <{HARD_KILL_OBSERVED_BUDGET:?}, took {elapsed:?}"
    );
    assert!(
        result.status == SandboxStatus::Fail || result.status == SandboxStatus::Timeout,
        "Hard-killed child must surface as Fail or Timeout, got {:?}",
        result.status
    );
    assert!(
        cancel.is_cancelled(),
        "cooperative token must be set after cancel_with_tier(Hard)"
    );
    assert_eq!(
        cancel.reason(),
        Some(CancelReason::UserInterrupt),
        "cancel reason must be recorded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_cancel_terminates_subtree_of_spawned_child() {
    // Cargo's `cargo --version` is a one-shot program that exits
    // before SIGTERM lands; this test uses a small shell pipeline
    // that spawns a child via `sh -c`, exercising the
    // setpgid-group-kill path through one level of `fork`/`exec`.
    let cancel = Cancel::new();
    let sandbox = Sandbox::new(SandboxConfig::new().with_timeout(Duration::from_secs(120)))
        .expect("sandbox builds")
        .with_cancel(cancel.clone());

    let work = TempDir::new().expect("tempdir");
    let cancel_for_assert = cancel.clone();

    let killer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        cancel_for_assert.cancel_with_tier(CancelReason::UserInterrupt, CancelTier::Hard);
    });

    let started = Instant::now();
    let result = sandbox
        .run_in(work.path(), "sh", &["-c", "sleep 60 & wait $!"])
        .await
        .expect("run_in returns Ok result");
    let elapsed = started.elapsed();
    killer.await.expect("killer task");

    assert!(
        elapsed < HARD_KILL_OBSERVED_BUDGET,
        "Hard cancel must reap a forked child too, took {elapsed:?}"
    );
    assert!(
        result.status == SandboxStatus::Fail || result.status == SandboxStatus::Timeout,
        "Hard-killed forked child must surface as Fail or Timeout, got {:?}",
        result.status
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soft_and_normal_cancel_do_not_touch_processes() {
    // Soft/Normal must signal the cooperative token without
    // touching any registered pgid. The sandbox runs `sleep 1`
    // which exits naturally; we verify cancel-with-t-ier(Soft|Normal)
    // does not panic, the cooperative token is set, and the child
    // completes normally.
    for tier in [CancelTier::Soft, CancelTier::Normal] {
        let cancel = Cancel::new();
        let sandbox = Sandbox::new(SandboxConfig::new().with_timeout(Duration::from_secs(5)))
            .expect("sandbox builds")
            .with_cancel(cancel.clone());

        let work = TempDir::new().expect("tempdir");
        let cancel_for_assert = cancel.clone();
        let killer = tokio::spawn(async move {
            cancel_for_assert.cancel_with_tier(CancelReason::Requested, tier);
        });

        let result = sandbox
            .run_in(work.path(), "sh", &["-c", "exec sleep 0.2"])
            .await
            .expect("run_in returns Ok result");
        killer.await.expect("killer task");

        assert_eq!(
            result.status,
            SandboxStatus::Pass,
            "{tier:?} must let the child finish naturally"
        );
        assert!(cancel.is_cancelled(), "{tier:?} must set the token");
    }
}
