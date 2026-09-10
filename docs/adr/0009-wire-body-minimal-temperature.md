# ADR 0009 — Wire-body minimal for `temperature` (Ruta A soft landing)

> **Status**: Accepted
> **Date**: 2026-09-10
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: nothing (additive to the existing wire-format
> serialisation in `src/llm/wire_format.rs` and
> `src/llm/wire.rs`).
> **Note on numbering**: the EPIC #836 issue that originated this
> ADR (#825) targeted `docs/adr/0006-wire-body-minimal-temperature.md`,
> but `docs/adr/0006-discover-test-structural-validation.md` already
> occupies slot 0006 (landed 2026-09-08, ADR-0006 is the
> `integration_discover_minimax` structural-validation test design).
> The next free slot was 0009, used here.
> **Relates to**:
> [`src/llm/wire.rs:30`](../../src/llm/wire.rs)
> (`Request::temperature`),
> [`src/llm/wire_format.rs:175-225`](../../src/llm/wire_format.rs)
> (`ResponsesWire` and `ResponsesWireBody`),
> [`src/phases/phase.rs:3398-3440`](../../src/phases/phase.rs)
> (`resolve_temperature` and `should_omit_default_temperature`),
> [EPIC #836](https://github.com/airvzxf/moagan/issues/836)
> (the rollout tracker),
> [spike #733](https://github.com/airvzxf/moagan/issues/733)
> (closed — investigation + 9 exit criteria),
> [PR #332](https://github.com/airvzxf/moagan/pull/332)
> (`MOAGAN_<NAME>_OMIT_MAX_TOKENS` — the soft-landing precedent),
> [PR #585](https://github.com/airvzxf/moagan/pull/585)
> (`temperature_auto` probe — auto-heal companion),
> [ADR-0001](../../docs/adr/0001-no-go-list-policy.md)
> (the differentiated allow-list that forbids any crate-level
> workaround).

## Context

Until v0.16.0, `moagan` stamped a `temperature` field on every
LLM wire body even when the operator had not configured one.
Two on-the-wire symptoms came from this:

1. `ResponsesWire::encode_body` (`src/llm/wire_format.rs:190-211`)
   used a `serde_json::json!({...})` builder that serialised
   `None` as `"temperature": null`. A handful of upstreams
   (`gpt-5.6-luna` on the OpenCode Responses path) reject the
   literal `null` payload with HTTP 400, forcing the auto-heal
   loop into a wasted round-trip on every first-run.
2. `Request::temperature` (`src/llm/wire.rs:30`) had no
   `#[serde(skip_serializing_if = "Option::is_none")]` attribute,
   so the absence-of-value contract could not be expressed at
   the type level — every wire builder had to remember the
   `if let Some(...) = req.temperature { ... } else { ... }`
   dance by hand.

Spike #733 (now closed) investigated the gap and produced three
architectural decisions documented in the EPIC:

- **Ruta A**: `resolve_temperature()` returns `Option<f32>`; the
  wire body omits `temperature` by default unless the operator
  pins a value.
- **Wire-builder fix (option a)**: `ResponsesWire::encode_body`
  is rewritten with a typed `ResponsesWireBody<'a>` struct
  mirroring `ResponsesRequest` (`src/llm/openai_compat.rs:271-299`).
- **Migration (option ii)**: soft landing via
  `MOAGAN_<NAME>_OMIT_DEFAULT_TEMPERATURE` env var, mirroring the
  `MOAGAN_<NAME>_OMIT_MAX_TOKENS` precedent from PR #332
  (`src/config/mod.rs:2794-2839`). Default off; default flips in
  the following minor.

This ADR formalises the resulting wire-body contract.

## Decision

### 1. `Request::temperature` is `skip_serializing_if = "Option::is_none"`

`src/llm/wire.rs:30` now carries:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub temperature: Option<f32>,
```

`Option::is_none` is the canonical serde idiom. Every wire format
that serialises a `Request` therefore omits `temperature` when it
is `None`, with zero per-builder code.

### 2. `ResponsesWire::encode_body` uses a typed struct

`src/llm/wire_format.rs` introduces:

```rust
#[derive(Debug, Serialize)]
struct ResponsesWireBody<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
}
```

The struct replaces the previous `serde_json::json!({...})`
builder. Every field except the always-present ones carries
`skip_serializing_if = "Option::is_none"`, so unset parameters
are absent on the wire byte-for-byte.

### 3. `resolve_temperature` returns `Option<f32>` and reads the env var

`src/phases/phase.rs:3398` now has the signature:

```rust
pub fn resolve_temperature(
    role: Role,
    profile_overrides: Option<&std::collections::HashMap<String, f32>>,
    provider_base: Option<f32>,
    section: &str,
) -> Option<f32>
```

The precedence is:

1. Profile-defined override for `role` (when present).
2. `provider_base` (when `Some`) — the per-provider default from
   `[providers.<name>].temperature` in the user's TOML.
3. `should_omit_default_temperature(section)` — when the env var
   `MOAGAN_<NAME>_OMIT_DEFAULT_TEMPERATURE` is set to a truthy
   value, returns `None` (omit the field on the wire).
4. The hard-coded per-role default from `temperature_for_role`.

Operators who pin a value through (1) or (2) always win. The
opt-out only suppresses the implicit fallback in (4). Default
behaviour is unchanged — operators see the same temperatures
they did before the rollout.

### 4. `should_omit_default_temperature` reads the env var

`src/phases/phase.rs:3431-3440`:

```rust
pub(crate) fn should_omit_default_temperature(section: &str) -> bool {
    let key = format!(
        "MOAGAN_{}_OMIT_DEFAULT_TEMPERATURE",
        section.to_uppercase().replace(['.', '-'], "_")
    );
    match std::env::var(&key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        Err(_) => false,
    }
}
```

Same mangling rule (`uppercase`, `.` and `-` rewritten to `_`)
as `MOAGAN_<NAME>_OMIT_MAX_TOKENS`. Truthy set is `true` / `1`
/ `yes` / `on`; anything else is a no-op so a typo does not
silently flip the flag.

### 5. Default flips in the following minor

The env var's default behaviour is "off" in v0.16.0. The default
flips to "on" in the following minor (planned v0.17.0). Operators
who want to pin temperatures via TOML still do — only the implicit
per-role defaults are affected.

## Consequences

### Positive

- **Wire body is minimal by construction.** Unset temperatures
  are absent from the wire; the upstream never sees
  `"temperature": null`. The auto-heal cascade stops wasting a
  round-trip on first-run against upstreams that reject the
  literal `null`.
- **Type-driven contract.** The wire-body contract is enforced
  by the `#[serde(skip_serializing_if)]` attribute on the
  `Request` field, not by per-builder `if let Some(...)` boilerplate.
  A future wire format added under EPIC #847's `LlmClient` refactor
  inherits the contract for free.
- **No new dependencies.** `serde_json::json!` removal is a net
  improvement (the typed struct has the same `serde::Serialize`
  derive as the rest of the wire types).
- **Soft landing for operators.** The env-var opt-out preserves
  v0.15.x behaviour for the rollout minor; only the v0.17.0
  default flip is observable.
- **Symmetry with `MOAGAN_<NAME>_OMIT_MAX_TOKENS`.** Operators
  who learned the pattern for `max_tokens` see the same shape
  here — same env-var prefix, same truthy set, same section-name
  mangling.

### Negative

- **New env var surface.** `MOAGAN_<NAME>_OMIT_DEFAULT_TEMPERATURE`
  joins the existing `MOAGAN_<NAME>_OMIT_MAX_TOKENS` family.
  Operators who script around the env vars need to know about
  the new one. Documented in CHANGELOG.
- **Two semantically related env vars.** A future operator
  confused by the difference between "omit max_tokens" and
  "omit default temperature" could mis-configure. Mitigated by
  the explicit naming and by the doc-comment on
  `should_omit_default_temperature`.
- **The default flip in v0.17.0 is a behaviour change.** Operators
  relying on implicit per-role defaults will see `temperature`
  absent from wire bodies; any upstream that defaults to
  non-zero temperature when the field is missing will produce
  different samples. This is the desired outcome (it closes the
  auto-heal loop on first-run), but it is observable.

### Mitigations

- The 4 `temperature_gate_*` tests at
  `src/phases/phase.rs:4409-4940` continue to pass — the
  precedence contract is pinned by 5 unit tests at
  `src/phases/phase.rs:4267-4351`.
- The proptest coverage at `src/llm/wire.rs:333-776` exercises
  the wire-body serialisation for all three wire shapes
  (`AnthropicWire`, `OpenAiWire`, `ResponsesWire`).
- The CHANGELOG entry for v0.16.0 documents the env var and the
  v0.17.0 default flip.

## Alternatives considered

### A. Always omit temperature from the wire (no env var)

Rejected. The hard landing — drop the field from every wire body
in v0.16.0 — would silently change the sampling temperatures on
every existing run. The spike #733 investigation surfaced this as
the highest-risk option; the v0.12.x `max_tokens` rollout went
through the same soft-landing pattern (PR #332) before flipping
the default, and the spike explicitly recommends mirroring that
precedent here.

### B. Add a `wire_minimal` config flag instead of an env var

Rejected. Config flags load at startup; env vars can be toggled
per-process (e.g., in CI per-job). The auto-heal cascade is a
runtime concern and reads the env var each call (cheap;
`std::env::var` is a single `getenv`), so per-call freshness is
preserved without the cost of a startup-only config knob.

### C. Keep the `serde_json::json!` builder, fix the `null` bug inline

Rejected. The typed struct is the only way to express
`skip_serializing_if` on a per-field basis; the builder does
not support per-field attributes. The fix would have been
`value.as_object_mut().unwrap().remove("temperature")` when
`req.temperature.is_none()` — a runtime check that would need to
be repeated in every future wire format. The typed struct is the
mechanical enforcement.

## References

- [EPIC #836](https://github.com/airvzxf/moagan/issues/836)
  (temperature wire-body minimal — the rollout tracker).
- [Spike #733](https://github.com/airvzxf/moagan/issues/733)
  (closed — investigation + 9 exit criteria + the three
  sub-decisions this ADR formalises).
- [PR #332](https://github.com/airvzxf/moagan/pull/332)
  (`MOAGAN_<NAME>_OMIT_MAX_TOKENS` — the soft-landing precedent).
- [PR #585](https://github.com/airvzxf/moagan/pull/585)
  (`temperature_auto` probe — auto-heal companion, closes #593).
- [PR #605](https://github.com/airvzxf/moagan/pull/605)
  (`max_tokens → Option<u32>` — type-driven refactor precedent).
- `src/llm/wire_format.rs:175-225` — `ResponsesWire` and
  `ResponsesWireBody`.
- `src/phases/phase.rs:3398-3440` — `resolve_temperature` and
  `should_omit_default_temperature`.
- `src/llm/wire.rs:30` — `Request::temperature` field.
- `AGENTS.md` §"No-go list" — the policy that forbids any
  crate-level workaround.
