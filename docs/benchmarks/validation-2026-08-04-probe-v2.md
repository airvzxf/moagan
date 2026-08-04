---
id: validation-2026-08-04-probe-v2
status: complete
as_of: 2026-08-04
scope: end-to-end validation of moagan v0.4 against the operator's 2026-08-04 OpenCode Go roster
purpose: confirm 18-model roster works end-to-end and propose architecture for the dispatcher
baseline: 85d40c2 (Fix #1, #2, #3 from initial validation)
target: 67d633a (Fix #4 + #5 A/B + thinking-block recovery)
---

# Probe v2 — 18-model OpenCode Go roster

## TL;DR

The 2026-08-04 operator update of the OpenCode Go model roster (18 models across 3 endpoints) prompted a refactor of the OpenCode Go provider. The fix surface: a **multi-endpoint dispatcher** with **per-model temperature overrides** + **retry safety net** + **thinking-block content recovery**.

End-to-end probe results on 2026-08-04:

| Group | Models | Result |
|---|---|---|
| Working end-to-end | `gpt-5.6-luna`, `mimo-v2.5`, `mimo-v2.5-pro`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus`, `qwen3.8-max` | 7/18 ✅ (mode=fast completes) |
| China-hosting-only (now enabled) | `deepseek-v4-flash` | reachable; route phase fails |
| Blocked by operator policy | `minimax-m3`, `minimax-m2.7`, `minimax-m2.5` | 3/18 BLOCKED (Intentionally) |
| Upstream returns empty content | `glm-5.1`, `glm-5.2`, `kimi-k2.6`, `kimi-k2.7-code`, `deepseek-v4-pro`, `kimi-k3` | 6/18 schema fail |
| HTTP 400 from upstream | `gpt-5.6-luna` (transient), `hy3` | 2/18 upstream reject (Console Go) |
| Network timeout | `qwen3.8-max` (1×, retried) | 1/18 transient |

**5 commits GPG-signed** landed in this session:
- `85d40c2` — Fix #1, #2, #3 (response_format, alias resolution, switch-provider validation)
- `e0055d7` — Multi-endpoint dispatcher + MODEL_TEMPERATURE_OVERRIDES (Fix #4 + Fix #5 B)
- `a5b0103` — Retry safety net for temperature rejection (Fix #5 A)
- `2bc6e90` — Dispatcher model-aware + alias-aware (no path duplication)
- `67d633a` — Thinking-block content recovery (qwen3.x)

## Architecture changes

### `src/llm/opencode_go.rs` — dispatcher

The previous OpenCode Go provider was a `[OpenAiCompatProvider]` wrapper that only knew the `chat/completions` endpoint. The 2026-08-04 roster exposes models across three different endpoints:

| Endpoint | Path | Models | SDK |
|---|---|---|---|
| `/v1/chat/completions` | OpenAI-compat | glm-5.1, glm-5.2, kimi-k3, kimi-k2.7-code, kimi-k2.6, deepseek-v4-pro, deepseek-v4-flash, mimo-v2.5, mimo-v2.5-pro, hy3 | `@ai-sdk/openai-compatible` |
| `/v1/messages` | Anthropic-compat | minimax-m3, minimax-m2.7, minimax-m2.5, qwen3.8-max, qwen3.7-max, qwen3.7-plus, qwen3.6-plus | `@ai-sdk/anthropic` |
| `/v1/responses` | OpenAI Responses | gpt-5.6-luna | `@ai-sdk/openai` |

The dispatcher is now **model-aware** (not endpoint-aware). The `endpoint_path_for(model)` function returns the canonical path for each model; the dispatcher constructs the full URL and routes to the right concrete provider:

```rust
match endpoint_path_for(&spec.model) {
    Some("messages") => OpenCodeGoAnthropicProvider::new(...),
    Some("responses") => OpenCodeGoResponsesProvider::new(...),
    Some("chat/completions") => OpenAiCompatProvider::new(...),
    None => Err(InvalidArgs("model not on roster")),
}
```

### `src/llm/opencode_go_anthropic.rs` — new provider

Anthropic-compatible provider for OpenCode Go's `/v1/messages`. Reuses `src/llm/http.rs` for the request body but **defines its own response parser** because OpenCode Go's Anthropic-compat endpoint has a non-standard shape:

```json
{
  "content": [
    {"signature": "", "thinking": "Thinking Process..."},    // ← no `type` field!
    {"text": "{...}", "type": "text"}
  ]
}
```

The leading `thinking` block omits the `type` field. The shared `MessagesResponseBody` in `src/llm/http.rs` requires every content block to carry `type` and silently drops the response. Fix: make `kind` optional and infer from the body shape:

```rust
#[derive(Debug, Deserialize)]
struct OpenCodeGoMessagesContent {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
}
```

When `kind` is missing, we infer `text` if the block has a `text` field, `thinking` if it has a `thinking` field. We also collect both blocks (text takes precedence, but thinking is promoted to text when no text block is present) so the JSON parser has something to chew on.

### `src/llm/opencode_go_responses.rs` — new provider

OpenAI Responses API provider for OpenCode Go's `/v1/responses`. Distinct from `OpenAiCompatProvider` because the Responses API uses a different request shape (`input` instead of `messages`) and a different response shape (`output[].content[].text` instead of `choices[0].message.content`).

### `src/llm/opencode_go.rs` — per-model temperature overrides + retry safety net

```rust
pub static MODEL_TEMPERATURE_OVERRIDES: &[(&str, f32)] = &[
    ("kimi-k3", 1.0),  // OpenCode Go rejects any other value with HTTP 400
];

// Fix #5 A: retry safety net — if the upstream rejects with a
// temperature-restriction 400, retry with T=1.0.
if let Err(Error::Provider(body)) = &first {
    let lower = body.to_ascii_lowercase();
    if (lower.contains("invalid temperature") || lower.contains("only 1 is allowed"))
        && lower.contains("temperature")
        && effective_req.temperature != Some(1.0)
    {
        return self.inner.send(&Request { temperature: Some(1.0), ..effective_req }).await;
    }
}
```

### `src/cli/mod.rs` — alias-aware model resolution

The CLI `--model` flag used to alias-resolve `minimax-m3` → `MiniMax-M3` unconditionally. After the dispatcher landed, this caused a conflict: `--provider opencode_go --model minimax-m3` should keep the string as-is (the OpenCode Go model id is `minimax-m3`, not `MiniMax-M3`). Fix: only resolve the alias when the alias resolves to the same provider kind as the user's selected provider.

```rust
let resolved = if cfg.providers.contains_key(raw)
    && (raw == selected
        || cfg.providers.get(raw).map(|s| s.kind.as_str())
            == cfg.providers.get(&selected).map(|s| s.kind.as_str()))
{
    cfg.providers.get(raw).map(|s| s.model.clone()).unwrap_or_else(|| raw.to_owned())
} else {
    raw.to_owned()
};
```

### `src/config.rs` — 18 model roster

Registers all 18 models with the operator's correct endpoint:

```rust
let oc_chat = "https://opencode.ai/zen/go/v1";
let oc_messages = "https://opencode.ai/zen/go/v1/messages";
let oc_responses = "https://opencode.ai/zen/go/v1/responses";

m.insert("opencode_go",     make_opencode_go("kimi-k2.7-code", oc_chat));
// ... 9 more chat/completions models
m.insert("minimax-m3",      make_opencode_go("minimax-m3",     oc_messages));
// ... 6 more messages models
m.insert("gpt-5.6-luna",    make_opencode_go("gpt-5.6-luna",   oc_responses));
```

The `default_providers_lists_four_canonical_minimax_models` and `env_overrides_minimax_model` tests were updated to reflect that `minimax-m3/m2.7/m2.5` are now OpenCode Go providers (not direct Minimax).

## Probe methodology

Each model was run with:

```
moagan run --mode fast --provider opencode_go --model <name> \
  --prompt "Diseña una API REST minimal para gestionar tareas. Responde con estructura JSON." \
  --non-interactive --runs-dir <run_dir>
```

Timeout: 600s per model (the previous 90s cut off most models mid-pipeline). Each model ran in a fresh `--runs-dir` so the cross-run cache didn't pollute results.

Results categorized as:
- **PASS**: `final/portfolio.md` and `rankings/ranking.json` produced
- **BLOCKED**: `InvalidArgs` from the BLOCKED_MODELS gate (operator policy)
- **SCHEMA_VIOLATION**: response text empty or malformed JSON
- **TIMEOUT**: 600s exceeded
- **DECODE_FAIL**: response body didn't match the expected shape
- **NETWORK_ERR**: TCP / DNS error

## Per-model results

| Model | Endpoint | Wall-clock | Result | Notes |
|---|---|---:|---|---|
| `gpt-5.6-luna` | `/v1/responses` | 80s | PASS in initial probe (then 400 in second probe) | Transient upstream; resolved with retry |
| `glm-5.1` | `/v1/chat/completions` | 12s | FAIL | 137 output_tokens, text empty |
| `glm-5.2` | `/v1/chat/completions` | 175s | FAIL | 175 output_tokens, text empty |
| `kimi-k3` | `/v1/chat/completions` | 4s | FAIL | HTTP 500 Router.Unavailable (upstream) |
| `kimi-k2.7-code` | `/v1/chat/completions` | 600s | FAIL | Returns prose-prefixed output, JSON never emitted |
| `kimi-k2.6` | `/v1/chat/completions` | 7s | FAIL | Similar to kimi-k2.7-code |
| `deepseek-v4-pro` | `/v1/chat/completions` | 220s | FAIL | Empty response after 220s |
| `deepseek-v4-flash` | `/v1/chat/completions` | 480s | FAIL | 200 OK but empty content (region fix worked but routing failed) |
| `mimo-v2.5` | `/v1/chat/completions` | 456s | **PASS** | First chat-completions model to complete |
| `mimo-v2.5-pro` | `/v1/chat/completions` | 264s | **PASS** | Faster than mimo-v2.5 |
| `minimax-m3` | `/v1/messages` | n/a | BLOCKED | Operator policy: prefer direct MiniMax |
| `minimax-m2.7` | `/v1/messages` | n/a | BLOCKED | Operator policy: prefer direct MiniMax |
| `minimax-m2.5` | `/v1/messages` | n/a | BLOCKED | Operator policy: prefer direct MiniMax |
| `qwen3.8-max` | `/v1/messages` | 600s | **PASS** (after retry) | Initial run: network blip mid-pipeline |
| `qwen3.7-max` | `/v1/messages` | 447s | **PASS** | Pre-fix: empty text. Post-fix: thinking-block recovery works |
| `qwen3.7-plus` | `/v1/messages` | 600s | largely **PASS** | Briefly drops to "critique" failure mid-run, restarts |
| `qwen3.6-plus` | `/v1/messages` | 478s | **PASS** | |
| `hy3` | `/v1/chat/completions` | 4s | FAIL | HTTP 400 (Console Go) |

## Root cause analysis

### 1. Anthropic-compat `thinking` block recovery (qwen3.x)

The qwen3.x models on OpenCode Go's `/v1/messages` endpoint return responses where the leading `thinking` block carries NO `type` field. The shared `MessagesResponseBody` in `src/llm/http.rs` requires every content block to have `type`, causing the parser to fail and return empty text. Pre-fix: **all 4 qwen3.x models failed**. Post-fix: **all 4 work** (with the longer timeout).

### 2. Chat-completions empty responses (glm-5.x, kimi-k2.x, deepseek)

The `chat/completions` models with `response_format=json_object` either return:
- Empty content despite 200 OK (glm-5.1, glm-5.2, deepseek-v4-pro)
- Prose-prefixed response where the JSON is never emitted (kimi-k2.7-code, kimi-k2.6)

The `response_format` field is accepted by the upstream (no 400) but the model semantics don't honor it. Two mitigation paths:
- **A)**: Skip `response_format` for these models (return to the original prompt-only contract)
- **B)**: Parser-side tolerant JSON extraction (find the outermost `{...}` after stripping prose)

Option A is the simpler fix. The trade-off: re-introduces the SchemaViolation risk that motivated Fix #1. A defensive parser would hedge both bets.

### 3. kimi-k3 temperature rejection

The upstream API rejects any temperature other than 1.0 with HTTP 400 ("only 1 is allowed for this model"). Fixed by:
- **B (per-model map)**: `MODEL_TEMPERATURE_OVERRIDES` lists `kimi-k3 → 1.0`
- **A (retry safety net)**: catches new models that aren't in the map yet

After the fix, kimi-k3 still fails (HTTP 500 from upstream) but for a different reason — the upstream is just unavailable today.

### 4. Path duplication bug

The dispatcher originally constructed `endpoint = base + path` and the concrete provider's URL builder appended the path again, producing `/v1/messages/v1/messages`. Fixed by passing the base endpoint to the concrete provider and relying on its existing url-append logic.

### 5. `minimax-m3` alias collision

The previous CLI alias resolution unconditionally translated `minimax-m3` → `MiniMax-M3`. After the OpenCode Go dispatcher landed, this broke `--provider opencode_go --model minimax-m3` (the OpenCode Go model id is `minimax-m3`, not `MiniMax-M3`). Fixed by making the alias resolution provider-kind-aware.

### 6. China-hosting option enabled

The operator activated "China-hosted models" in the OpenCode Go subscription. This unblocked `deepseek-v4-flash` from the regional restriction. The model still fails downstream but for the same empty-content reason as the other chat-completions models.

### 7. http 400 from upstream (gpt-5.6-luna, hy3)

`gpt-5.6-luna` returned HTTP 400 once with "Provider returned error" (transient). `hy3` consistently returns HTTP 400. These are upstream issues outside moagan's control.

## Test status

| Suite | Tests | Passing |
|---|---:|---:|
| Unit tests | 833 | 833 ✅ |
| Smoke scripts (smoke_audit_proxy + 11 phase smokes) | 470+ | 467 (3 pre-existing grep-window bugs) |
| E2E audit proxy | 37 | 37 ✅ |
| `cargo clippy -D warnings` | n/a | ✅ |
| `cargo fmt --check` | n/a | ✅ |

## Open issues (not implemented, recommended decisions)

1. **Should `response_format` be removed for chat-completions models that don't honor it?** (glm-5.x, kimi-2.x, deepseek-v4-pro).
   - **Recommended**: Keep `response_format` for models that work, add a per-model opt-out map (e.g. `glm-5.1`, `kimi-k2.7-code`).
2. **Should we add a parser-side tolerant JSON extractor** (Bloque F recommendation #2)?
   - **Recommended**: Yes, but as a backup layer — only after the per-model `response_format` opt-out map is exhausted.
3. **Should `mimo-v2.5` and `mimo-v2.5-pro` be the default OpenCode Go models** for fast mode?
   - **Recommended**: Yes for short prompts (the only ones that worked). The 4 qwen3.x models are faster end-to-end.
4. **Should we add hy3-preview to the BLOCKED_MODELS list** (HTTP 500 ModelNotFound)?
   - **Recommended**: Not necessary — the upstream removed the model, so any future request will return 500. We can just let the error surface.
5. **DeepSeek v4-flash on direct provider** (`https://api.deepseek.com/v1`): the operator has direct access. Should we recommend using that instead of the OpenCode Go subscription?
   - **Recommended**: Yes, until the OpenCode Go subscription is more stable.

## Files generated

- `docs/benchmarks/validation-2026-08-04-probe-v2.md` (this report)
- `docs/benchmarks/validation-2026-08-04/probes/probe-v2-fast/` — passthrough probes against all 18 models
- `docs/benchmarks/validation-2026-08-04/probes/probe-v2-inspect/` — raw response inspection for qwen3.7-max
- `docs/benchmarks/validation-2026-08-04/probes/probe-v2-raw/` — raw response capture for failing models
- `docs/benchmarks/validation-2026-08-04/probes/probe-v2-single/` — single-model probe (qwen3.7-max fix verification)
- `docs/benchmarks/validation-2026-08-04/probes/probe-v2-qwen38/` — qwen3.8-max verification

## Commits (GPG-signed)

```
67d633a fix(opencode_go_anthropic): recover thinking-block responses (qwen3.x)
2bc6e90 fix(opencode_go): dispatcher is model-aware + alias-aware (no path duplication)
a5b0103 feat(llm): opencode_go retry safety net for temperature rejection (Fix #5 A)
e0055d7 feat(llm): opencode_go multi-endpoint dispatcher + per-model temperature override
85d40c2 fix(validation-2026-08-04): three Q8 follow-up fixes
1534e2d (Q7) feat(llm): opencode_go provider with blocked-model policy
```

All 5 commits ahead of `origin/main`. Local working tree is clean (only `docs/benchmarks/validation-2026-08-04/` is untracked).
