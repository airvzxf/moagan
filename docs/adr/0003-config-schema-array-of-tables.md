# ADR 0003 — `[[providers.<name>]]` array-of-tables config schema

> **Status**: Accepted
> **Date**: 2026-08-29
> **Deciders**: `airvzxf/moagan` operator + Phase B-1 subagents
> **Supersedes**: implicit v0.10 single-table + `Vec<ModelConfig>` shape
> in [`src/config/mod.rs`](../../src/config/mod.rs) (replaced by
> `ProviderEntry` + `SectionKnobs` in PR #1+2, commit `0145b31`).
> **Relates to**:
> [`src/config/mod.rs`](../../src/config/mod.rs) (new types + bridge),
> [`src/config/dual_mode.rs`](../../src/config/dual_mode.rs) (dual-mode
> deserializer, PR #3, commit `e2d74a2`),
> [`src/llm/max_tokens.rs`](../../src/llm/max_tokens.rs) (centralised
> resolver),
> [`config.example.toml`](../../config.example.toml) (rewritten to the
> new shape),
> [`docs/migrations/v0.12-to-v0.13-config.md`](../migrations/v0.12-to-v0.13-config.md)
> (operator migration guide).

## Context

The v0.10 / v0.11 / v0.12 schema treats every `[providers.<name>]`
section as a **single TOML table** whose `models[]` field is a list of
inline tables (`[{ id = "...", endpoint = "...", max_tokens = N }, …]`).
The on-disk shape works for single-endpoint providers (MiniMax,
DeepSeek, Mock) but three structural problems surfaced in v0.12.x that
the schema itself made hard to fix without a redesign.

1. **Section-level fields and per-model fields share the same namespace
   in confusing ways.** `temperature`, `top_p`, `omit_max_tokens`,
   `max_token_auto*`, `temperature_auto_enabled`, `plan` are
   section-level knobs. `id`, `endpoint`, `max_tokens` are per-model
   fields. The two kinds sit on the same `[providers.X]` table with no
   structural separation; an operator reading `models = [{id =
   "kimi-k3", endpoint = ".../chat/completions"}]` cannot tell whether
   the `endpoint` came from the section default or the per-model
   override without reading the dispatcher code. Operators routinely
   put section-level knobs at the bottom of the table and per-model
   fields at the top (or vice versa) — the conventions diverged over
   the v0.10 / v0.11 cycles and the documentation never settled.

2. **OpenCode already has three distinct wire formats on one
   provider.** As of v0.12.x, `[providers.opencode]` registers 19
   models, each with an inline-table `endpoint` that picks
   `/v1/chat/completions` (10 models), `/v1/messages` (7 models), or
   `/v1/responses` (2 models). The single-table + per-model `endpoint`
   shape is a workaround: the wire-format suffix lives inside an
   inline table field that serde flattens, and the dispatcher picks
   the format from the URL string at runtime (`src/llm/wire_format.rs`
   + `src/llm/provider.rs`). An operator adding a fourth endpoint has
   to know the v0.10 inline-table grammar and the wire-format suffix
   lookup table — neither of which is in `config.example.toml`.

3. **Per-model `max_tokens` lives in two places with no central
   resolver.** v0.12.x reads `models[].max_tokens` (operator-supplied)
   alongside the auto-probe cache (`<MOAGAN_HOME>/max_tokens_auto.toml`,
   managed by `src/llm/probe_table.rs`) and the kind-level hardcoded
   cap (`MINIMAX_MAX_TOKENS_CAP`, `DEEPSEEK_MAX_TOKENS_CAP` in
   `src/llm/capabilities.rs:35,57`). Seven call-sites in
   `src/llm/{minimax,deepseek,openai_compat,anthropic_compat,
   openai_compatible}.rs` each roll their own three-layer
   `min(operator, cache, kind_cap)` chain. None of them honour an
   `MOAGAN_<SECTION>_MAX_TOKENS` env var. A `OnceLock`-memoised
   resolver would speed up the per-call lookup (≥ 1 Hz per pipeline
   role × 10 000 calls = 20 000 allocs/run of `format!` +
   `to_uppercase().replace(...)`).

4. **`serde(flatten)` and `serde(deny_unknown_fields)` are
   incompatible.** The cleanest way to keep the section-level knobs
   accessible on every `[[providers.X]]` entry in the new shape is
   `#[serde(flatten)] knobs: SectionKnobs` on `ProviderEntry`. The
   known serde foot-gun is that `flatten` does not propagate
   `deny_unknown_fields` to the flattened sub-struct, so any unknown
   key at entry level would be silently accepted. We resolve the
   conflict by dropping `deny_unknown_fields` from the entry struct
   and relying on the post-load `Config::warn_unknown_provider_keys`
   walk (which inspects the raw TOML) to surface typos. The 16
   subagents' consensus (alpha/beta/eta) was unanimous on this point.

The redesign lives in **three PRs already merged on
`feature/v0.13.0-config-array-of-tables`**:

- PR #1 + #2 (commit `0145b31`): `ProviderEntry` + `SectionKnobs`
  types + `merge_first_wins` + rewrite `default_providers()` to the
  array-of-tables shape.
- PR #3 (commit `e2d74a2`): `dual_mode::deserialize_providers_map`
  + `dual_mode::deserialize_model_list` + central
  `resolve_max_tokens()` helper + migration of the 7 per-call
  `min(...)` chains to the helper.
- **PR #4 (this commit)**: documentation, ADR, migration guide,
  `config.example.toml` rewrite, CHANGELOG entry, and the
  integration-test / fixture surface.

## Decision

The v0.13.0 schema adopts a **bridge-pattern refactor** to
`[[providers.<name>]]` + `Vec<ProviderEntry>` with a computed
`providers_legacy: BTreeMap<String, ProviderConfig>` field that every
existing consumer reads. The dual-mode deserializer accepts both the
new shape (canonical) and the legacy single-table shape (with a
`tracing::warn!` per section) until v0.15, when legacy support is
removed. A single `resolve_max_tokens(section, model, table,
operator_cap, kind_hard_cap) -> u32` helper centralises the 5-rung
chain.

### D-1 — New shape: `[[providers.<name>]]`

```toml
[[providers.minimax]]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
models = ["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed", "MiniMax-M2.5"]

[[providers.opencode]]
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"
models = ["kimi-k3", "glm-5.1", "glm-5.3-flash", "deepseek-v4-flash", ...]
temperature = 1.0
top_p = 0.95

[[providers.opencode]]
endpoint = "https://opencode.ai/zen/go/v1/messages"
models = ["minimax-m3", "minimax-m2.7", "qwen3.7-max", ...]

[[providers.opencode]]
endpoint = "https://opencode.ai/zen/go/v1/responses"
models = ["gpt-5.6-luna", "muse-spark-1.2-contributor"]
omit_max_tokens = true

[[providers.mock]]
endpoint = "mock://local"
models = ["mock-model"]
```

Properties:

- **One `[[providers.X]]` entry per upstream endpoint.** A provider
  that fronts one URL (minimax, deepseek, mock) is a single entry;
  OpenCode's three wire formats become three entries on the same
  `[[providers.opencode]]` roof.
- **`endpoint` is required per entry.** Unlike v0.12, the
  array-of-tables form does not inherit a section-level default — every
  entry carries its own URL. The dual-mode deserializer rejects an
  entry with `endpoint = ""` with `Err(InvalidArgs("providers.X[i]:
  \`endpoint\` is required in v0.13"))`.
- **`models = ["<id>", ...]` is `Vec<String>`** (no inline tables).
  Per-model `endpoint` is gone (the entry carries it). Per-model
  `max_tokens` is gone — it's resolved centrally.
- **Section-level knobs can appear on any entry.** `temperature`,
  `top_p`, `omit_max_tokens`, `max_token_auto*`,
  `temperature_auto_enabled`, `plan` are accepted on every `[[…]]`
  entry; the bridge (`Config::compute_legacy_providers`) merges them
  with `SectionKnobs::merge_first_wins` when collapsing into the
  canonical `ProviderConfig`. Operators can spread the knobs across
  entries or keep them all on the first one — both forms parse
  identically.

### D-2 — Bridge pattern: `providers` (new) + `providers_legacy` (computed)

```rust
#[serde(deserialize_with = "dual_mode::deserialize_providers_map")]
pub providers: BTreeMap<String, Vec<ProviderEntry>>,

#[serde(skip)]
pub providers_legacy: BTreeMap<String, ProviderConfig>,
```

`compute_legacy_providers()` (in [`src/config/mod.rs`](../../src/config/mod.rs))
runs after `Config::load()` and collapses the new shape into the
canonical `ProviderConfig` shape every existing consumer
(`llm::provider::registry_from_config_with_home`, `cli::doctor`,
`cli::run`, `cli::telemetry`, …) reads. The conversion is:

1. Walk each section's entries. The first entry seeds the
   section-level knobs via `SectionKnobs::merge_first_wins`; later
   entries fill in slots the first entry left at the default.
2. Each entry's `models` becomes one `ModelConfig` per id, carrying
   the entry's endpoint on the per-model `endpoint` field so the
   dispatcher can pick the wire format from the URL.
3. Duplicate model ids across entries of the same section error out
   with `Error::InvalidArgs` so an operator typo surfaces immediately
   instead of silently shadowing the first registration.
4. The section-level `ProviderConfig::endpoint` becomes `None` for
   every section, **except `mock`** — the mock section's first-entry
   endpoint propagates to `ProviderConfig::endpoint` so `is_mock` in
   `src/llm/provider.rs:1351-1356` keeps working. That check queries
   `spec.endpoint.starts_with("mock://")`; without this corner the
  mock provider would silently fail to be recognised as mock after
  the bridge runs.

Public API of `ProviderConfig` is unchanged — the dispatcher,
audit pipeline, telemetry pipeline, and CLI all keep reading
`cfg.providers_legacy` exactly as they did in v0.12.x.

### D-3 — Dual-mode deserializer

The `dual_mode::deserialize_providers_map` and
`dual_mode::deserialize_model_list` helpers (in
[`src/config/dual_mode.rs`](../../src/config/dual_mode.rs)) accept
both shapes:

- **Array of tables** (`[[providers.X]]`):
  each element is a `ProviderEntry` table; `endpoint` is required,
  `models = ["…"]` is `Vec<String>`.
- **Single table** (`[providers.X]`):
  emits a `tracing::warn!(section = %name, "[providers.{name}] …")`
  on parse, then groups the legacy `Vec<ModelConfig>` by effective
  endpoint (entry-level override, else section-level). Models that
  resolve to the same URL land in one `ProviderEntry` so the bridge
  produces the same `(endpoint, models)` pair the operator intended.
  Per-model `max_tokens` is preserved via the
  `legacy_model_max_tokens: BTreeMap<String, u32>` side-channel; the
  bridge reads it to populate `ModelConfig::max_tokens` so a v0.12
  TOML continues to clamp the wire body until the operator upgrades
  the TOML.

The warning fires **once per section**, not once per model. A mixed
TOML (one section legacy, another new) loads both; only the legacy
section warns.

### D-4 — Centralised `resolve_max_tokens(section, model, table, operator_cap, kind_hard_cap) -> u32`

```rust
pub fn resolve_max_tokens(
    section: &str,
    model: &str,
    table: Option<&MaxTokensTable>,
    operator_cap: Option<u32>,
    kind_hard_cap: Option<u32>,
) -> u32
```

Five-rung chain (highest wins):

1. `MOAGAN_<SECTION>_MAX_TOKENS` env var (uppercased; dots / dashes
   folded to underscores so `opencode-go` resolves to
   `MOAGAN_OPENCODE_GO_MAX_TOKENS`). Out-of-range or unparseable
   values fall through with a `tracing::warn!`.
2. `MaxTokensTable::get(section, model)` cache (filtered by
   `>= MIN_AUTOPROBE_FLOOR` so a manually-edited sidecar cannot leak
   a degenerate value).
3. `operator_cap` (the per-model knob the operator set in the
   v0.12 legacy TOML — propagated via the `legacy_model_max_tokens`
   side-channel for legacy entries, or `Some(n)` for new entries
   that the runtime populates from the v0.12.18 follow-up).
4. `kind_hard_cap` (the kind-level safety net —
   `MINIMAX_MAX_TOKENS_CAP`, `DEEPSEEK_MAX_TOKENS_CAP`, etc.).
5. `DEFAULT_MAX_TOKENS = 1M` (the documented default; not
   `MAX_AUTOPROBE_CEILING = 2^30` because MiniMax rejects values
   > 524 288 with HTTP 400).

The helper replaces the seven hand-rolled three-layer `min(...)`
chains in `minimax.rs`, `deepseek.rs`, `openai_compat.rs`,
`anthropic_compat.rs`, and `openai_compatible.rs`. Memoisation via
`OnceLock<RwLock<HashMap<(String, String), u32>>>` is documented as a
**v0.13.x follow-up** (deferred to TIER 2 per the 16-subagent
consensus; `OnceLock` invalidation hooks already exist at
`src/llm/probe_table.rs:265, 333` so the cost is contained).

## Consequences

### Positive

- **Multi-endpoint providers are first-class.** OpenCode's three wire
  formats are three entries under the same section, not 19 inline
  tables with hand-typed URL suffixes. A fourth endpoint is a fourth
  `[[providers.opencode]]` entry — no schema change.
- **Per-model and section-level fields are structurally separated.**
  `models = ["…"]` carries only ids; knobs (`temperature`,
  `top_p`, …) sit at entry level and merge via
  `SectionKnobs::merge_first_wins`. The v0.10 "is this `endpoint`
  section-level or per-model?" question disappears.
- **Centralised `max_tokens` resolution.** One helper, one chain,
  one env-var override. The seven call-sites that rolled their own
  three-layer `min(...)` chain now consult the helper; their
  hand-rolled chains are gone. `MOAGAN_<SECTION>_MAX_TOKENS` is the
  new operator-facing knob; documented in the migration guide.
- **No public-API break for consumers.** `ProviderConfig`,
  `ModelConfig`, the dispatcher, the audit pipeline, the telemetry
  pipeline, the CLI all keep reading `cfg.providers_legacy` exactly
  as they did in v0.12.x. The bridge is invisible to 2 000+ lines of
  provider code.
- **Dual-mode loads are deterministic.** A v0.12 TOML continues to
  load, the runtime behaviour is bit-identical (modulo the new
  per-section warning), and the operator gets a `tracing::warn!` per
  legacy section reminding them to migrate before v0.15. The
  per-section `models[]` array form (`[[providers.X.models]]`) that
  some operators already used is also detected and warned — its
  classification as legacy comes from the inline-table shape of the
  model entries, not the section header.
- **`serde(flatten)` + `deny_unknown_fields` conflict resolved.** We
  drop `deny_unknown_fields` from `ProviderEntry` and rely on
  `Config::warn_unknown_provider_keys` to surface typos at load
  time. The check inspects the raw TOML after parse and warns on
  unknown section-level / entry-level keys (e.g. an operator who
  puts `api_key = "…"` in `moagan.toml` instead of `api_keys.toml`).

### Negative / accepted risks

- **+200 LOC in `src/config/mod.rs`** (new types + bridge + knobs
  merge helper) and **+580 LOC in `src/llm/max_tokens.rs`** (the
  helper + its 11 unit tests + memoisation hooks). These are net
  additions; `src/config/mod.rs` keeps both shapes in scope because
  the legacy `ProviderConfig` still exists as the bridge's output
  type.
- **`+780 LOC` in `src/config/dual_mode.rs`** (the deserializer
  visitor + 11 unit tests covering new-array, mixed-array,
  legacy-grouping, duplicate-endpoint, side-channel preservation,
  etc.). The size is justified by the test density: every edge case
  the 16 subagents flagged in §7.B is pinned by an integration or
  unit test in this commit or in PRs #1+2 / #3.
- **Breaking for v0.12 TOML syntax.** A TOML that uses the legacy
  single-table form continues to load with a `tracing::warn!` per
  section; a TOML that uses the new array-of-tables form is not
  loadable by v0.12 binaries. This asymmetry is deliberate — the
  Cargo version bump from v0.12 to v0.13 is the canonical signal to
  operators, and the v0.13 release notes spell out the migration
  window ("legacy supported through v0.13.x, removed in v0.15").
- **`MOAGAN_<SECTION>_MAX_TOKENS` is new.** Operators who used the
  legacy `MOAGAN_MINIMAX_MAX_TOKENS` (a distinct name, no longer
  wired) must rename. The migration guide spells out the rename.
- **`Config::default()` calls `compute_legacy_providers()`** so the
  default `Config` is bridge-ready without going through
  `Config::load()`. The duplicate-id error cannot fire on the
  defaults (they are duplicate-free by construction); if it ever
  did, a `tracing::error!` fires and `providers_legacy` is left
  empty rather than panicking from `Default::default()`.
- **Two views of the same data are now live** (`providers` and
  `providers_legacy`). A future contributor who mutates one without
  calling `compute_legacy_providers()` will see stale data; the
  function is `pub` and the post-load call site
  (`Config::load()`) is the documented contract.

### Compliance

| Surface | Where it lives | Enforced by |
|---|---|---|
| `ProviderEntry`, `SectionKnobs` | `src/config/mod.rs:1554-1699` | unit tests in `src/config/mod.rs::tests`, `src/config/dual_mode.rs::tests` |
| `deserialize_providers_map`, `deserialize_model_list` | `src/config/dual_mode.rs` | `cargo test --lib config::dual_mode` |
| `compute_legacy_providers()` | `src/config/mod.rs:1818-1869` | `cargo test --lib config` |
| `resolve_max_tokens()` | `src/llm/max_tokens.rs` | `cargo test --lib llm::max_tokens` |
| `Config::load()` bridge + dual-mode | `src/config/mod.rs:1896-1951` | `tests/integration_config_dual_mode.rs` (this PR) |
| `config.example.toml` | repo root | `toml::from_str` parses cleanly under `cargo test` |
| Migration guide | `docs/migrations/v0.12-to-v0.13-config.md` | manual review |
| ADR 0003 | this document | this document |
| CHANGELOG entry | `CHANGELOG.md [Unreleased]` | `make fmt-check` + manual review |
| Integration tests | `tests/integration_config_dual_mode.rs` | `make test-ci` |
| Legacy guard script (optional) | `scripts/check-no-legacy-config-schema.sh` | `scripts/gauntlet.sh` (TODO: add hook) |

## Re-evaluation

This ADR will be revisited when any of the following happen:

1. **v0.15 cycle opens and the legacy `[providers.X]` form is
   considered for removal.** The deprecation window (`v0.13.0` →
   `v0.15.0`) is two minor versions per the operator's
   "deprecate-then-remove" convention; the cutover requires a
   follow-up ADR that points at this one and votes the removal.
2. **`resolve_max_tokens` memoisation lands.** The `OnceLock` cache
   is a TIER-2 follow-up tracked in the plan §11 step 4; landing
   it requires re-validating the `invalidate_cache()` hooks in
   `src/llm/probe_table.rs` and the helper's test suite (the 11
   tests already cover the env-var / cache / fallback chains; the
   memoisation layer adds cache-hit / invalidation tests).
3. **A fifth wire format joins the existing three** (the opencode
   pool adds an OpenAI Assistants endpoint, say). The
   array-of-tables schema accommodates it natively — the ADR does
   not need amending, only the `default_providers()` defaults and
   the `wire_format_from_url` lookup in `src/llm/wire_format.rs`.
4. **`serde(flatten)` + `deny_unknown_fields` becomes officially
   compatible in a future serde release.** When that lands, we can
   add `#[serde(deny_unknown_fields)]` back on `ProviderEntry` and
   drop the post-load `warn_unknown_provider_keys` walk. The
   downgrade is mechanical and self-contained.
5. **The `deny_unknown_fields` removal causes operator-visible
   regressions.** If the post-load walk misses a typo class (e.g. a
   nested-table key the visitor can't reach), the fix is to extend
   the walk — not to add `deny_unknown_fields` back. Track via
   `gh issue` so the regression surface is auditable.

Until then, the verdicts above are authoritative.

---

## Appendix A — Alternatives considered

### A.1 — Full refactor without a bridge (rejected)

Rip out `ProviderConfig` entirely, change every consumer
(`llm::provider::registry_from_config_with_home`, `cli::doctor`,
`cli::run`, `cli::telemetry`, `MinimaxProvider::new`,
`DeepSeekProvider::new`, `OpenAICompatibleProvider::new`, the
`BreakeredProvider` wrapper, the audit pipeline's
`AuditWriter::record_call`, …) to read `Vec<ProviderEntry>` and
re-derive `wire_format` from the entry URL on every call. The
audit subagent (omicron) estimated **4 000+ lines across 40+
files**; the consensus from all 16 subagents was unanimous —
operator breakage during the migration window is unacceptable, and
the bridge pattern costs ~200 LOC for a clean upgrade path.

### A.2 — `serde(flatten)` knobs WITHOUT a bridge (rejected)

Keep the legacy single-table form, flatten `SectionKnobs` onto
`ProviderConfig`, accept both `models = ["id", …]` and
`models = [{id, …}, …]` on the same field. The `serde` flattening
mechanism does not interact well with `Vec<ModelConfig>` — you
cannot flatten a knob into a `Vec<ModelConfig>` element, only into
the outer table. The 16-subagent consensus was that this is a dead
end: the section-level knobs (`temperature`, `top_p`, …) end up
adjacent to per-model fields (`id`, `endpoint`, `max_tokens`) in
the same flat namespace, which is the original problem we are
trying to solve.

### A.3 — `OnceLock`-memoised resolver as part of PR #4 (deferred)

The `resolve_max_tokens` helper currently does 2 allocations per
call (`format!` + `to_uppercase().replace(...)`). For a 10 000-call
run that's 20 000 allocations. The memoisation helper is sketched in
`src/llm/max_tokens.rs` (deferred to v0.13.x per TIER-2
consensus — the 16 subagents agreed the memoisation belongs in a
follow-up so PR #4 stays scoped to the schema + resolver chain
shipped in v0.13.0). The invalidation hooks
(`probe_and_store` / `verify` at `src/llm/probe_table.rs:265, 333`)
already exist, so the memoisation is a localised follow-up rather
than a system-wide refactor.

### A.4 — Per-provider schema extensions (rejected)

A future-friendly alternative: per-provider TOML extensions like
`[providers.opencode.formats.chat_completions]`,
`[providers.opencode.formats.messages]`,
`[providers.opencode.formats.responses]`. This buys us a flatter
TOML for the multi-endpoint case but at the cost of cross-provider
ergonomics (every provider has a different shape, no shared
dispatcher). The `[[providers.X]]` array-of-tables form keeps
single-endpoint providers ergonomic (`[[providers.minimax]]` is one
entry) and multi-endpoint providers explicit (three entries
document themselves).