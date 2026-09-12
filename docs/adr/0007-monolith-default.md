# ADR 0007 — Moagan is a single-crate monolith by default

> **Status**: Accepted
> **Date**: 2026-09-08
> **Deciders**: `airvzxf/moagan` operator + 12 parallel
> investigation subagents (spike #829)
> **Supersedes**: nothing. The "single crate, no workspace" line
> in [`AGENTS.md`](../../AGENTS.md) §"Stack" was descriptive,
> not a prior ADR.
> **Relates to**:
> [spike issue #829](https://github.com/airvzxf/moagan/issues/829)
> (the 12-angle investigation that motivates this ADR),
> [EPIC #830](https://github.com/airvzxf/moagan/issues/830)
> (the action plan this ADR opens),
> [`AGENTS.md` §"Stack"](../../AGENTS.md) (the canon this ADR
> formalises),
> [`AGENTS.md` §"No-go list"](../../AGENTS.md) +
> [ADR-0001](../../docs/adr/0001-no-go-list-policy.md)
> (the differentiated allow-list that any future split must
> survive),
> [ADR-0002](../../docs/adr/0002-runtime-coverage.md)
> (the `coverage` feature — precedent for isolation without
> crate boundaries).

## Context

`moagan` is currently a single Rust crate with one binary
(`src/main.rs`) and one library (`src/lib.rs`). It builds
~148 KLOC across ~33 top-level modules under `src/`, plus a
~24 KLOC test suite under `tests/`. The CI gauntlet
(`make fmt-check`, `make guard-deps`, `make lint`,
`make build`, `make test-ci`, `make smoke`, `make e2e`) is
calibrated against this shape, and
`scripts/check-no-forbidden-crates.sh` enforces the no-go list
against `Cargo.toml` + `Cargo.lock`.

The maintainer opened a philosophical question (spike
[#829](https://github.com/airvzxf/moagan/issues/829)):
**should `moagan` be split into Cargo Workspaces, or should it
keep its monolithic shape and invest in better internal
discipline?**

Both options have credible precedent:

- **Monolith**: Linux, SQLite, Redis, Git, FFmpeg — monolithic
  binaries that scale via *in-source* modularity (Kconfig-style
  feature flags, `#ifdef`, loadable modules, namespace prefixes,
  `MAINTAINERS` files).
- **Workspaces**: tokio, ripgrep, serde, bevy, cargo itself,
  deno — multi-crate projects where the split is justified by
  external publication, heterogeneous deployment surfaces, or
  build-time isolation.

The maintainer has successfully used Cargo Workspaces in another
project (a music-harmonies tool with `core / cli / web / Android
/ desktop` — five heterogeneous deployment surfaces) and is
*skeptical* of splitting `moagan` into crates but leaves the
question open.

The maintainer also proposed an alternative shape: **phases as
independent binaries sharing a single raw-HTTP LLM client
library** — essentially microservices-in-monorepo or
"libraries with binary IPC". This is *not* identical to a Cargo
Workspace split and was evaluated on its own terms.

Spike [#829](https://github.com/airvzxf/moagan/issues/829)
dispatched 12 parallel subagents, one per angle of the
question. Their findings, consolidated in the spike comment,
agree on the following:

| Angle                                  | Verdict          | Viability |
|----------------------------------------|------------------|-----------|
| Current `src/` inventory               | Hub `phases/phase.rs` ties everything together. | 6/10 |
| Phase boundaries                       | Per-phase split destroys in-memory state. | 2/10 |
| Workspace precedents in Rust           | Workspaces make sense with feature divergence, external publication, or build-time isolation. None apply today. | 8/10 (only if/when conditions flip) |
| Polylith / monorepo patterns           | Useful as vocabulary, no new tooling needed. | 4/10 |
| Successful monolith patterns (Linux, SQLite, Redis, FFmpeg) | Modular via *internal* discipline (Kconfig, amalgamation, modules, namespace prefixes, `MAINTAINERS`, feature flags). | 9/10 |
| Library-driven / plugin architecture   | Already half-built (`Vec<Box<dyn Phase>>`); needs auto-discovery only. | 7/10 |
| Maintainer's "phases as binaries"      | Destroys `Governor` AIMD, `PromptCache` hot-path, `heartbeat_handle`. Precedents are uniformly monolithic. | 2/10 |
| IPC patterns for cross-binary phases   | stdio JSON-RPC (MCP pattern) is the only no-go-list-safe option; HTTP localhost reuses `src/llm/`. | 7/10 (moot unless architecture flips) |
| Workspace cost analysis (quantified)   | `cargo build` inner-loop ≈ 11.7 s hot; workspace overhead grows super-linearly with crate count. | 3/10 for 5–10 crates |
| Monolith cost analysis (measured)      | 143 KLOC, 8 jobs CI, no per-module timings exist (gap). | 8/10 |
| Migration risk                         | Reversibility is asymmetric; "phases as binaries" is one-way. | 2/10 to split now |
| Weighted decision matrix               | Monolith 8.95 / 10 vs. workspaces 4.05 / 10. | Strong default |

The maintainer's harmonic-tool precedent (5 deployment surfaces)
**does not transfer**: `moagan` has 1 surface (one CLI binary,
one library, one release artefact). The maintainer's
Linux-as-monolith argument **is correct** but the lesson is
*Kconfig-style feature flags*, not crate partitions.

## Decision

`moagan` **stays a single crate** through v0.16–v0.18.
Internal modularity is enforced via:

- **`#[cfg(feature)]`** for optional subsystems. Precedent:
  `dag` (gates `petgraph 0.6` per
  [ADR-0001 D-1](../../docs/adr/0001-no-go-list-policy.md))
  and `coverage` (gates SanCov runtime per
  [ADR-0002](../../docs/adr/0002-runtime-coverage.md)). Future
  per-validator flags land in
  [EPIC #830 sub-issue #834](https://github.com/airvzxf/moagan/issues/834).
- **Rust modules + `pub use` re-exports** for visibility
  discipline. `src/lib.rs:7` carries `#![warn(unreachable_pub)]`
  which makes accidental cross-module public surface a lint
  warning.
- **`docs/MAINTAINERS.md`** for ownership. Lands in
  [EPIC #830 sub-issue #833](https://github.com/airvzxf/moagan/issues/833).
- **Library-driven `PhaseFactory` registry** for plugin-style
  extensibility. Lands in
  [EPIC #830 sub-issue #835](https://github.com/airvzxf/moagan/issues/835).
- **`make profile-build`** to make the monolith's compile costs
  *measured*, not estimated. Lands in
  [EPIC #830 sub-issue #832](https://github.com/airvzxf/moagan/issues/832).

## Measured costs

This section is populated by the `make profile-build` target
landed in [#832](https://github.com/airvzxf/moagan/issues/832).
The numbers below come from the baseline run on `main` at
v0.15.1 (commit `b52cca4`, captured in
`target/timings/main/{leaf,middle,hub}.html`). They reflect a
**hot-cache inner-loop rebuild** — the realistic cost of a
single-file edit in the operator's day-to-day flow.

| Scenario | File touched                  | Total inner-loop rebuild (hot cache) |
|----------|-------------------------------|---------------------------------------|
| `leaf`   | `src/llm/openai_compat.rs`    | **9.0 s** (1 crate recompiled) |
| `middle` | `src/cli/discover.rs`         | **9.0 s** (1 crate recompiled) |
| `hub`    | `src/phases/phase.rs`         | **9.1 s** (1 crate recompiled) |

Spike reference point: `touch src/lib.rs && cargo build`
(measured at spike time on a different host) ran in **11.73 s**.
That edit fans out wider than the three scenarios above because
`src/lib.rs` re-exports every `pub mod`. The three scenarios
shown here touch a single leaf module and stay closer to the
~9 s floor.

The narrow spread between `leaf`, `middle`, and `hub`
(9.0 / 9.0 / 9.1 s) means **the inner-loop rebuild is not yet
hitting the fan-out wall** on this hardware. The hypothetical
inversion condition "T1 > 90 s reproducibly" is ~10× above the
current cost. Either:

- the spike's "cascading recompile" worry is overstated for
  current code shape, or
- the spike was measuring a cold cache (very different regime),
  or
- the bottleneck lives in the *cold* build, not the inner loop.

Cold-cache measurements belong in a follow-up PR; this ADR's
table stays scoped to hot-cache inner-loop, which is the metric
operators actually feel.

Other measured numbers already in the repo (from cluster reports
under `docs/cluster-v0.15.0-validation-reports/`):

- CI gauntlet wall-clock: cold ≈ 6 min, warm ≈ 3 min.
- T1 (`cargo clippy --all-targets`): ~60 s.
- T2 (`cargo test --all-targets`): 1–5 min.
- T3 (`make smoke` + `make e2e`): ~5 min.
- Binary sizes: `target/debug/moagan` ≈ 124 MB,
  `target/release/moagan` ≈ 28 MB.
- `target/release/deps/` carries 10 `libmoagan-*.rlib` variants,
  evidence that hub edits already trigger cascading recompiles
  inside the single-crate graph.

## Inversion conditions

The decision is **reopened** with ADR-0008 iff **any** of the
following becomes true:

1. **Second binary with its own release** lands. Candidates:
   `moagan-server` (HTTP daemon), `moagan-mcp` (Model Context
   Protocol server), or a third-party SDK distributed via
   crates.io. When `cargo publish` becomes part of the release
   ritual, workspaces earn their keep.
2. **`make build` exceeds 90 s T1 reproducibly**, with the
   dominant cost localised to one subtree (`llm/` or `phases/`).
   This is the threshold where Cargo's per-crate parallelism
   outweighs workspace coordination overhead. The
   `make profile-build` artefact in `target/timings/main/` is
   the canonical input to this check.
3. **A second active maintainer** works on disjoint subsystems
   on a sustained cadence (≥ 1 PR/week on each side for 4
   consecutive weeks). Single-maintainer workflows tolerate
   monolith coordination overhead; multi-maintainer workflows
   benefit from per-crate `CODEOWNERS` + per-crate CI shards.

If **any** condition fires, reopen with ADR-0008 following the
split order below.

### Split order if inversion fires

If a workspace extraction is approved, the order is **horizontal,
never per-phase**:

1. **`moagan-core`** first. Moves `error/`, `error_code.rs`,
   `ids.rs`, `time.rs`, `serde_util/`, `secret.rs`, `fs_layout.rs`,
   `redact/`, `validators/`. Result: a `core` crate with no I/O,
   no `tokio`, no async. Tests go from `cargo test --all` to
   `cargo test --workspace` (same mental cost, same wall-clock).
2. **`moagan-llm`** second. Moves all of `src/llm/`. Keeps
   `default-features = false, features = ["rustls-tls"]` to
   survive `scripts/check-no-forbidden-crates.sh`. This is the
   shared "raw HTTP + retry + circuit_breaker + cache" layer
   the maintainer's intuition pointed at — extended to the full
   surface (not just `reqwest::Client`).
3. **`moagan-storage`** third. `storage/` + `reconcile/` +
   `telemetry/` (telemetry depends on storage, so it travels
   with it).
4. **`moagan-phases` fourth** — and **do not** split phases into
   per-phase crates. The 30 sub-modules in `src/phases/` share a
   `Pipeline` + `RunContext` + `PhaseOutput` kernel
   (`src/phases/pipe.rs`, `src/phases/phase.rs`) and per-phase
   extraction adds `pub use` inter-crate without decoupling
   anything real.
5. **`moagan-cli` fifth** (the existing binary, repackaged as a
   workspace member). Depends on `core`, `llm`, `storage`,
   `phases`. Thin orchestrator.

**Why `core` first**: extracting `llm` first would force it to
drag `RunContext` (5 443 LOC, in-degree 25+) along, recreating
a mini-monolith inside the `llm` crate. Extracting `core` first
gives `llm` a pure-types substrate to depend on without
re-implementing the run-state machinery.

**Why `phases` never split**: 27 KLOC of phase logic
(`src/phases/*.rs`) all share one `Pipeline` + `RunContext` +
`PhaseOutput` enum. Per-phase crates would import each other
across the workspace boundary anyway (e.g. `rank.rs` imports
`judge::Aggregated`, `synthesize.rs` imports
`cluster_proposals::ProposalCluster`,
`discover_matrix.rs` imports 5 `discovery::*` modules). Net
result: same compile graph, more `pub use` boilerplate, no
isolation gained.

### Why not "phases as binaries" (the maintainer's alternative)

Cross-process `phases` was evaluated as part of spike
[#829](https://github.com/airvzxf/moagan/issues/829) and scored
**2/10** for Moagan's current single-user, single-machine,
single-run shape. The blockers are concrete:

- **`Arc<parking_lot::Mutex<PromptCache>>`** in `RunContext`
  (D.6.4 hot-path) would be re-initialised per phase invocation.
  Cache hit-rate collapses; cross-run dedup (one of MoA's core
  value propositions) breaks.
- **`GovernorRegistry` AIMD** in `src/phases/phase.rs:30-43`
  would restart every spawn. AIMD backoff never converges
  without persistent state.
- **`heartbeat_handle: JoinHandle`** in `RunContext` is aborted
  on `Drop` (`src/phases/phase.rs:170`). Cross-process, that
  `Drop` no longer fires when the phase finishes — lease
  renewal stops mid-run.
- **`pipeline_span` with `run_id`** is `tracing::Span::enter`'d
  in-process; cross-process it would need OTLP externalisation
  or be broken.
- **`Pipeline::resume_with_kind`** (`src/phases/pipe.rs:405`) is
  in-memory today; cross-process it would need external state.
- **CI gauntlet grows linearly** with binary count, not
  proportionally. 60+ integration tests in `tests/` would each
  multiply against N phase binaries.
- **CLI orchestration**: `moagan run --mode deep` would have
  to spawn 15 children and stitch their stderr — shell
  scripting disguised as Rust.

External precedents are uniformly monolithic: aider (one CLI),
SWE-agent (recently **simplified** with `mini-swe-agent` 100
LOC), gpt-engineer (one CLI + one unrelated bench binary),
Letta (OSS is monolith, microservices are SaaS-only).

Reopens only if Moagan migrates to a multi-tenant SaaS with
hundreds of concurrent runs, where fault isolation per phase
outweighs the in-memory state loss. Not in scope today.

## Consequences

### Trade-offs accepted

- **Single binary deploy**: one AGPL-3.0 binary per release.
  Easy packaging (`tar` / `deb` / `apk`).
- **Atomic versioning**: one `Cargo.toml` version, one tag per
  release. The `verify-tag-reachability` guard in `release.yml`
  pins this invariant.
- **No cross-crate API surface to maintain**: every internal
  `pub(crate)` stays `pub(crate)`; no semver bump for internal
  reorganisation.
- **One `Cargo.lock`**: zero risk of drift between members.
- **One `target/`, one cold build**: ~6 min cold CI; not
  reducible without remote cache (out of scope).
- **Predictable CI gauntlet**: the 8 required checks assume a
  single `Cargo.toml`; they keep working as-is.

### Benefits foregone

- **Per-team parallel compilation**: not relevant with 1
  maintainer (or even 2).
- **Per-crate publishing**: not relevant without external
  consumers.
- **Cross-crate feature divergence**: handled by `#[cfg(feature)]`
  inside the single crate (precedent: `dag`, `coverage`).
- **Affected-target incremental CI**: handled by
  `cargo test -p <filter>` and `scripts/gauntlet.sh`'s tiered
  model.

### Documentation updates

- `AGENTS.md` §"Stack" remains authoritative. The descriptive
  "single crate, no workspace" line is now backed by an ADR.
- `docs/adr/0007-monolith-default.md` (this file) is the
  reference for the decision.
- Spike [#829](https://github.com/airvzxf/moagan/issues/829)
  holds the raw 12-angle investigation.
- EPIC [#830](https://github.com/airvzxf/moagan/issues/830)
  holds the action plan.

## References

- Spike issue:
  [#829](https://github.com/airvzxf/moagan/issues/829)
  — the 12-angle parallel investigation.
- EPIC:
  [#830](https://github.com/airvzxf/moagan/issues/830)
  — the action plan.
- Sub-issues: #831 (this ADR), #832 (`make profile-build`),
  #833 (`docs/MAINTAINERS.md`), #834 (feature flags per
  validator), #835 (library-driven `PhaseFactory`).
- Related ADRs:
  [0001](../../docs/adr/0001-no-go-list-policy.md) (no-go list),
  [0002](../../docs/adr/0002-runtime-coverage.md) (coverage),
  [0005](../../docs/adr/0005-verify-tag-signature-guard.md)
  (tag signature guard),
  [0006](../../docs/adr/0006-discover-test-structural-validation.md)
  (most recent accepted ADR; number occupied).
- Project canon:
  [`AGENTS.md`](../../AGENTS.md) (operational rules),
  [`docs/branch-protection.md`](../../docs/branch-protection.md)
  (required CI checks),
  [`docs/events-reference.md`](../events-reference.md) (NDJSON event schema).
