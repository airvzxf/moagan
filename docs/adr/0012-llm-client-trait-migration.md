# ADR 0012 — LLM client trait migration (post-#933 surface)

> **Status**: Accepted (decisions and rollout recorded 2026-09-12/14)
> **Date**: 2026-09-13
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: [ADR-0010](0010-llm-client-trait-impl-plan.md) (the
> deferred implementation plan that landed in v0.17.0 and was carried
> forward into v0.18.0).
> **Superseded by**: nothing.
> **Relates to**:
> [EPIC #847](https://github.com/airvzxf/moagan/issues/847) (the umbrella),
> [Issue #900](https://github.com/airvzxf/moagan/issues/900) (the D1–D11
> architectural decisions this ADR formalises),
> [ADR-0009](0009-wire-body-minimal-temperature.md) (the soft-landing
> precedent the new no-compat stance supersedes),
> [ADR-0001](0001-no-go-list-policy.md) (the no-go list that forbids
> new LLM SDK crates — D1 stays inside this constraint),
> [ADR-0011](0011-no-cancel-in-progress.md) (the "merge forward, not
> revert" principle the rollout adopted).

## Context

`moagan`'s pre-v0.18 LLM layer centred on a `Provider` trait
(`src/llm/provider.rs`) with five concrete impls
(`MinimaxProvider`, `DeepSeekProvider`, `AnthropicCompatProvider`,
`OpenAICompatibleProvider`, `OpenAICompatProvider`) selected by a
provider-name-string dispatcher. Three anti-patterns had accumulated:

1. **Per-provider name-string dispatch.** Adding a provider required
   touching a centralised dispatcher keyed on the section name.
2. **`if default_provider == "minimax"` audit-hash branch** in
   `src/phases/phase.rs:1528, :1986` — the smoking-gun hack that
   pinned the audit hash to a provider name rather than a wire
   format.
3. **Four duplicated cascade loops** in `phase.rs` (`:1496-1527`,
   `:1607-1672`, `:1968-1985`, `:2011-2057`) that the dispatcher
   re-implemented per call site instead of letting the SDK own it.

EPIC #847 (2026-09-09) proposed replacing the design with a single
`LlmClient` trait + a URL-driven dispatcher. A 4-hour review session
on 2026-09-10/11 with 18 subagents in parallel produced a
consolidated decision set (issue #900 — D1–D11) that resolved the
trade-off in favour of **"simplicity over flexibility, breaking
changes over backward compatibility"**. The migration landed across
16 issues (#919–#934) in v0.18.0.

## Decision

EPIC #847 ships in v0.18.0 as a single coordinated release. The
three legacy anti-patterns above are gone. The post-#933 runtime
exposes a single SDK surface — `LlmClient` + the three SDK impls
(`AnthropicClient`, `OpenAIClient`, `MockClient`) — and a
URL-path dispatcher that picks the SDK from the endpoint URL's
path suffix.

This ADR records the 11 decisions from #900 + the 16-issue rollout
that implemented them, so a future agent or operator can read one
file instead of tracking 16+ GitHub threads.

### The 11 decisions

#### D1 — Three SDKs total, no custom anything

The `LlmClient` trait has **exactly 3 SDK impls**:

| SDK impl           | URL path trigger        | `sdk_type()`              | Purpose                                                |
|--------------------|-------------------------|---------------------------|--------------------------------------------------------|
| `AnthropicClient`  | `/v1/messages`          | `"anthropic"`             | Anthropic Messages API + Anthropic-compatible endpoints |
| `OpenAIClient`     | `/v1/chat/completions`  | `"openai_compatible"`     | OpenAI Chat Completions                                 |
| `OpenAIClient`     | `/v1/responses`         | `"openai"`                | OpenAI Responses API                                    |
| `MockClient`       | `mock://...`            | `"mock"`                  | Test doubles                                            |

One impl handles both OpenAI URL variants per the principle of
"no custom anything" — the chat/responses delta is a body-shape
detail, not an SDK-identity difference.

**Removed permanently**: `CustomClient`, `CustomWire`, the
5 concrete `Provider` impls, and `BreakeredProvider` (replaced by
the `BreakeredClient` wrapper).

#### D2 — Dispatcher by URL path

`src/llm/client/dispatcher.rs::pick_sdk` matches the endpoint
URL's path suffix:

```
/v1/messages           → AnthropicClient
/v1/chat/completions   → OpenAIClient (chat variant)
/v1/responses          → OpenAIClient (responses variant)
mock://...             → MockClient
```

The operator configures only the URL in
`[providers.<name>].endpoint`; the dispatcher picks the SDK. **No**
`sdk = "..."` knob in `config.toml`.

#### D3 — No backward compatibility, no soft-landing

`moagan` is alpha (no external users, no stars, no issue
traffic). Breaking changes are explicitly allowed:

- No `Request`/`Response` type aliases after the migration.
- No `pub use crate::llm::legacy::*` compat shims.
- No `MOAGAN_<NAME>_OMIT_<OLD>` env vars for transition.
- Old config keys simply stop working; the operator edits
  `config.toml` once.

Tests, files, dependencies — touched freely. The 12 integration
tests that reference `Provider` get migrated in the same release
as the legacy deletion.

#### D4 — `max_tokens` always at the per-provider hard cap

Every call sends `max_tokens` set to the **per-provider hard
cap** (e.g. `MINIMAX_MAX_TOKENS_CAP = 524_288`) or to whatever
value the operator explicitly configured. **No conditional logic**
that reduces `max_tokens` based on the question type, the role,
or any other heuristic.

Precedence (per `src/llm/max_tokens.rs`):

1. Operator env var `MOAGAN_<NAME>_MAX_TOKENS`.
2. Operator TOML `max_tokens = ...` in `[providers.<name>]`.
3. Cached probe value in `<MOAGAN_HOME>/max_tokens_auto.toml`.
4. `OperatorCap` from `ProviderConfig::max_token_auto`.
5. `KindHardCap` (per-provider hard cap).
6. `DEFAULT_MAX_TOKENS` (last-resort).

#### D5 — Config and env var together (always)

Every knob in `config.toml` has a `MOAGAN_*` env var override and
vice versa. Precedence (highest first): env var → per-provider
TOML → global `[defaults]` TOML → hard-coded default. Lists
accept CSV in env, arrays in TOML.

#### D6 — No defaults that hide errors

A missing `provider` or `model` fails loudly at startup with
`Error::MissingConfig` — it does NOT fall back to `minimax` or any
other default. `Auto-heal` is reserved for upstream-rejected
sampling parameters (`temperature`, `max_tokens`) **during
long-running batches** (e.g. 8-hour discovery mode), never to
substitute a missing or invalid operator config.

#### D7 — `_auto` suffix nomenclature (two-plane)

The codebase has two naming planes; the `_auto` suffix marks
**mechanism** (probe + persistence + opt-out), not field:

| Plane                | Suffix     | Examples                                            |
|----------------------|------------|-----------------------------------------------------|
| Wire field           | _none_     | `temperature`, `max_tokens`, `top_p`, `top_k`      |
| CLI verb             | _none_     | `moagan probe temperature`, `moagan probe top_p`    |
| `probe_kind` log key | _none_     | `"temperature"`, `"top_p"`                          |
| Sidecar file         | `_auto`    | `temperatures_auto.toml`, `top_p_auto.toml`         |
| Env var              | `_AUTO`    | `MOAGAN_TEMPERATURE_AUTO`, `MOAGAN_TOP_P_AUTO`      |
| Config key           | `_auto`    | `temperature_auto_enabled`, `top_p_auto_enabled`    |

#### D8 — No compat layer; `BreakeredProvider` deleted

`Provider::effective_max_tokens` is **not** kept as a compat shim.
`LlmClient::body_sha256` is the single source of truth for the
audit hash. The migration deletes `BreakeredProvider` in the same
release as the legacy `Provider` impls.

#### D9 — Cascade preflight + retry absorbed in `send`

The four duplicated cascade loops in
`src/phases/phase.rs:1496-1527, :1607-1672, :1968-1985,
:2011-2057` are absorbed into `LlmClient::send`. Callers (the six
`call_*` methods) construct an `LlmRequest` and call `send`; the
retry / preflight is the SDK's responsibility.

`continue_truncated_response` uses the same `resolve_temperature`
as the other paths — no `temperature_for_role(Some(...))`
special-case.

#### D10 — Whitelist = fixed safe set, not user-configurable

The auto-heal whitelist is a fixed safe set (`temperature`,
`top_p`, `max_tokens`) defined in code, not user-editable. The
CSV format `MOAGAN_OPEN_ALLOWLIST=temperature,top_k,frequency_penalty`
is **not** exposed. Adding a new parameter requires a code change
to the constant.

#### D11 — Spikes have no due date

The `spike` GitHub label does NOT carry a due date. Spikes are
tracked until the operator decides to close or convert them into
work. The previous behaviour (spike expires after 2 weeks) is
removed.

### The 16-issue rollout (v0.18.0)

The implementation landed in four waves. Every issue is merged to
`main` and CI-green before the next one starts. The order is
load-bearing — earlier waves expose the trait surface that later
waves migrate the call sites onto.

#### Wave 1 — Trait + SDK impls + dispatcher

| Issue | PR | Title |
|---|---|---|
| [#919](https://github.com/airvzxf/moagan/issues/919) | #935 | feat(llm): introduce `LlmClient` trait + `LlmRequest` / `LlmResponse` + `MockClient` |
| [#920](https://github.com/airvzxf/moagan/issues/920) | #936 | feat(llm): introduce `AnthropicClient` SDK impl |
| [#921](https://github.com/airvzxf/moagan/issues/921) | #937 | feat(llm): introduce `OpenAIClient` SDK impl (handles `/chat/completions` + `/responses`) |
| [#922](https://github.com/airvzxf/moagan/issues/922) | #938 | feat(llm): URL-path dispatcher (D2) |

#### Wave 2 — Call-site migration (12 files in `src/phases/phase.rs` and the probe subsystem)

| Issue | PR | Title |
|---|---|---|
| [#923](https://github.com/airvzxf/moagan/issues/923) | #939 | refactor(llm): migrate `phase.rs` default-pair dispatch to `LlmClient` |
| [#924](https://github.com/airvzxf/moagan/issues/924) | #942 | refactor(llm): migrate `phase.rs` explicit-pair dispatch to `LlmClient` |
| [#925](https://github.com/airvzxf/moagan/issues/925) | #943 | refactor(llm): migrate probe subsystem to `LlmClient` |
| [#926](https://github.com/airvzxf/moagan/issues/926) | #944 | refactor(llm): migrate CLI to `LlmClient` |
| [#927](https://github.com/airvzxf/moagan/issues/927) | #945 | refactor(llm): migrate remaining 12 phase files to `LlmClient` |
| [#928](https://github.com/airvzxf/moagan/issues/928) | #946 | refactor(llm): migrate 4 discovery files to `LlmClient` |
| [#929](https://github.com/airvzxf/moagan/issues/929) | #947 | refactor(llm): migrate integration tests to `LlmClient` |

#### Wave 3 — Cascade absorption + audit-hash fix + top_p / top_k surface

| Issue | PR | Title |
|---|---|---|
| [#930](https://github.com/airvzxf/moagan/issues/930) | #948 | feat(llm): add `top_p_auto` + `top_k_auto` probe + table + sidecar (D7) |
| [#931](https://github.com/airvzxf/moagan/issues/931) | #949 | feat(llm): add CLI subcommands `moagan probe top_p` + `moagan probe top_k` (D7) |
| [#932](https://github.com/airvzxf/moagan/issues/932) | #950 | refactor(llm): absorb cascade loops into `LlmClient::send` (D9) + kill audit-hash hack (D8) |

#### Wave 4 — Legacy deletion + docs cleanup

| Issue | PR | Title |
|---|---|---|
| [#933](https://github.com/airvzxf/moagan/issues/933) | #951 | refactor(llm)!: delete legacy `Provider` trait + 5 impls + `BreakeredProvider` + `ProviderRegistry` + `wire.rs` + `wire_format.rs` (BREAKING v0.18.0) |
| [#934](https://github.com/airvzxf/moagan/issues/934) | _this issue_ | chore(docs): update `config.example.toml` + `cli-reference` + `events-reference` + `AGENTS.md` + `CHANGELOG` + new ADR-0012 + close EPIC #847 |

### Why "superset" of ADR-0010 (not "replace")

ADR-0010 (the deferred plan) was the design that EPIC #847 was
*originally* going to ship. That design was deferred to "v0.18.0
or later" because the architectural scope (8 PRs, ~6 production
call sites, legacy provider deletion) was too large to land in a
single working session.

When the migration actually landed in v0.18.0, the architectural
shape changed in two ways that ADR-0010's deferred plan did not
predict:

- The 4-SDK split (`AnthropicClient` / `OpenAiChatClient` /
  `OpenAiResponsesClient` / `MockClient`) collapsed to 3 impls
  with `OpenAIClient` covering both URL variants (#900 D1).
- A complete no-backward-compat stance replaced the soft-landing
  precedent from ADR-0009 (#900 D3).

ADR-0012 supersedes ADR-0010 because the latter's design is no
longer the system that ships. The deferred plan's contribution
to the migration — the trait shape, the URL-driven dispatcher,
the cascade absorption — is preserved here in §D1, §D2, §D9.

## Consequences

### Positive

- **One architectural source of truth.** This ADR + issue #900
  replace what would otherwise be 7 disconnected sub-issues and
  16 separate PR descriptions.
- **The principle "simplicity over flexibility, breaking changes
  over compatibility" is codified** (D3). The next agent does not
  have to re-litigate it.
- **Mock and real providers follow the same `max_tokens = max`
  rule** (D4) — no special cases.
- **Two-plane naming** (D7) eliminates ambiguity between wire
  field and mechanism.
- **Cascade absorption into `send`** (D9) collapses ~70 lines of
  cascade logic in `phase.rs` into a single
  `client.send(&req).await` call.
- **The audit hash comes from `body_sha256`** (D8), removing the
  `if default_provider == "minimax"` branch in
  `src/phases/phase.rs:1528, :1986`.
- **`src/llm/` shrank from 39 files to 28** (eleven files left).
  The directory is now focused on the new trait surface plus the
  shared HTTP / rate-limiter / circuit-breaker plumbing.

### Negative

- **Operators with existing configs need to edit `config.toml`
  once** after the migration lands (D3).
- **Removing the CSV env var for the whitelist** (D10) means a
  future "I want to add `frequency_penalty` to the whitelist"
  requires a code change, not just an env var.
- **Removing spike due dates** (D11) means a spike might rot if
  not revisited; the operator must self-discipline.
- **The pre-#933 name aliases `Request` / `Response` /
  `MockProvider` / `BreakeredClient`-wrapping-Provider
  bridge** were all deprecated-then-removed; downstream code
  that depended on them breaks.

## Acceptance criteria for "migration done"

EPIC #847 is closed when **all** of the following hold on the
v0.18.0 trunk commit:

- [x] All 5 concrete `Provider` impls deleted from `src/`.
- [x] Dispatcher in `src/llm/client/dispatcher.rs` matches by URL
      path per D2.
- [x] Every LLM call sets `max_tokens` per the precedence in D4
      (no conditional reductions).
- [x] All ~12 existing integration tests migrated to use
      `LlmClient` (no `Provider` references).
- [x] New integration tests cover each of the 3 SDK impls
      (`AnthropicClient`, `OpenAIClient` both variants,
      `MockClient`).
- [x] `BreakeredProvider` deleted; `BreakeredClient` is the
      single circuit-breaker wrapper (D8).
- [x] The 4 cascade loops in `phase.rs` absorbed into
      `LlmClient::send` (D9).
- [x] `make fmt fmt-check guard-deps lint build test-ci` green.
- [x] `cargo build --release --all-features` clean (no dead-code
      warnings).
- [x] `docs/cli-reference.md`, `docs/events-reference.md`,
      `docs/test-skips-report.md` regenerated via
      `moagan-docgen` (EPIC #852 tool).
- [x] `config.example.toml`, `AGENTS.md`, `CHANGELOG.md` updated
      to reference `LlmClient`.
- [x] This ADR exists, status **Accepted**, linked from CHANGELOG
      + AGENTS.md.

## Open items (carry forward)

- **JSON-truncation recovery strategies** (spike #901, no due
  date per D11). The spike investigates Claude Code leaked
  source, Anthropic SDK official, OpenAI SDK official, and Rust
  crates for JSON repair (`jsonrepair`, `partial-json`, etc.).
- **Native async fns in trait.** The `LlmClient` trait uses
  `async_trait` to match the legacy `Provider` surface. Switching
  to native async fns in trait is deferred to a later Rust
  toolchain bump.

## References

- [EPIC #847](https://github.com/airvzxf/moagan/issues/847) —
  the umbrella closed by v0.18.0.
- [Issue #900](https://github.com/airvzxf/moagan/issues/900) —
  D1–D11 architectural decisions this ADR formalises.
- [ADR-0010](0010-llm-client-trait-impl-plan.md) — the deferred
  plan this ADR supersedes.
- [ADR-0009](0009-wire-body-minimal-temperature.md) — the
  soft-landing precedent the no-compat stance supersedes.
- [ADR-0001](0001-no-go-list-policy.md) — the no-go list that
  forbids new LLM SDK crates (D1 stays inside this constraint).
- [ADR-0011](0011-no-cancel-in-progress.md) — the
  "merge forward, not revert" principle the rollout adopted.
- [PR #935](https://github.com/airvzxf/moagan/pull/935) –
  [#951](https://github.com/airvzxf/moagan/pull/951) — the 15
  PRs that land the migration.
- [`src/llm/client/mod.rs`](../../src/llm/client/mod.rs) — the
  canonical `LlmClient` trait surface.
- [`src/llm/client/dispatcher.rs`](../../src/llm/client/dispatcher.rs)
  — the URL-path dispatcher.
- [CHANGELOG v0.18.0](../../CHANGELOG.md) — the release notes
  this ADR links to.
