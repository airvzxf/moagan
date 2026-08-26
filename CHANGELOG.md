# Changelog

All notable changes to `moagan` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed (BREAKING)

- **`--logs <PATH>` flag and `MOAGAN_RUN_LOGS` env var are removed.**
  v0.10 shipped an opt-in file log writer as a non-standard
  workaround. POSIX shells already provide the right primitive
  (redirection), and the indirection only made the JSON output
  harder to wire into `jq` / Promtail / Loki. The replacement is:
  - stderr (FD 2): trace events, JSONL by default
    (`moagan 2> run.jsonl | jq .` parses cleanly).
  - stdout (FD 1): typed domain events in JSONL
    (`moagan > events.jsonl` for `phase_start`, `llm_call`, `probe`,
    `run_start`, `run_end`, …).
  See `docs/events-v1.md` for the event schema.

### Added

- **POSIX-idiomatic stderr routing (per ADR-0002 §B).** `init_tracing`
  now writes `fmt::layer().json()` to stderr by default when stderr
  is not a TTY, and `fmt::layer()` with file/line/target colouring
  when stderr IS a TTY. The auto-detection is override-able via
  the new `--log-format <text|json|auto>` global flag and the
  `MOAGAN_LOG_FORMAT` env var. Closes the gap that ADR-0002 §B
  originally specified (the v0.10 implementation wrote plain text
  to stderr).
- **Stdout events JSONL.** A new `src/telemetry/stdout_events.rs`
  module with a typed `Event` enum (RunStart, RunEnd, PhaseStart,
  PhaseEnd, PhaseError, LlmCall, Probe, Warning, Decision,
  DiscoveryIteration). Each event is one NDJSON line on stdout
  when stdout is not a TTY, silenced in interactive mode (operators
  get a clean terminal). Override with `--event-format
  <jsonl|off>` or `MOAGAN_EVENT_FORMAT`. Pipe-friendly:
  `moagan … 2>log.jsonl | jq -c 'select(.kind=="llm_call")'`.
- **Tracing span hierarchy** (Android-2018 ambient-context model).
  `pipeline` → `phase` → `iteration` → `llm_call` /
  `llm_probe` / `llm_probe_background`. Every event emitted by
  an LLM call (regular OR probe) now carries the full chain:
  `pipeline{run_id, mode, provider, model, resumed} + phase{name,
  seq} + iteration{n, total, cell, temperature, replica,
  sketch_index} + llm_call{call_id, role}`. Operators can grep
  one phase or one iteration to follow the entire timeline
  end-to-end. The redact hot path stays event-free.

### Fixed

- **Plain-text errors no longer break NDJSON consumers.** The
  dispatcher's `eprintln!("error: …")` is now conditional on
  `std::io::stderr().is_terminal()` so the operator gets a
  one-liner on a TTY but the JSONL stream stays clean for
  `moagan … 2>log.jsonl | jq`.

## [0.10.0] - 2026-08-24

### Breaking

- **`config.toml` schema v0.10 — per-model endpoints, no more singletons**.
  The v0.9 `[providers.<name>]` shape with `kind` / `endpoint` /
  `model` / `max_tokens` / `hard_incompatibilities` singletons is gone.
  The canonical schema is:

  ```toml
  [providers.minimax]
  endpoint = "https://api.minimax.io/anthropic/v1/messages"
  models = [
    { id = "MiniMax-M3",         max_tokens = 1000000 },
    { id = "MiniMax-M2.7",       max_tokens = 1000000 },
    { id = "MiniMax-M2.7-highspeed", max_tokens = 1000000 },
    { id = "MiniMax-M2.5",       max_tokens = 1000000 },
  ]

  [providers.opencode]
  models = [
    { id = "kimi-k3",   endpoint = "https://opencode.ai/zen/go/v1/chat/completions", max_tokens = 1000000 },
    { id = "minimax-m3", endpoint = "https://opencode.ai/zen/go/v1/messages",        max_tokens = 1000000 },
    { id = "gpt-5.6-luna", endpoint = "https://opencode.ai/zen/go/v1/responses",    max_tokens = 1000000 },
    # …other OpenCode models…
  ]

  [providers.deepseek]
  endpoint = "https://api.deepseek.com/v1/chat/completions"
  models = [
    { id = "deepseek-chat",    max_tokens = 1000000 },
    { id = "deepseek-reasoner", max_tokens = 1000000 },
  ]
  ```

  * `endpoint` is now `Option<String>` (rename of the intermediate
    `endpoint_new`). Per-model `endpoint` overrides the section
    default; this is how a single `[providers.opencode]` section
    groups the chat-completions, Anthropic, and Responses paths.
  * `models[].endpoint` overrides the section-level `endpoint`
    when set, so a single canonical section can host every
    wire-format variant the upstream exposes.
  * `kind` / `model` / `max_tokens` / `hard_incompatibilities`
    fields on the section are gone — they only powered legacy
    code paths that are no longer in the tree.

- **`**OPENCODE_GO_MAX_TOKENS_CAP** global constant removed**. The wire
  layer no longer applies a 16 384 ceiling to every OpenCode Go
  call. The auto-probe (`max_token_auto`) discovers the real
  upstream boundary per `(provider, model)` and persists it to
  `<MOAGAN_HOME>/max_tokens_auto.toml`. With no probe result and
  no operator override, the wire body carries the request's
  `max_tokens` unchanged.

- **`minimax` routed through its own provider** (not through
  `AnthropicCompatProvider`). Before v0.10, every `minimax` call
  was clamped to `OPENCODE_GO_MAX_TOKENS_CAP = 16_384` even though
  MiniMax's real ceiling is `MINIMAX_MAX_TOKENS_CAP = 524_288`.
  v0.10 dispatches `[providers.minimax]` to `MinimaxProvider`,
  which clamps at the wire body to the real MiniMax boundary.

- **Per-model alias sections collapsed into canonical
  families**. `[providers.kimi-k3]`, `[providers.minimax-m3]`,
  etc. no longer exist as separate sections. The operator
  reaches every model via `--provider SECTION:MODEL`
  (`--provider opencode:kimi-k3`).

- **`--provider PROVIDER[:MODEL]` is mandatory** on every
  LLM-touching command (`run`, `discover`, `preflight`).
  Resolution order (highest first):
  1. CLI flag.
  2. `MOAGAN_DEFAULT_PROVIDER` env var.
  3. `[defaults] provider` in `~/.config/moagan/config.toml`.

  When none is set the command exits with:

  ```
  --provider is required (or set MOAGAN_DEFAULT_PROVIDER, or
  [defaults] provider in config.toml); example:
    moagan run --provider opencode:kimi-k3 --prompt "..."
    moagan run --provider opencode --model kimi-k3 --prompt "..."
  ```

- **`--model <name>` removed**. The model id is the second
  half of `--provider SECTION:MODEL`. The CLI still parses
  `--model` to surface a friendly error so legacy scripts do
  not silently break.

- **`api_keys.toml` / `<NAME>_API_KEY` env-var names**: the
  canonical provider-family key is the **section name** —
  no more `kind` indirection. `OPENCODE_API_KEY` (was
  `OPENCODE_GO_API_KEY`), `MINIMAX_API_KEY`, `DEEPSEEK_API_KEY`
  stay unchanged; legacy `OPENCODE_GO_API_KEY` exports are
  no longer consulted.

### Changed

- **`ProviderCapabilities::wire_format_id()` returns `"anthropic"`
  / `"openai"` / `"openai_compatible"`** to match the serde
  renames on `WireFormatId`. Log lines, JSON serialisation, and
  the in-memory capability matrix all agree.
- **`Config::default_provider`** is `Option<String>` (was
  `String`). The empty-string check stays as the canonical
  "no default" sentinel so v0.9 `default_provider = ""` keeps
  parsing cleanly.
- **`Config::resolve_provider(raw) -> Result<(section, model)>`**
  is the new helper every CLI dispatcher uses to parse the
  `--provider` value. Validates the section exists, that the
  requested model id is registered under it, and that
  multi-model sections reject the bare `SECTION` form.

### Removed

- `OPENCODE_GO_MAX_TOKENS_CAP` global constant.
- `ProviderConfig::endpoint_str()` / `model_str()` accessors.
- `ProviderConfig::kind`, `::model`, `::hard_incompatibilities`,
  and `::max_tokens: Option<u32>` fields (v0.9 singletons).
- `BLOCKED_MODELS` list in `opencode_go.rs` (deprecated; the
  operator now decides which models route through OpenCode via
  their `config.toml`).

## [0.9.12] - 2026-08-23

### Fixed

- **TOML quoting inconsistente en `temperatures_auto.toml` y `max_tokens_auto.toml`**: el crate `toml` no envuelve keys en comillas cuando son bare (e.g. `kimi-k3`). Nuevo helper `quote_provider_model_keys` post-procesa el output de `toml::to_string_pretty` para que las keys `[providers."X"."Y"]` se serialicen siempre entre comillas, consistente con `config.toml`. (PR #587)

- **Visibilidad del clamp de temperatura en logs**: el coordinator del discovery mode ahora emite `temperature_profile` (no `temperature`) en el log de iteración, dejando claro que es el valor del profile post-rewrite, no el valor enviado. `RunContext::dispatch_to_provider` añade un `tracing::trace!` (`RUST_LOG=moagan::phases::phase=trace`) cuando el gate encuentra que la temperatura ya está en el set soportado. (PR #587)

- **Regex `credit_card` matcheaba f32 precision**: la regex original `\b(?:\d[ -]?){13,16}\b` matcheaba los 15 dígitos de `1.899999976158142` (f32 precision de `1.9`), redactando el `9` en logs como `temperature=1.[REDACTED:credit_card]`. La nueva regex requiere separadores visibles (` ` o `-`) entre grupos de 4 dígitos y descarta secuencias largas sin separadores. Trade-off documentado en el doc-comment del patrón. (PR #587)

## [0.9.11] - 2026-08-23

### Added

- **Auto-detection of supported sampling temperatures per `(provider, model)`**
  (PR #585). New module `src/llm::temperature_probe` that mirrors the
  `max_tokens` auto-probe pattern: discover the discrete set of temperatures each
  upstream accepts and persist it so the runtime can rewrite user-requested
  values without re-running the pipeline. The canonical candidate set is
  `[0.0, 0.1, ..., 2.0]` (21 values, `0.1` step); the probe fans out in groups
  of 3 so it never saturates the upstream, and persists the result in
  `<MOAGAN_HOME>/temperatures_auto.toml`. The runtime table
  (`TemperatureTable`) exposes `nearest_supported(...)`, which
  `RunContext::dispatch_to_provider` queries: when the temperature resolved by
  the per-role default / profile / matrix-profile falls outside the
  discovered set, it is clamped to the nearest neighbour and a
  `tracing::warn!` is emitted. The operator-supplied matrix profile is
  rewritten to the boundary in `coordinator.rs` (nearest-neighbour per
  `provider_model`), preserving the declared cardinality.

  CLI: new subcommand
  `moagan probe temperature --provider PROVIDER:MODEL [--persist-union]
  [--batch-size N] [--dry-run]`. `--persist-union` takes the union (not
  intersection) of the discovered sets across every model of the same
  provider and writes the resulting set into the sidecar as the operator
  cap (`auto = false`). The default `--batch-size = 3` matches the runtime
  constant `TEMPERATURE_PROBE_BATCH_SIZE` so the CLI never exceeds the
  auto-probe's own concurrency envelope at startup. Tests: 23 unit
  (`temperature_probe.rs`) + 5 CLI (`cli/probe.rs`) + 9 integration
  (`tests/integration_*_temperature_*.rs`). New docs:
  [`docs/temperatures-auto.md`](docs/temperatures-auto.md) (algorithm,
  sidecar format, troubleshooting).

### Changed

- **`moagan probe` now lists `max-tokens` and `temperature`** as
  verb-first subcommands. The dispatch lives in
  `src/cli/probe.rs::ProbeCmd`. Cheatsheet: §20.

## [0.9.6] - 2026-08-21

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

- **Missing-open-brace repair pass** (PR #575): the MiniMax-M3
  upstream occasionally emits the object body without the outer
  `{` (e.g. literally `"problem": "Design a calculator", ...}`).
  The tolerant extractor could not find a `{` or `[` to balance, so
  the chain returned `Error::SchemaViolation(rc=7)` and the whole
  run aborted on the very first sketch. New `RepairKind::OpenBrace`
  pass prepends `{` when the (possibly BOM- or prose-prefixed)
  input starts with a JSON key. Runs explicitly before Path B
  extraction so Path B sees the balanced object instead of the
  inner `["..."]` substring.

- **Self-inflicted circuit-open cascade guard** (PR #575): on
  `card-1576 par-512` the upstream returned `PlanExhausted` (429)
  for individual sketches. The per-`(provider, role)` breaker
  tripped, but every subsequent call on the already-open path
  returned the synthetic `PlanExhausted("circuit open: ...")` and
  the post-call match caught that same variant and re-armed the
  breaker via `record_failure()`. The breaker never recovered.
  Now `was_open` is captured pre-call and `record_failure` only
  fires on `PlanExhausted` when the breaker was CLOSED at the
  start of the call. Self-inflicted circuit-open errors are no-ops.

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
  `--sketches-per-cell 10` (4-dim × 2-facet matrix → 80
  sketches) or higher — the facet-deriver fan-out similarly
  rate-limits upstream and defaults to an empty facet list when
  the upstream returns 429s (verified on the same mini run
  without the env var: 6 extractions vs 0).

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
