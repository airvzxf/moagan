# D-1 — Multi-provider profile (viability)

> **Status**: Viability document (Phase 1). Implementation lands in a
> follow-up PR after this document is merged (Phase 2).
> **Date**: 2026-09-03
> **Deciders**: `airvzxf/moagan` operator + Phase A subagent.
> **Scope**: Tanda 04e — D-1, `--temperature-profile` multi-provider.

## Context

`moagan discover` takes a `--provider SECTION:MODEL` argument that pins
the orchestrator (intake, clarify, persona/angle pickers, sketch
fan-out, tagger, cluster, facet, extract, integrator, summary) to one
provider + one model. The `--temperature-profile` flag, introduced by
PR-D1, already supports per-provider sampling-temperature profiles,
but the profile key is the **MODEL name only** (e.g. `provider=MiniMax-M3`),
and the coordinator fans out sketches through the SINGLE provider
that `--provider` resolved to. Profiles for any other model live on
the matrix but are silently ignored by the loop.

The user's ask (Tanda 04e):

> "Que en las temperaturas puedas usar otros proveedores con otros
> modelos. Si puedo poner un proveedor con un modelo, luego otro
> proveedor distinto con otro modelo y otro proveedor con tres
> modelos diferentes, esa es la verdadera diversidad que se espera."

In plain words: an operator wants the orchestrator/intake phases to
stay on `--provider SECTION_A:MODEL_A` (the "base model"), but the
sketch fan-out to ALSO accept profiles for `SECTION_B:MODEL_B`,
`SECTION_C:MODEL_C`, etc., so each provider contributes to the
exploration.

## Surface today (what exists, what is missing)

| Capability | Status |
|---|---|
| `--provider SECTION:MODEL` for orchestrator | Yes (`v0.10`, enforced) |
| `--temperature-profile` per-model profiles keyed by MODEL name | Yes (`PR-D1`, v0.13.x) |
| `ExplorationMatrix.temperature_profiles: HashMap<String, TempProfile>` keyed by MODEL name | Yes |
| Coordinator loop dispatches per the `default_model` profile only | Yes (limitation) |
| Coordinator fan-out across MULTIPLE `(section, model)` pairs | **No** — this is the gap |
| `--temperature-profile` keys by `(section, model)` pair | **No** — this is the gap |
| `ProviderRegistry` registry-key is `section::model_id` (or plain `section` for legacy single-model sections) | Yes (`PR-04b-1`) |
| `ProviderRegistry::get_model(section, model_id)` lookup | Yes |
| `ProviderRegistry::registry_from_config_with_sink_active(spec_map, …, Some(&active_pairs))` builds only the listed pairs | Yes (PR-x23) |

So the **registry already knows how to host multiple `(section, model)`
pairs in one run** — it was specifically designed for that with the
`active_pairs` filter. What's missing is the **wiring** from the
operator-facing flag to the loop:

1. The CLI flag currently stores `provider = <model name>` only. The
   section name is implicit (it must match the section from
   `--provider`).
2. The coordinator at
   `src/discovery/coordinator.rs:755` does
   `let profile = matrix.profile_for(&ctx.default_model).clone();`
   — single lookup. Any profile whose key is not `ctx.default_model`
   is dropped on the floor.
3. The dispatch site (`ctx.call_with_retry_at_temp` /
   `ctx.call_uncached_at_temp`) uses `ctx.default_provider` /
   `ctx.default_model` as the section/model pair — there is no
   per-iteration override path today.

## Decision (viability → recommended path)

**D-1 is viable as a feature**, scoped as:

1. **CLI grammar extension**: `--temperature-profile` accepts the
   existing `provider=<model>` form (kept as-is for backward
   compatibility — the section is implicit) AND a new
   `provider=<section>:<model>` form (or equivalent
   `provider=<section>;model=<model>` split-key form — TBD in
   implementation). Both forms must be supported by the same parser;
   the `section::model` join key matches the registry's
   `ProviderRegistry::registry_key(section, model_id)` so the
   coordinator can index `temperature_profiles` directly by that key.

2. **Persistence schema**: the on-disk
   `<run_dir>/exploration_matrix.json` keeps the same field
   (`temperature_profiles: HashMap<String, TemperatureProfile>`) but
   the key changes from `model_id` to the joined
   `ProviderRegistry::registry_key(section, model_id)` string (e.g.
   `"minimax::MiniMax-M3"`). On read-back, the resume path resolves
   the key back into a `(section, model)` pair via
   `ProviderRegistry::registry_key` (the inverse mapping is just
   `split_once("::")`).

3. **Coordinator loop fan-out**: change the loop at
   `src/discovery/coordinator.rs:833` from
   `for cell in cells { for temperature in profile_temperatures … }`
   (one provider × temperatures × replicas × cells × sketches_per_cell)
   to
   `for (section, model, profile) in active_provider_profiles { for cell in cells … }`
   where `active_provider_profiles` is the merged list of every
   profile's `(section, model)` pair PLUS the default provider (with
   the matrix's `default_profile`) when no explicit profile is set
   for the default. The loop dispatches each iteration through
   `ctx.call_with_retry_at_temp_for(section, model, role, …)` (new
   helper, see §"Helper APIs" below).

4. **`DiscoverMatrixPhase` parity**: the deprecated
   `src/phases/discover_matrix.rs` path (still wired as
   `discover_matrix_phase` in the legacy pipeline) gets the same
   fan-out treatment. Either delegate to the coordinator loop (the
   coordinator is the source of truth since v0.13), or mirror the
   logic so a phase-only run also benefits.

5. **Viability evaluation for OTHER subcommands/modes** — see
   §"Applicability to other subcommands" below. **Phase 2 implements
   `moagan discover` only**; the other modes get a one-paragraph
   evaluation that lives in this document, no code change.

### Helper APIs to add (Phase 2 scope)

* `RunContext::call_with_retry_at_temp_for(&self, section, model, role, system, user, attempt, temperature) -> Result<Response>`
  — like the existing `call_with_retry_at_temp`, but pinned to a
  specific `(section, model)` pair instead of `default_provider /
  default_model`. The implementation looks up the provider via
  `self.providers.get_model(section, model)` and threads the cache
  key through the same hash the existing helper computes (with the
  `(section, model)` pair mixed in so two providers answering the
  same `Role::Sketch` prompt do not collide in the cross-run cache).

* `RunContext::call_uncached_at_temp_for(...)` — same shape, no
  cache lookup.

* `ExplorationMatrix::active_provider_profiles(default_section, default_model) -> Vec<(String, String, TemperatureProfile)>`
  — enumerates the per-`(section, model)` profiles that the
  coordinator should fan out across, falling back to the matrix's
  `default_profile` for any provider without an explicit entry.
  Returns at minimum `[(default_section, default_model, default_profile)]`
  so the unconfigured case stays byte-identical to today (one
  provider, one profile, one sketch per `(cell, replica)`).

### Backward compatibility

The PR MUST keep the following contracts intact so existing runs are
byte-identical:

* The default profile (`[1.0] × 1`) is still `default_profile` when
  the operator passes no `--temperature-profile` flag and no
  `[discovery_matrix].temperature_profiles` block. The
  `active_provider_profiles` enumeration must collapse to
  `[(default_section, default_model, default_profile)]` so the
  coordinator loop fires the same `cells × sketches_per_cell`
  sketches against the same provider at the same temperature.
* The on-disk `<run_dir>/exploration_matrix.json` from v0.14.x is
  still readable. Reading a v0.14.x sidecar with `temperature_profiles`
  keyed by MODEL NAME (e.g. `MiniMax-M3`) needs a migration step:
  if the key matches `ctx.default_model` AND is NOT a valid
  `section::model` join, treat it as a legacy entry that should be
  re-keyed under `default_section::default_model` at load time. The
  migration is a one-time, in-memory rewrite — the file is rewritten
  in-place on the next `write_json`.
* `moagan doctor --json` keeps reporting the same schema version.
* Existing tests in `tests/integration_pr19_stop_policy.rs` and
  `tests/integration_discover.rs` keep passing unchanged.

### Files the implementation is expected to touch (estimate)

| File | Lines (rough) | Why |
|---|---|---|
| `src/cli/discover.rs` | +60 / -20 | New `TemperatureProfileSpec::parse` keys + post-merge to (section, model); active-pairs collection; registry wiring |
| `src/cli/mod.rs` | +30 / -10 | Flag doc; merge order |
| `src/cli/run.rs` | +15 / -5 | `build_registry_for_with_active` accepts a wider active-pairs list (or new helper) |
| `src/discovery/matrix.rs` | +80 / -10 | `active_provider_profiles` helper; key migration on read; persistence schema stays the same field but the key changes |
| `src/discovery/matrix_spec.rs` | ±5 | No shape change |
| `src/discovery/coordinator.rs` | +120 / -30 | Multi-provider fan-out; per-iteration dispatch through new helpers; saturation tracker target scales with provider count |
| `src/phases/discover_matrix.rs` | +60 / -20 | Same fan-out (mirror coordinator) |
| `src/phases/phase.rs` (`RunContext`) | +50 / -10 | `call_*_at_temp_for(section, model, …)` helpers; cache key gets `(section, model)` |
| `src/llm/provider.rs` | ±5 | (Optional) helper to enumerate `(section, model)` pairs from a registry |
| `src/config/mod.rs` | +20 / -10 | Persistent `[discovery_matrix].temperature_profiles` keys migration note; default_profile behaviour unchanged |
| `tests/integration_discover.rs` | +80 / -20 | New multi-provider integration test |
| `tests/integration_discovery.rs` | +40 / -10 | Multi-provider registry fixture |
| `docs/viability/multi-provider-profile.md` | (this file) | Phase 1 artefact |
| `CHANGELOG.md` | +20 | `[0.14.3]` entry under "Added" |

Total estimate: ~700–900 LOC (counting tests + comments + doc).

## Applicability to other subcommands / modes

Per the Tanda 04e brief, the user asked for an **evaluation** (not an
implementation) of the multi-provider profile idea in the rest of the
CLI. Summary verdict:

| Subcommand / mode | Multi-provider profile applicable? | Why |
|---|---|---|
| `moagan discover` | **YES (Phase 2 implements this)** | The whole point of `discover` is mass exploration; multi-provider fan-out is the canonical use case. |
| `moagan run --mode fast` | **No** | Fast mode skips `SketchPhase` entirely; there is nothing to fan out across providers. |
| `moagan run --mode standard/deep/batch` | **VIABLE, deferred** | `SketchPhase` (`src/phases/sketch_phase.rs`) uses `ctx.call_with_retry_parse` with the single `default_model`. A future follow-up could give `SketchPhase` a per-call `(section, model)` selector by introducing a `MultiProviderSketchFanout` helper that consumes the same `ExplorationMatrix::active_provider_profiles` enumeration. Sketch diversity would rise, but the orchestrator (intake/clarify/propose/judge) would still be on the default provider — same constraint as `moagan discover`. |
| `moagan run --mode explore` | **VIABLE, deferred** | Same reasoning as `standard/deep/batch`. The angle-cycled `SketchPhase` would just rotate through `(section, model)` pairs instead of `DEFAULT_ANGLES`. |
| `moagan probe <verb>` | **No (already supports multiple pairs)** | `moagan probe max_tokens` and `moagan probe temperature` already accept a `Vec<(String, String)>` of pairs (see `src/cli/probe.rs:90-99`). No further change needed. |
| `moagan run --mode discover` (alias) | **YES** (covered) | Routes to `moagan discover` via `PipelineKind::Discovery`. |
| `moagan continue` / `moagan resume` / `moagan rerun` | **NO (deferred)** | These flows re-execute against the persisted `manifest.json` from the original run. The original run's profile key (model name vs `section::model`) is migrated on read so the resume picks up the new schema automatically; no explicit multi-provider flag needed here. |

**Bottom line**: only `moagan discover` is in scope for Phase 2. The
other paths are **architecturally feasible** but explicitly out of
scope for Tanda 04e.

## Phased rollout

| Phase | Artefact | Gate |
|---|---|---|
| 1 (this doc) | `docs/viability/multi-provider-profile.md` | merge + operator sign-off |
| 2 (impl) | Multi-provider profile wire-up | `make lint`, `make build`, `make test-ci`, `make smoke`, `make e2e` all green; CHANGELOG entry under `[0.14.3]` |
| 3 (deferred) | Multi-provider `SketchPhase` for `standard/deep/batch/explore` | follow-up tanda |

## Risks

1. **Cache-key collision** — the cross-run cache key in
   `src/llm/cache/mod.rs` currently mixes `(role, prompt_hash,
   temperature)` only. Two providers answering the same prompt at
   the same temperature would collide. The new `call_*_at_temp_for`
   helper MUST mix `(section, model)` into the cache key.
2. **Saturation tracker scaling** — `target = matrix.cardinality()` is
   currently `cells × sketches_per_cell`. With multi-provider the
   total grows to `cells × sketches_per_cell × Σ profile_total`. The
   saturation tracker already anchors against `total.max(target)`
   (`src/discovery/coordinator.rs:780`); the multi-provider total
   must be passed in instead of the single-provider one.
3. **Rate-limit + circuit-breaker per provider** — the existing
   `attach_parallelism_rate_limit` and `BreakerRegistry` already key
   per `Role` + `default_provider`. The new helper needs the per-call
   `(section, model)` to be honoured by the rate-limiter lookup. If
   a provider is missing from `[rate_limit_per_provider]`, the
   global `effective_rate_limit` cap (parallelism) applies — same
   behaviour as today.
4. **Sidecar migration** — see §"Backward compatibility". A v0.14.x
   sidecar with `temperature_profiles["MiniMax-M3"] = …` and a
   v0.14.3+ run with `--provider minimax:MiniMax-M3` must
   transparently re-key the entry to `temperature_profiles["minimax::MiniMax-M3"]`.
5. **Audit log readability** — operators reading the JSONL sidecar
   today see `default_model` (e.g. `MiniMax-M3`). The new helper
   stamps `(section, model)` on every LLM call event so the audit
   log keeps the full identifier. No silent downgrade.
6. **CLI grammar ambiguity** — the new `provider=<section>:<model>`
   form contains a `:`. The existing
   `parse_provider_model(<section>:<model>)` splits on the first
   `:`. The spec parser must call `parse_provider_model` BEFORE
   the section/model split to avoid ambiguity with the
   `temperatures=<csv>` list (the `,` already breaks CSV parsing;
   the `:` does not, so the order of `key=value` segments inside
   the spec is `provider=…;temperatures=…,…;replicas=N` and the
   `provider=…` segment is parsed last so it does not eat the
   `temperatures` segment).

## Re-evaluation triggers

This decision will be revisited when any of the following happen:

1. The `[0.14.3]` release ships and operators file issues about the
   cache-key migration (§"Risks" #1) — the audit log's per-call
   `(section, model)` must be enough to debug; if not, the
   cache-key builder needs a richer envelope.
2. A future PR proposes to bring multi-provider fan-out to
   `moagan run --mode {standard,deep,batch,explore}` (Tanda 04e+
   follow-up). The viability note in §"Applicability to other
   subcommands" is the starting point.
3. The persistence schema bumps to `exploration_matrix.json` schema
   version 2 (`matrix-v2`) because the join key changed. The
   migration is in-memory only today; if the on-disk wire format
   changes further, an explicit schema bump is required.

## Alternatives considered

* **A) Drop multi-provider, only support per-model profiles on the
  same `--provider` section.** Rejected — the user's ask is
  specifically about cross-provider diversity, which this option
  does not enable.
* **B) Introduce a separate `--extra-providers` flag that adds
  profiles for additional sections without touching
  `--temperature-profile`.** Rejected — splits the surface and the
  audit log across two flags. Better to extend `--temperature-profile`
  so every per-call decision is in one place.
* **C) Replace `HashMap<String, TempProfile>` with
  `Vec<TempProfile>` and let the operator pass multiple
  `--temperature-profile` flags (today's behaviour) but with the
  new `provider=<section>:<model>` key.** **Adopted** — minimal
  surface change, the merge order stays "last-wins per
  `(section, model)` pair", and the existing CLI keeps working for
  the single-provider case.

## Cross-references

* `src/cli/discover.rs:337-489` — current `TemperatureProfileSpec`
  parser.
* `src/cli/discover.rs:644-680` — current CLI → `effective_cfg`
  merge logic for temperature profiles.
* `src/discovery/coordinator.rs:538-602, 745-758` — current
  single-provider fan-out site.
* `src/discovery/matrix.rs:68-96, 293-322` — current
  `TemperatureProfile` + `ExplorationMatrix::temperature_profiles`.
* `src/phases/discover_matrix.rs:317-340` — deprecated phase fan-out.
* `src/cli/probe.rs:641-669` — `parse_provider_model` (canonical
  `section:model` split).
* `src/llm/provider.rs:421-427` — `ProviderRegistry::registry_key`.
* `src/config/mod.rs:402-510` — persistent
  `[discovery_matrix]` block.
* AGENTS.md §"Coding conventions" — type-driven design + idiomatic
  Rust.
* AGENTS.md §"No-go list" — no new dependencies are needed; this is a
  pure-Cargo-feature implementation.

---

*This document is the canonical Phase-1 artefact for Tanda 04e / D-1.
The Phase-2 implementation lands in a follow-up PR; once that PR
merges and the `[0.14.3]` release ships, this doc moves to
`docs/adr/0006-multi-provider-profile.md` as a historical ADR.*