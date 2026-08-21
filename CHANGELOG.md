# Changelog

All notable changes to `moagan` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.6] - Unreleased

### Fixed

- **Per-provider circuit breaker cascade on facet-deriver 429**
  (the bug surfaced in `run-real-600` with v0.9.5). Two-tier
  recovery: `Error::Throttled` (transient HTTP 429 with RPM/TPM
  message) is absorbed by the per-`(provider, role)` adaptive
  throttle governor (AIMD backpressure that drops concurrency and
  increases the pre-call backoff with jitter); `Error::PlanExhausted`
  (token plan / monthly quota / subscription keywords) trips the
  per-`(provider, role)` circuit breaker. The legacy per-provider
  breaker is kept only for the provider pool's `is_available()`
  signal; it no longer short-circuits `send()`.

### Added

- **`Error::Throttled { retry_after_ms, message }`** variant —
  HTTP 429 with a non-plan body now classifies as transient
  rate-limit (handled by the throttle governor) rather than the
  pre-v0.9.6 `PlanExhausted` blanket-classification.
- **`Error::provider_cause()`** — helper that categorises
  provider-side errors into `PlanExhausted`, `Throttled`, or `Other`
  for the call-site governor / breaker to consume.
- **`ThrottleGovernor` + `GovernorRegistry`** (in
  `src/llm/governor.rs`) — per-`(provider, role)` AIMD
  backpressure. Reduces per-role in-flight concurrency and
  increases the pre-call backoff on transient 429s, recovers
  via additive increase + decay. Configurable via
  `[throttle_per_role]` in `~/.config/moagan/config.toml` or
  `MOAGAN_THROTTLE_PER_ROLE_<role>=<initial>:<max>:<init_backoff>:<max_backoff>:<additive_after>:<jitter>`.
- **`BreakerRegistry`** (in `src/llm/circuit_breaker.rs`) —
  per-`(provider, role)` breaker registry. Replaces the v0.9.4
  per-provider breaker. Configurable via
  `[circuit_breaker_per_role]` in `~/.config/moagan/config.toml` or
  `MOAGAN_CIRCUIT_BREAKER_PER_ROLE_<role>=<threshold>:<window_secs>:<cooldown_secs>`.

## [0.9.5] - 2026-08-21

### Fixed

- **`.meta.json` sidecar leak in discovery walks** — `discover_cluster`,
  `discover_facet`, and `discover_tag` filtered on
  `extension == "json"` only, which also matched the
  `.meta.json` sealed sidecars the FS layer writes next to every
  artefact. Because every domain struct (`Sketch`, `Cluster`,
  `FacetList`, `SketchTags`) is `#[serde(default)]`, the sidecars
  deserialised cleanly into records with empty ids / cluster_ids /
  empty facet lists. The downstream extract phase then saw 579
  phantom clusters with `members:[""]` and skipped them, yielding
  zero facet extractions and aborting with
  `discover_extract produced zero facet extractions`. Reproduced
  pre-fix on `run-real-600` (`01a0228d-da36-7583-afc0-52bd3b825d82`):
  1156 sketch files (578 real + 578 sidecar), 1160 cluster files
  (579 with `members:[""]` + 1 cluster_00 with all 578 real
  members). The filter that already excluded `.meta.json` in
  `discover_extract`, `discover_summary`, `discover_integrate`,
  `discover_contradict`, `propose`, and `discovery::context` is
  now factored into `phases::util::primary_json_paths` and used by
  the three missing call sites. End-to-end verified post-fix on
  card-80 par-8 (mini-validate / `01a022d9-…`): 80 sketches, 2
  clusters (0 phantom), 1 facet list (0 empty `cluster_id`), 6
  facet `.md` extractions, rc=0 elapsed=292 s. Sister fix:
  setting `MOAGAN_RATE_LIMIT_ROLE_FACET_DERIVER=30:2` (alongside
  the v0.9.4 tagger knob) is required when running with
  `--cardinality 80` or higher — the facet-deriver fan-out
  similarly rate-limits upstream and defaults to an empty facet
  list when the upstream returns 429s (verified on the same mini
  run without the env var: 6 extractions vs 0).

## [0.9.4] - 2026-08-21

### Added

- **Per-role rate-limit (catalog §D.19.6)** — new `[rate_limit_per_role]`
  TOML knob (also reachable via env `MOAGAN_RATE_LIMIT_ROLE_<role>`
  or `RateLimitConfig::default()` extension) throttles a single role
  independently of the per-provider bucket. Resolves the run06
  cascade where the post-matrix tagger fan-out (1500+ LLM calls)
  saturated the upstream provider's quota and produced
  1484+ placeholder clusters with empty `label`/`summary`, which
  then inflated `discovery_context.json.facet_ids` to 15,668.
  Empty by default (no per-role limit) so existing runs are
  bit-identical. Operators opt in with, e.g.:
  ```toml
  [rate_limit_per_role]
  tagger = { capacity = 30, refill_per_sec = 2 }
  ```
  Acquired in `call_with_retry` after the cache lookup and in
  `call_uncached` so the retry path (which bypasses the cache)
  honours the same per-role bucket. CLI wiring at
  `src/cli/discover.rs` parses each string key into a `Role` via
  `<str>::parse::<Role>()`; unknown role names are silently
  skipped so a stale config never aborts the run.

## [0.9.2] - 2026-08-20

### Fixed

- **Parser chain stability** (#562) — `repair_missing_separators` in
  `src/phases/util.rs` now infers object context when the stack is empty
  and the next non-whitespace char is `:`. The previous behaviour re-inserted
  a stray `,` between a JSON key and its colon, breaking the lenient
  repair chain for fragments without a leading `{`. Unit-tested in
  `phases::util::tests::parse_model_json_does_not_reintroduce_comma_after_stray_comma_fix`.
  Closes `#558`.

- **SanCov profraw rotation** (#563) — the `*.profraw` file emitted by
  the
  SanCov runtime coverage instrumentation previously grew unbounded
  (~12 GB/hour on a healthily running pipeline). Added
  `CoverageRecorder::start_rotation(max_bytes, interval_secs)` that
  rotates the active file when it exceeds `MOAGAN_COVERAGE_PROFRAW_BYTES_MAX`
  (default 1 GiB). The slice capture is renamed to a timestamped
  sibling; merge with `llvm-profdata merge *.profraw.*` for cumulative
  coverage.

- **SanCov runtime warning** (#562) — `moagan` now emits a
  `tracing::warn!` at startup when the binary was built with the
  `coverage` Cargo feature AND `LLVM_PROFILE_FILE` is set, so the
  operator knows the runtime file will grow unbounded without
  rotation. The `start_rotation` thread spawned by `Telemetry::open`
  keeps the warning actionable.

- **Run-comparison script hardening** (#562) — `scripts/comparison/run-comparison.sh`
  now traps SIGINT/SIGTERM and quarantines failed runs to
  `.failed-<unix-ts>` siblings instead of leaving 60+ GB on disk
  when the underlying discovery run is aborted. Verified context:
  run8 on 2026-08-19 left a 66 GB `active.profraw` because the
  previous cleanup was conditional on `rc=0`.

- **Sketch extraction retries** (#564) — sketches that fail JSON parse
  on the first attempt are now retried up to 2 times (3 attempts
  total). Run8 on 2026-08-19 had a 4.2 % rejection rate; the
  additional retry recovers the majority of JSON-fragment failures
  caused by the temperature 1.0+ pathology on MiniMax-M3, without
  re-introducing the 30-day cardinality 880 projection that motivated
  the original drop from 3 to 1.

- **Stale `clippy` comment** (#564) — the audit comment in
  `src/cli/discover.rs:359` documenting the `--max-parallelism`
  cap was last touched when the cap was 64. PR #543 lifted it to
  `u32::MAX` (1) and PR #544 scaled the rate limiter with the cap;
  the comment is now refreshed to match the current state without
  implying a 64-call hard limit that no longer exists.

### Changed

- **Rate limiter refill** (#563) — the rate limiter default
  `refill_per_sec` was previously `parallelism / 4`, calibrated for
  the old sequential discovery loop where the bottleneck was a
  single concurrent call. The discovery loop is now actually
  parallel (see `perf` below), so the semaphore and the rate
  limiter are the same knob (both limit in-flight calls). The
  default now matches 1:1: `refill_per_sec = parallelism`. Operators
  who want a lower rate than the parallelism cap can override with
  `MOAGAN_RATE_LIMIT_<provider>`.

### Performance

- **Discovery loop parallelisation** (#563) — `src/discovery/coordinator.rs`
  spawned each `(cell, temperature, replica, sketch_index)` iteration
  as a separate `tokio::task` via a `tokio::task::JoinSet`
  (per AGENTS.md: no `tokio::spawn` without a recorded join handle).
  The semaphore (`ctx.parallelism`) limits the number of concurrent
  LLM calls. The previous implementation was sequential because
  the loop awaited each call in-place; the semaphore was acquired
  and released immediately, and only one permit was ever in flight.
  Throughput was ~3.3 sketches/min on every run regardless of
  `--max-parallelism` (verified against run7 8h 1619 sketches and
  run8 5h 40m 1348 sketches). The parallel loop honours the
  configured parallelism; a `--max-parallelism 64` run that took
  8 hours now targets the same workload in ~15-20 min.

- **Discovery lock discipline** (#563) — `SketchLoopState` and
  `SaturationTracker` are now wrapped in `Arc<std::sync::Mutex<>>`
  so concurrent tasks can safely mutate them. The mutex is held
  only for the mutation + `state.save`; the lock window is a few
  microseconds, dominated by the LLM round-trip (15-18 s).

### Added

- **`moagan preflight` subcommand** (#564, #566) — smoke-tests the
  full pipeline end-to-end against the real provider. Two-run flow:
  1. `moagan discover` with cardinalidad 8 (1 sketch per dimension
     × facets_per_dimension = 1), 1 temperature (1.0), 1 replica.
  2. `moagan run --mode fast` with `--context <discover_run_id>
     --context-full` so the second run consumes the discover run's
     library through the cross-run `--context` plumbing.
  Both run ids are printed on stdout so the operator can drill
  into either one with `moagan inspect`. Cost: ~60-120 s of API
  budget and ~3 MB of disk per preflight. Exits 0 only when both
  runs succeed; a non-zero exit code is a strong signal that the
  pipeline broke somewhere.

- **`scripts/smoke_preflight.sh`** (#566) — T3 smoke coverage for
  the preflight subcommand. Four tests against the mock provider
  in `tests/fixtures/mock_provider`:
  `preflight_two_run_ids_printed`, `preflight_mock_creates_discover_sketches`,
  `preflight_non_interactive_no_prompts`, `preflight_invalid_provider_fails_fast`.
  Wired into the `Makefile` `SMOKE_SCRIPTS` list so `make smoke` (the
  T3 gate in the validation gauntlet) runs it.

### Internal

- The `src/discovery/coordinator.rs` loop now uses
  `tokio::task::JoinSet` and `let _permit = ctx.parallelism.acquire()`
  inside each spawned task. Stop conditions (target reached, tracker
  `Stop`, cancel tripped) are polled between spawns; in-flight tasks
  continue to whatever they have in flight before the `JoinSet`
  drains.

- `startup_reconcile` is invoked at the top of every pipeline-opening
  dispatch (T3 D.28.3 + D.28.4). It reconciles filesystem vs SQLite
  and sweeps orphan `*.tmp.<uuid>` and stale `*.lock` files. The
  `Config::startup_reconcile` flag (default `true`) and
  `MOAGAN_STARTUP_RECONCILE=false` env var gate the call.

## [0.9.1] - 2026-08-19

### Fixed

- Anthropic prefill `{` for JSON-required roles (#554) — the wire
  layer now emits a leading `{` in the assistant prefill when the
  response payload is expected to be a JSON object. Avoids the
  upstream MiniMax model stalling on the first character of the
  response.

[0.9.2]: https://github.com/airvzxf/moagan/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/airvzxf/moagan/compare/v0.9.0...v0.9.1
