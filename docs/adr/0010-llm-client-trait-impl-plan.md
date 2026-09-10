# ADR 0010 — LLM client trait refactor implementation plan (deferred)

> **Status**: Proposed (deferred to a follow-up release)
> **Date**: 2026-09-10
> **Deciders**: `airvzxf/moagan` operator
> **Note on numbering**: the EPIC #847 issue that originated this
> ADR (#839) targeted `docs/adr/0007-llm-client-trait.md`, but
> `docs/adr/0007-monolith-default.md` already occupies slot 0007
> (landed 2026-09-08). The next free slot was 0010, used here.
> **Supersedes**: nothing.
> **Relates to**:
> [EPIC #847](https://github.com/airvzxf/moagan/issues/847)
> (the action plan this ADR formalises),
> [`src/llm/provider.rs`](../../src/llm/provider.rs) (the current
> `Provider` trait + 5 concrete impls this ADR proposes to replace),
> [`src/llm/wire.rs`](../../src/llm/wire.rs) (the current
> `Request`/`Response` types this ADR generalises),
> [EPIC #836](https://github.com/airvzxf/moagan/issues/836) +
> [ADR-0009](0009-wire-body-minimal-temperature.md) (the soft-landing
> precedent this EPIC generalises),
> [ADR-0001](0001-no-go-list-policy.md) (the differentiated
> allow-list that forbids any new LLM SDK crate).

## Context

`moagan`'s LLM layer currently centres on a `Provider` trait
(`src/llm/provider.rs:70-168`) with 5 concrete impls
(`MinimaxProvider`, `DeepSeekProvider`, `AnthropicCompatProvider`,
`OpenAICompatibleProvider`, `OpenAICompatProvider`). The
dispatcher at `src/llm/provider.rs:1423-1438` branches on the
provider name string to pick the right concrete impl; the
auto-heal cascade (`src/llm/param_rejections.rs::PARAM_NAMES`) is
hardcoded to `["temperature", "top_p", "max_tokens"]`; and the
audit-log SHA-256 computation is gated by an
`if self.default_provider == "minimax"` branch
(`src/phases/phase.rs:1528, :1986`) — the smoking-gun
anti-pattern that ties the audit hash to a provider name rather
than a wire format.

EPIC #847 (2026-09-09) was the architectural follow-up that
proposed replacing this design with a single `LlmClient` trait
+ 4 SDK impls (`AnthropicClient`, `OpenAiChatClient`,
`OpenAiResponsesClient`, `MockClient`) and a URL-driven
dispatcher. The spike investigation (8 subagents in parallel)
validated the architecture.

## Decision

EPIC #847 is **deferred to a follow-up release** (planned
v0.18.0 or later). The architectural refactor touches ~6 production
call sites in `src/phases/phase.rs`, deletes 5 concrete provider
impls, and reshapes the `Request`/`Response` API surface. The
blast radius is large enough that landing it in a single session
would be unsafe; the EPIC body itself describes it as a 5-PR
streaming rollout with RGR methodology.

This ADR records the implementation plan so a future session can
pick it up without re-deriving the design.

### Implementation plan (carried forward from EPIC #847 body)

| Order | Issue | Title | Depends on |
|---|---|---|---|
| 1 | [#839](https://github.com/airvzxf/moagan/issues/839) | ADR — author this file | — |
| 2 | [#840](https://github.com/airvzxf/moagan/issues/840) | PR #1 — generalize `param_rejections.toml` whitelist | — |
| 3 | [#841](https://github.com/airvzxf/moagan/issues/841) | PR #2 — `LlmRequest` / `LlmResponse` structs | #839 |
| 4 | [#842](https://github.com/airvzxf/moagan/issues/842) | PR #3 — `LlmClient` trait + 4 SDK impls | #839, #841, #840 |
| 5 (parallel) | [#843](https://github.com/airvzxf/moagan/issues/843) | PR #4 — migrate `phase.rs` call_* methods | #842 |
| 5 (parallel) | [#844](https://github.com/airvzxf/moagan/issues/844) | PR #5 — kill audit-hash hack | #842 |
| 6 | [#845](https://github.com/airvzxf/moagan/issues/845) | PR #6 — delete legacy providers | #843, #844 |
| 7 (parallel) | [#846](https://github.com/airvzxf/moagan/issues/846) | PR #7 — generalize auto-probe | #842, #840 |

### Why deferred (not abandoned)

- The architectural scope (8 PRs, ~6 production call sites,
  legacy provider deletion) does not fit in a single working
  session without risking CI red on the trunk.
- The auto-heal close-the-loop behaviour this EPIC closes is
  already gated by the EPIC #836 / ADR-0009 wire-body-minimal
  rollout (closed #825, #826, #827, #828 in v0.16.1). The
  remaining gap is the `if minimax` audit-hash hack and the
  per-provider dispatcher — both real but not urgent.
- EPIC #847 was designed as a streaming rollout (RGR methodology)
  with 5 PRs landing one-at-a-time. Forcing it into a single PR
  would invert the methodology and defeat its safety net.

## Consequences

### Positive

- **The architectural decision is recorded.** This ADR captures
  the URL-driven dispatcher design, the 4-SDK split, and the
  Actor A/B data-structure pattern. A future session can pick up
  at PR #2 (#840) without re-investigating.
- **EPIC #836 closes the most user-visible gap.** The
  temperature wire-body minimal rollout (#826 + #827 + #828)
  addresses the same upstream-quote-unquote auto-heal cascade
  that EPIC #847's generalisation would address. Operators see
  the most painful bug class fixed in v0.16.1; the remaining
  architectural cleanup is a code-quality improvement, not a
  user-facing fix.
- **CI remains green.** The deferral avoids a risky multi-PR
  push that could land red and require a repair cycle on the
  trunk.

### Negative

- **`if minimax` audit-hash hack remains in `src/phases/phase.rs`.**
  Operators debugging a MiniMax-specific audit hash mismatch
  will see the hardcoded branch instead of a trait-level
  abstraction. This is a code-readability cost, not a
  behaviour bug.
- **`PARAM_NAMES` hardcoded whitelist remains.**
  The auto-heal cascade still rejects any rejected wire field
  name outside the three whitelisted ones. Upstreams that
  reject a fourth field will keep paying a 400 round-trip on
  first-run instead of caching the rejection.
- **5 concrete `Provider` impls remain in the source tree.**
  The dispatcher at `src/llm/provider.rs:1423-1438` still
  branches on provider name string. A future provider addition
  requires touching the dispatcher.

## Acceptance criteria for un-deferring

EPIC #847 can be picked up in a future session when **all** of
the following hold:

- [ ] The trunk is green at v0.17.0 (or whatever minor ships next
  after the docgen + CI consolidation stack lands).
- [ ] The session has enough capacity to land at least the first
  three PRs (#840, #841, #842) in streaming mode with CI green
  between each.
- [ ] The `LlmRequest`/`LlmResponse` struct fields have been
  pinned (PR #2 must land before PR #3, and PR #2's design is
  what determines whether PR #3 needs to add fields).

## References

- [EPIC #847](https://github.com/airvzxf/moagan/issues/847) —
  the action plan this ADR formalises.
- [EPIC #836](https://github.com/airvzxf/moagan/issues/836) +
  [ADR-0009](0009-wire-body-minimal-temperature.md) — the
  soft-landing precedent this EPIC generalises.
- [Spike #733](https://github.com/airvzxf/moagan/issues/733)
  (closed) — the original temperature wire-body investigation
  that motivated both EPICs.
- [PR #332](https://github.com/airvzxf/moagan/pull/332) —
  `MOAGAN_<NAME>_OMIT_MAX_TOKENS` (the soft-landing precedent).
- [PR #585](https://github.com/airvzxf/moagan/pull/585) —
  `temperature_auto` probe (auto-heal companion).
- [ADR-0001](0001-no-go-list-policy.md) — the differentiated
  allow-list that forbids any new LLM SDK crate.
