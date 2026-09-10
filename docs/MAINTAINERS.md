# Moagan MAINTAINERS

Ownership map for `moagan`'s subsystems. Each row names one or
more maintainers who are the **point of contact** for changes
inside that subsystem. The model is borrowed from FFmpeg
[`MAINTAINERS`](https://ffmpeg.org/developer.html#Submitting-patches)
(developer documentation §9.1).

The project has a single maintainer today (`@airvzxf`). The
table is structured to absorb multiple owners per row when a
co-maintainer joins; ADR-0005 §"Re-evaluation §1" is the
trigger.

## Scope of "ownership"

Listed owners are the **first people to CC** on a PR that
touches their subsystem. They are **not a hard gate**: an
absent owner does not block a PR. AGENTS.md's commit policy
(GPG-signed, conventional commit, gauntlet green) remains the
load-bearing contract; the table is awareness, not authority.

When a PR crosses multiple subsystems, every listed owner is
asked for review. When a PR has subsystem-level concerns a
listed owner raises, that owner has the **soft veto** to ask
for a follow-up commit; the merge still proceeds at the
maintainer's discretion.

## Linear pipeline phases

These phases run sequentially in the canonical pipeline
(`src/phases/pipe.rs::Pipeline::canonical_phase_order_for`).
Each phase reads the previous phase's sidecar artefact from
the run directory and writes its own. The `pub use` re-exports
in `src/phases/mod.rs:44-82` are the public contract.

| Subsystem                       | Path                              | Owner(s)  | Notes |
|---------------------------------|-----------------------------------|-----------|-------|
| `intake`                        | `src/phases/intake.rs`            | @airvzxf  | First phase; canonical prompt path; writes `<run_dir>/intake.json`. |
| `clarify`                       | `src/phases/clarify.rs`           | @airvzxf  | Reads/writes `<run_dir>/brief.json`. |
| `route`                         | `src/phases/route.rs`             | @airvzxf  | Picks the pipeline variant (linear vs discovery). |
| `sketch_phase`                  | `src/phases/sketch_phase.rs`      | @airvzxf  | Skipped in `fast` mode; gated by `Mode::runs_sketches()`. |
| `propose`                       | `src/phases/propose.rs`           | @airvzxf  | Writes `<run_dir>/proposals/p_*.json`. |
| `gate`                          | `src/phases/gate.rs`              | @airvzxf  | Hard gate: passes/forwards or fails. |
| `validate`                      | `src/phases/validate.rs`          | @airvzxf  | Runs per-validator feature flags (see #834). |
| `critique`                      | `src/phases/critique.rs`          | @airvzxf  | Self-critique pass. |
| `cluster_proposals`             | `src/phases/cluster_proposals.rs` | @airvzxf  | Phase D — simhash cluster of proposals. |
| `synthesize`                    | `src/phases/synthesize.rs`        | @airvzxf  | Phase D — merges clusters into candidate synthesis. |
| `repair`                        | `src/phases/repair.rs`            | @airvzxf  | JSON repair + retry budget. |
| `judge`                         | `src/phases/judge.rs`             | @airvzxf  | Aggregated scoring; hosts the adversary branch. |
| `adversary`                     | `src/phases/adversary.rs`         | @airvzxf  | Pattern-adversary probe. |
| `rank`                          | `src/phases/rank.rs`              | @airvzxf  | Pareto + cluster ranking; writes `<run_dir>/rankings/ranking.json`. |
| `deliver`                       | `src/phases/deliver.rs`           | @airvzxf  | Writes `<run_dir>/final/portfolio.md`. |

## Discovery pipeline phases

These phases fan-out / saturate via `DiscoveryCoordinator`
(`src/discovery/coordinator.rs`). They run when `Mode::runs_discovery()`
is true; the discovery pipeline is the "Plan B" branch.

| Subsystem                       | Path                                       | Owner(s)  | Notes |
|---------------------------------|--------------------------------------------|-----------|-------|
| `discover_matrix`               | `src/phases/discover_matrix.rs`            | @airvzxf  | Seeds the matrix from the brief. |
| `discover_tag`                  | `src/phases/discover_tag.rs`               | @airvzxf  | Tags dimensions. |
| `discover_cluster`              | `src/phases/discover_cluster.rs`           | @airvzxf  | Clusters dimensions into facets. |
| `discover_contradict`           | `src/phases/discover_contradict.rs`        | @airvzxf  | Adversarial probe of the matrix. |
| `discover_facet`                | `src/phases/discover_facet.rs`             | @airvzxf  | Per-facet extraction. |
| `discover_extract`              | `src/phases/discover_extract.rs`           | @airvzxf  | Extracts candidates per facet. |
| `discover_integrate`            | `src/phases/discover_integrate.rs`         | @airvzxf  | Integrates facets into a draft. |
| `discover_summary`              | `src/phases/discover_summary.rs`           | @airvzxf  | Writes the discovery summary sidecar. |

## Pipeline internals (kernel + helpers)

These are not real `Phase` impls but are the substrate the
linear + discovery pipelines share. They are load-bearing; any
change here touches nearly every phase above.

| Subsystem                       | Path                                   | Owner(s)  | Notes |
|---------------------------------|----------------------------------------|-----------|-------|
| `phases::phase` (kernel)        | `src/phases/phase.rs`                  | @airvzxf  | The 5 443-LOC kernel: `pub trait Phase`, `PhaseOutput`, `RunContext`, telemetry wiring, heartbeat. Per ADR-0007 §"Split order", this is the *one* module whose extraction would force a workspace; do not extract it without ADR-0008. |
| `phases::pipe` (Pipeline)       | `src/phases/pipe.rs`                   | @airvzxf  | The `Pipeline` runner + `PipelineKind` + canonical-order table. |
| `phases::util`                  | `src/phases/util.rs`                   | @airvzxf  | `read_json` / `write_json` sidecar helpers used by every phase. |
| `phases::dag`                   | `src/phases/dag.rs`                    | @airvzxf  | Optional DAG execution; gated by `--features dag` (ADR-0001 §D-1). |
| `phases::budget`                | `src/phases/budget.rs`                 | @airvzxf  | `PressureLevel` + `BudgetObserver`; consumed by `Pipeline`. |
| `phases::cardinality`           | `src/phases/cardinality.rs`            | @airvzxf  | Cardinality + parallelism helpers. |
| `phases::refine`                | `src/phases/refine.rs`                 | @airvzxf  | `RefineContext` + dispatch helpers. |
| `phases::replace`               | `src/phases/replace.rs`                | @airvzxf  | Replacement helpers consumed by `refine`. |

## LLM layer (`src/llm/`)

The transport + resilience layer. All of `phases/*` consumes
this via `RunContext` (`src/phases/phase.rs:30-43`).

| Subsystem                       | Path                                                                                       | Owner(s)  | Notes |
|---------------------------------|--------------------------------------------------------------------------------------------|-----------|-------|
| `llm::provider`                 | `src/llm/provider.rs`, `src/llm/{minimax,openai_compat,deepseek,anthropic_compat,openai_compatible}.rs` | @airvzxf | `pub trait Provider` + per-vendor impls; `ProviderRegistry` + `ProviderPool`. |
| `llm::transport`                | `src/llm/{http,wire,wire_format,sse_parser,response_format_opt_out}.rs`                    | @airvzxf  | The "raw HTTP" layer the maintainer's spike pointed at; `reqwest + rustls`. |
| `llm::cache`                    | `src/llm/prompt_cache.rs`, `src/llm/mod.rs` (cache sub-module)                              | @airvzxf  | Cross-run + in-memory cache; consolidated under `llm/mod.rs` (no standalone `cache.rs` file). |
| `llm::prompt_cache`             | `src/llm/prompt_cache.rs`                                                                  | @airvzxf  | In-memory `parking_lot::Mutex`-guarded D.6.4 hot-path. Per ADR-0007, this is the *load-bearing* state that argues against phases-as-binaries. |
| `llm::circuit_breaker`          | `src/llm/circuit_breaker.rs`                                                               | @airvzxf  | Per-`(provider, role)` AIMD breaker. |
| `llm::governor`                 | `src/llm/governor.rs`                                                                      | @airvzxf  | `ThrottleGovernorRegistry`; 429 AIMD. |
| `llm::rate_limiter`             | `src/llm/rate_limiter.rs`                                                                  | @airvzxf  | Per-role rate limiter. |
| `llm::probe_table`              | `src/llm/probe_table.rs`                                                                   | @airvzxf  | Auto-discovery of `max_tokens`. |
| `llm::temperature_probe`        | `src/llm/temperature_probe.rs`                                                             | @airvzxf  | Auto-discovery of `temperature` support. |
| `llm::models_dev`               | `src/llm/models_dev.rs`                                                                    | @airvzxf  | `models.dev` catalog fetch. |
| `llm::param_rejections`         | `src/llm/param_rejections.rs`                                                              | @airvzxf  | `ParamRejectionsTable`. |
| `llm::capability`               | `src/llm/capability.rs`                                                                    | @airvzxf  | `CapabilityResolver`. |
| `llm::prompts`                  | `src/llm/prompts.rs`                                                                       | @airvzxf  | The shared prompt catalogue. |

## Cross-cutting subsystems

| Subsystem                       | Path                                                | Owner(s)  | Notes |
|---------------------------------|-----------------------------------------------------|-----------|-------|
| `cli`                           | `src/cli/**`                                        | @airvzxf  | clap sub-commands; `moagan run / inspect / doctor / coverage / …`. Hosts `build_pipeline_for_mode` (see #835). |
| `config`                        | `src/config/**`                                     | @airvzxf  | `Config` + profiles + env overrides. |
| `domain`                        | `src/domain/**`                                     | @airvzxf  | Single source of truth for the JSON types (`Brief`, `Proposal`, `Ranking`, …). |
| `error`                         | `src/error/**` (`mod.rs` etc.), `src/error_code.rs` | @airvzxf  | `Result`, `Error`, `ExitCode`, `ErrorCode`. The single in-degree-126 hub. |
| `storage`                       | `src/storage/**`                                    | @airvzxf  | `rusqlite + r2d2 + migrations + lease + outbox + compression`. |
| `telemetry`                     | `src/telemetry/**`                                  | @airvzxf  | NDJSON `Event` stream + dashboard CSV + heartbeat. Per ADR-0002, every event carries `file:line:column`. |
| `audit`                         | `src/audit/**`                                      | @airvzxf  | Proxy HTTP + verify + format. |
| `checkpoint`                    | `src/checkpoint/**`                                 | @airvzxf  | Human checkpoint (`dialoguer`). |
| `research`                      | `src/research/**`                                   | @airvzxf  | External HTTP fetcher + PDF + allowlist. |
| `ranking`                       | `src/ranking/**`                                    | @airvzxf  | Pareto + simhash cluster + adversary-pattern helpers. |
| `sandbox`                       | `src/sandbox/**`                                    | @airvzxf  | seccomp / cgroup / namespace / process. |
| `validators`                    | `src/validators/**`                                 | @airvzxf  | Per-validator feature flags land in #834. |
| `coverage`                      | `src/coverage/**`                                   | @airvzxf  | Runtime SanCov coverage; gated by `--features coverage` (ADR-0002). |
| `context`                       | `src/context/**`                                    | @airvzxf  | Context loader/resolver. |
| `redact`                        | `src/redact/**`                                     | @airvzxf  | `RedactPolicy` + `RedactWriter`. |
| `reconcile`                     | `src/reconcile/**`                                  | @airvzxf  | Outbox transaction reconciliation. |
| `preferences`                   | `src/preferences/**`                                | @airvzxf  | Ratings cache. |
| `secret`                        | `src/secret.rs`                                     | @airvzxf  | `SecretString` (zeroize). |
| `cancel`                        | `src/cancel.rs`                                     | @airvzxf  | `Cancel` + `HARD_KILL_GRACE`. |
| `execution`                     | `src/execution/**`                                  | @airvzxf  | `Parallelism` + per-provider semaphores. |
| `ids`                           | `src/ids.rs`                                        | @airvzxf  | `RunId` + `blake3_hex` + `canonical_hash`. |
| `time`                          | `src/time.rs`                                       | @airvzxf  | `now_unix_secs` / `now_unix_millis`. |
| `fs_layout`                     | `src/fs_layout.rs`                                  | @airvzxf  | `MoaganHome`, `RunDir`, paths canónicos. |
| `serde_util`                    | `src/serde_util/**`                                 | @airvzxf  | `clean_f32` + serde helpers. |
| `atomic`                        | `src/atomic/**`                                     | @airvzxf  | `AtomicWriter` (escrituras atómicas). |
| `cli::Mode` enum                | `src/cli/mod.rs` (enum `Mode`)                      | @airvzxf  | The mode discriminator; per spike angle #1 this is the one enum that **lives in `cli`** but is semantically a domain concept — promote to `domain` if a workspace split ever fires (ADR-0007 §"Split order"). |

## Release + tooling

| Subsystem                       | Path                                                                            | Owner(s)  | Notes |
|---------------------------------|---------------------------------------------------------------------------------|-----------|-------|
| Release pipeline                | `Makefile`, `scripts/gauntlet.sh`, `release.yml`, `.github/trusted-signers*`   | @airvzxf  | Tag-signature guard per ADR-0005. Tag-reachability guard per AGENTS.md §"⚠️ Red workflows". |
| CI gauntlet                     | `.github/workflows/`, `lefthook.yml`, `scripts/check-*.sh`                      | @airvzxf  | 8 required CI checks + lefthook pre-commit/pre-push. |
| Documentation / ADRs            | `docs/adr/**`, `AGENTS.md`, `docs/branch-protection.md`, `docs/events-reference.md`, `docs/cli-reference.md`, `docs/test-skips-report.md`    | @airvzxf  | ADR-NNNN format; numbering is sequential; consult the latest ADR before proposing a structural change. The CLI / events / test-skips docs are auto-generated by `moagan-docgen` (EPIC #852); edit the source and re-run, do not hand-edit. |
| Validation tiers                | `AGENTS.md §"Validation tiers"`, `lefthook.yml`                                  | @airvzxf  | T0 / T1 / T2 / T3 split; escape hatches documented in `AGENTS.md`. |

## How to add or remove an owner

**Add an owner**:

1. Append a `@github-handle` to the relevant row.
2. If the new owner uses PGP, append their ASCII-armored public
   key to `.github/trusted-signers.asc` (per ADR-0005).
3. If the new owner uses SSH, append a `<principal> <key-type>
   <key-body>` entry to `.github/trusted-signers`.
4. Document the signer's key fingerprint and identity in
   `.github/CONTRIBUTING.md`.
5. Open a PR; AGENTS.md requires GPG-signed commits and a
   green gauntlet.

**Remove an owner**:

1. Per ADR-0005: must wait until the most recent tag signed by
   that key is at least one minor version old, so a compromise
   of the removed key cannot rewrite a release that's in
   production.
2. Strip the `@github-handle` from the row.
3. Remove the corresponding entry from
   `.github/trusted-signers` (and `.github/trusted-signers.asc`
   if PGP).
4. Open a PR; the gauntlet's tag-signature gate re-verifies
   every `vX.Y.Z` tag locally, so a removed signer surfaces
   immediately for the operator who removes it.
