# JSON recovery strategy — per-model enum

> **Status**: shipped in PR #360. The parse chain in
> `src/llm/json_extractor.rs` and the dispatcher retry loop in
> `src/phases/phase.rs::call_with_retry_parse` both consult the
> per-model table to decide which recovery strategy to apply.

## Why this exists

The JSON-output audit surfaced four distinct recovery paths the
provider roster needs. Each one is a different *response-side*
decision; today every call goes through the same chain (Lenient,
with continuation + retry budget). After this PR the dispatcher
picks the strategy from the model name so each path can be
calibrated independently.

The four strategies:

| Variant            | Parse chain                     | Retry on failure                              |
|--------------------|---------------------------------|-----------------------------------------------|
| `Strict`           | direct `serde_json::from_str`   | none                                          |
| `Lenient`          | full chain (Path B + m3 repair) | none (one attempt per call)                   |
| `Continuation`     | full chain                      | up to 2 `Role::Continuation` re-calls         |
| `PromptPrefill`    | full chain                      | one-shot assistant prefill `{` retry          |

## Per-model default table

```rust
pub const STRATEGY_BY_MODEL: &[(&str, JsonRecoveryStrategy)] = &[
    // kimi — chat-template leaks + Llama bracket variants
    ("kimi-k2.7-code", JsonRecoveryStrategy::Lenient),
    ("kimi-k3",        JsonRecoveryStrategy::Lenient),
    ("kimi-k2.6",      JsonRecoveryStrategy::Lenient),
    // glm — opencode_go chat-completions
    ("glm-5.1",        JsonRecoveryStrategy::Lenient),
    ("glm-5.2",        JsonRecoveryStrategy::Lenient),
    // hy3 — opencode_go
    ("hy3",            JsonRecoveryStrategy::Lenient),
    // mimo — chat-completions
    ("mimo-v2.5",      JsonRecoveryStrategy::Lenient),
    ("mimo-v2.5-pro",  JsonRecoveryStrategy::Lenient),
    // deepseek — ignores response_format; needs a nudge
    ("deepseek-v4-pro",   JsonRecoveryStrategy::PromptPrefill),
    ("deepseek-v4-flash", JsonRecoveryStrategy::PromptPrefill),
    // minimax — Anthropic-compat, occasionally truncates
    ("minimax-m3",    JsonRecoveryStrategy::Continuation),
    ("minimax-m2.7",  JsonRecoveryStrategy::Continuation),
    ("minimax-m2.5",  JsonRecoveryStrategy::Continuation),
    // gpt-5.6-luna — Responses API, strict JSON mode
    ("gpt-5.6-luna",  JsonRecoveryStrategy::Strict),
];
```

Unrecognised models fall through to `DEFAULT_STRATEGY = Lenient`.

## How the dispatcher uses the table

`phases/phase.rs::call_with_retry_parse` resolves the strategy
once per call:

```rust
let strategy = json_strategy::strategy_for(&self.default_model, None);
```

then:

- passes it to `parse_with_strategy(strategy, model, request, raw)`
  which routes to the matching parse chain (Strict vs Lenient).
- when `strategy == PromptPrefill`, fires a one-shot
  assistant-prefill retry via `Request::extra_messages =
  [{role:"assistant", content:"{"}]` after the first parse
  failure. The retry uses `call_uncached` so the prefill
  response never poisons the steady-state cache.
- when `strategy == Continuation`, defers to the existing
  PR-C2 truncated-response re-call (cap: 2 attempts) wired by
  the same helper.

## How the OpenAI-compat body builder uses the table

`llm/openai_compatible.rs::build_chat_request` consults
`json_strategy::strategy_for(&self.model, None)` and, when the
strategy is `PromptPrefill`, appends an assistant prefill of `{`
after the user turn so the model sees
`[system, user, assistant:]` and continues with a JSON object
body. The wire shape is independent of the
`Request::extra_messages` field; the body builder pushes both
caller-supplied extra messages AND the per-model default prefill.

The Anthropic-compat (`opencode_go_anthropic`) and Responses API
(`opencode_go_responses`) providers do NOT honour prefill; the
`PromptPrefill` strategy on those paths is effectively a no-op
on the request side and the dispatcher falls back to the normal
parse-failure budget.

## Boundary with `response_format_opt_out`

`response_format_opt_out` is a *request-side* decision (omit
`response_format: json_object` from the wire body). The strategy
enum is a *parse-side* decision (what to do with a malformed
response). They overlap in the model list but operate on
independent mechanisms. The strategy enum does NOT consult the
opt-out list, and `response_format_opt_out` does NOT consult
the strategy table.

## Cache-key contract

`Request::extra_messages` is part of the wire shape but
**deliberately ignored** by `wire::build_cache_key`. Rationale:
the prefill is a response-side hint that nudges the model into
starting with `{`; the request *identity* (model + system +
user + max_tokens + temperature + top_p) is what defines a cache
key. The prefill retry uses `call_uncached` so a prefill-induced
response never enters the cache in the first place.

Tests in `src/llm/wire.rs::tests::cache_key_ignores_extra_messages`
pin this contract.

## Tests

- 20 unit tests in `src/llm/json_strategy.rs::tests` covering
  the per-model table, lookup semantics, profile overrides,
  case sensitivity, and the two helpers.
- 4 integration tests in `src/phases/phase.rs::tests` covering
  the retry-loop behaviour for each strategy.
- 9 wrapper tests in `src/llm/json_extractor.rs::tests`
  covering `parse_with_strategy` for each variant.
- 3 prefill-injection tests in `src/llm/openai_compatible.rs::tests`
  covering the wire shape on PromptPrefill, non-prefill models,
  and caller-supplied `extra_messages`.
- 3 cache-key tests in `src/llm/wire.rs::tests` covering the
  round-trip and the `extra_messages` invariant.

Total: ~39 tests for this feature.

## Future work

- A `--strategy` CLI flag that consults `profile_overrides` to
  pin a specific strategy for a model without shipping a new
  binary. The `strategy_for` signature already accepts
  `Option<&HashMap<String, JsonRecoveryStrategy>>` so the
  wiring is in place.
- A `MOAGAN_STRATEGY_OVERRIDES` env var (mirroring
  `MOAGAN_RESPONSE_FORMAT_OPT_OUT`) for runtime override
  without rebuilding.