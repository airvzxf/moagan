# ADR 0002 — Runtime coverage for source-level error correlation

> **Status**: Accepted
> **Date**: 2026-08-18
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: nothing
> **Relates to**:
> [`docs/proposal-02-rust.md` T01-06 §10 (telemetry)](../proposal-02-rust.md),
> [`AGENTS.md` §"No-go list"](../../AGENTS.md),
> [`docs/adr/0001-no-go-list-policy.md`](0001-no-go-list-policy.md).

## Context

`moagan` already ships a structured telemetry layer
(`src/telemetry/mod.rs`, T01-06 §10): every phase, LLM call, warning,
checkpoint, and saturation event is appended to a `RedactWriter`-gated
JSONL stream and mirrored to SQLite when the index is enabled. Errors
flow through the same plumbing with an `error_code` and a
`phase / seq / call_id` triple so post-mortem review can group related
events.

What the telemetry does **not** carry is the source-level answer to
"which lines of code did this run actually execute?" Today, an operator
facing an unexpected `Error::Provider("http 500: boom")` (or any of
the other variants in `src/error/mod.rs:177`) has to:

1. Re-derive the call site by reading the JSONL `phase / role / call_id`
   triple and locating the matching `tracing::error!(...)` in the code.
2. Reason about the control flow that led to that call site by reading
   the source file.

The AI-driven origin of the project (the user has stated the entire
codebase was written without manual review) makes the second step
expensive and error-prone: there are no humans in the loop who can
recover the control-flow intent of every function.

The Rust ecosystem offers three mature, complementary techniques for
this:

- **LLVM source-based code coverage** (`-Cinstrument-coverage`): a
  rustc flag that instruments every source line with counters. At
  runtime the binary emits `*.profraw` files that
  `llvm-profdata` + `llvm-cov` (or `grcov`) post-process into per-line
  counts. This is what `cargo-llvm-cov` uses for test coverage, but
  the same machinery works on any binary and on production runs
  (the "runtime coverage" use case documented by the Rust
  documentation). Both `sancov` and `grcov` are already installed on
  the build host (`/usr/bin/sancov`, `~/.cargo/bin/grcov`).
- **`tracing` enriched with `file:line:column`**: the
  `tracing_subscriber::fmt::Layer` already supports
  `with_file(true)`, `with_line_number(true)`, and
  `with_current_span(true)`. Today the subscriber in
  `src/main.rs:295` uses none of these, so a `tracing::error!` only
  carries the message and the target, not the call site.
- **External eBPF / `bpftrace` uprobes**: a userspace tracer that
  attaches to a running process via uprobes. Powerful but
  Linux-only, requires frame-pointers, and does not naturally
  integrate with the existing JSONL telemetry stream.

The user has explicitly asked for the first two techniques
("A + B combinadas"). This ADR formalises that decision.

## Decision

The project will adopt a **two-layer observability upgrade** that
combines LLVM source-based runtime coverage (layer A, opt-in) with
`tracing` enriched with `file:line:column` metadata (layer B, always
on). A new `moagan coverage show <run_id>` subcommand closes the
loop by post-processing the `profraw` files emitted by layer A.

### A.1 — Layer B: enriched `tracing` subscriber (always on, cost ~0)

Change `init_tracing` in `src/main.rs:295` to enable file/line/column
metadata on the JSON `fmt` layer, plus a per-event writer that
routes `INFO`-and-below to stdout and `WARN`/`ERROR` to stderr
(the v0.12.0 PR-04a / E-1 stream-routing flip):

```rust
let writer = RoutingWriter { log_to_stderr };
let redacted_writer = moagan::telemetry::redact::ReportingLayer::new(writer);

let layer = match format {
    LogFormat::Text => fmt::layer()
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(redacted_writer)
        .boxed(),
    LogFormat::Json => fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(redacted_writer)
        .boxed(),
};
```

No new dependency, no new feature flag. Every `tracing::error!`,
`tracing::warn!`, and `tracing::info!` event written to the JSONL
streams now carries `file`, `line`, `column`, and (for the JSON
branch) the active span list. The text branch omits
`with_current_span` deliberately — terminal-friendly output is
already span-aware via the `RUST_LOG` filter and doesn't need the
extra metadata. The existing `serde_json` consumers (dashboard,
sqlite mirror) already use `serde(default)` /
`skip_serializing_if = "Option::is_none"` for unknown fields, so
the format change is backwards compatible.

### A.2 — Layer A: SanCov runtime coverage (opt-in via `coverage` feature)

Add a Cargo feature `coverage` (default `off`, alongside the existing
`dag` feature) that documents the intent and gates the runtime
helpers. The instrumented binary is built with a dedicated
`[profile.coverage]` that inherits from `release` with `debug = true`
and is built with the external flag
`RUSTFLAGS="-Cinstrument-coverage"`. The release binary in CI does
**not** carry the flag, so the production build is byte-identical to
today's binary.

When the feature is on, a new `moagan::coverage::CoverageRecorder`
injects a `LLVM_PROFILE_FILE` env var pointing at
`<run_dir>/telemetry/coverage/<run_id>-<tag>.profraw`, snapshots
counters on every `Telemetry::record_phase()` and
`Telemetry::record_call()` (see `src/telemetry/mod.rs:569-590,
649-678` and `src/coverage/mod.rs:281-441`), and rotates the active
`profraw` so the snapshot list stays bounded. The recorder is a
no-op when the binary is not instrumented (i.e. when the env var
is not honoured by the runtime), mirroring `Telemetry::noop()`.

### A.3 — Correlation layer

Each `Telemetry::phase()` and `Telemetry::call()` records the path of
the most recent `profraw` snapshot into the JSONL row (as a new
`coverage_snapshot: Option<String>` field, `serde(default)` so legacy
rows deserialize cleanly). When the operator faces an error, the
post-mortem story becomes:

1. Open `telemetry/phases.jsonl.gz`, locate the row with
   `error_code = PROVIDER_ERROR` (or whichever).
2. Read the `coverage_snapshot` field to find the
   `<run_id>-<seq>.profraw` file that was active at the moment of
   the error.
3. Run `moagan coverage show <run_id> --since-tag <tag>` (e.g. the
   phase name or call id stored in the `profraw` filename) to dump
   the lines that were visited in that window.

### A.4 — New CLI subcommand: `moagan coverage show`

Add `moagan coverage show <run_id>` to `src/cli/coverage_cmd.rs`
(`CoverageCmd::Show` at `src/cli/coverage_cmd.rs:14-42`), following the
existing pattern for `moagan inspect` and `moagan telemetry provider`.
Sub-options:

- `--since-tag <tag>`: filter the snapshot list to files whose name
  contains the given tag (case-insensitive). Useful for narrowing to
  a single phase or call id.
- `--format {text,html}`: `text` (default) writes a columnar
  `file:line:count` view to stdout for pipelines; `html` writes a
  navigable `coverage.html` next to the run dir via `grcov`.
- `--html-out <path>`: override the path the HTML report is written
  to (defaults to `<run_dir>/coverage.html`). Ignored when the format
  is `text`.

The subcommand shells out to `grcov` (or `llvm-profdata merge` +
`llvm-cov show` as a fallback) via `std::process::Command` — neither
is added as a crate dependency. When neither tool is on `PATH` the
HTML view fails with a clean error; the text view always works (it
just prints a "not instrumented" hint when the report is empty).

### A.5 — Default-off, no release binary impact

The new feature flag is **opt-in**, mirroring `dag`. The default
build (`cargo build` with no features) is byte-identical to today;
the new `src/coverage/` module is a no-op stub when
`LLVM_PROFILE_FILE` is not set. CI's `release.yml` is untouched.

## Consequences

### Positive

- The operator can answer "which lines ran before this error fired?"
  with one command, instead of reading source.
- Layer B is a 5-line change to `src/main.rs` and adds zero
  dependencies. It improves the existing telemetry immediately.
- No new crate dependencies in the dependency tree. SanCov is
  built into `rustc` itself; the only external tools required
  (`grcov` / `llvm-cov`) are already on the host and are invoked
  as subprocesses, not linked.
- The `coverage` feature follows the exact same opt-in pattern as
  the existing `dag` feature, so the policy guard
  (`scripts/check-no-forbidden-crates.sh`) needs no change.
- The JSONL format gains new fields with `serde(default)` so legacy
  consumers (dashboard, sqlite mirror, downstream tools) keep
  working without changes.

### Negative / accepted risks

- **SanCov requires `debug = true` in the profile.** The
  `profile.coverage` inherits from `release` but turns debug info
  on, so the "coverage binary" is not exactly the production
  binary. The trade-off is documented in the plan and the ADR
  accepts the gap: code coverage is debug-grade, not
  performance-grade. The user must understand that the coverage
  report reflects the *coverage* build, not the *release* build,
  even though the source is identical.
- **`*.profraw` volume.** A long run emits one `profraw` per phase
  start and one per `tracing::error!`. PR #563 (commit `2d10fc9`)
  added `CoverageRecorder::start_rotation` (`src/coverage/mod.rs:399-441`)
  which spawns a background thread that rotates the active
  `profraw` (the `profraw` file is renamed to a `<run_id>-<tag>-<seq>.profraw`
  snapshot and a new active `profraw` is created). The
  `daily_rotation` helper in `src/telemetry/daily_rotation.rs` is a
  separate concern — it only emits a `stale_artifact` warning on
  day-rollover for the regular `telemetry/daily.log` stream.
- **The "line that caused the error" is still approximate.** The
  coverage report tells you which lines ran *before* the error,
  not the exact line that raised. The panic hook in
  `src/main.rs:367` already gives the exact line for panics;
  for non-panic `Error` values, layer B's enriched tracing is the
  best we can do without per-error `#[track_caller]` propagation
  (left as a future improvement, not in scope).
- **The `coverage` feature flag is not a magic switch.** It only
  documents intent and gates the runtime helpers. The
  `-Cinstrument-coverage` flag must be passed at compile time via
  `RUSTFLAGS` (the plan documents the exact command). This is the
  same constraint that `cargo-llvm-cov` has, and it is the only
  way SanCov works with `rustc` today.

### Compliance

| Surface | Where it lives | Enforced by |
|---|---|---|
| `coverage` feature flag | `Cargo.toml` `[features]` | existing `check-no-forbidden-crates.sh` (no change; `coverage` is not on the no-go list) |
| `moagan::coverage` module | `src/coverage/mod.rs` | `make build` (T1) + `make test-ci` (T2) |
| `moagan coverage show` | `src/cli/coverage_cmd.rs:14-42` | `make test-ci` integration test |
| JSONL `coverage_snapshot` field | `src/telemetry/mod.rs` | `serde(default)` for backwards compat |
| ADR 0002 | `docs/adr/0002-runtime-coverage.md` | this document |

## Re-evaluation

This ADR will be revisited when any of the following happen:

1. SanCov graduates from `-Cinstrument-coverage` to a default-on
   configuration in `rustc` (the Rust project has signalled
   long-term intent to stabilise runtime coverage; today it is
   still considered an "unstable feature" by the upstream
   docs).
2. A new, lighter-weight Rust-native coverage mechanism becomes
   available (e.g. MIRI-based coverage, or a `coverage` crate
   that drops the LLVM dependency). If that happens, the
   `CoverageRecorder` design should be ported and this ADR
   superseded.
3. The `coverage` feature flag is needed by default in production
   (i.e. users start asking for runtime coverage in
   always-on mode). Until then, default-off keeps the release
   binary footprint stable.

Until then, the verdicts above are authoritative.
