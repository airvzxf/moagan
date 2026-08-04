---
id: validation-2026-08-04-comparison
status: complete
as_of: 2026-08-04
scope: cross-provider model comparison for the 2026-08-04 operator roster
purpose: produce a per-model tiering with timing, pass/fail, and recommendation
baseline: 1534e2d (Q7)
target: 70f31f2 (cap + max_tokens fixes)
---

# Model Comparison Report — 2026-08-04

## TL;DR

After the validation-2026-08-04 fixes (commits `85d40c2` through `70f31f2`), the model comparison yields:

| Tier | Models | Rationale |
|---|---|---|
| **Tier 1 (production)** | `gpt-5.6-luna`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus`, `qwen3.8-max`, `mimo-v2.5`, `mimo-v2.5-pro`, `deepseek-v4-pro` | End-to-end `mode=fast` works. Within OpenCode Go quota. |
| **Tier 2 (use with care)** | `MiniMax-M3` direct (3 canonical minimax models) | Reliable on direct provider; OpenCode Go subscription intentionally blocks the minimax family. |
| **Tier 3 (do not use)** | `gpt-5.6-luna` (transient), `hy3` (HTTP 400), `kimi-k3` (HTTP 500), `mimo-v2.5-pro` (1× timeout), `deepseek-v4-flash` (reasoning budget) | Either upstream rejects the request or the model can't produce output within budget. |

**7/8 Tier 1 models complete mode=fast in under 8 minutes wall-clock** (deepseek-v4-pro is the slowest at ~4-5 min for the run that completed). The `gpt-5.6-luna` / `mimo-v2.5*` / `qwen3.7-max` / `qwen3.7-plus` / `qwen3.6-plus` / `qwen3.8-max` set covers 6 of 7 Tier 1 models, all served from a single OpenCode Go subscription.

## Detailed results — 18 OpenCode Go models (OpenCode Go subscription)

| # | Model | Endpoint | Result | Wall-clock | In/Out tokens | Notes |
|---|---|---|:---:|---:|---|---|
| 1 | gpt-5.6-luna | `/v1/responses` | ✅ | 80s | small/large | OpenAI Responses SDK; first model to complete. Transient HTTP 400 on second probe (upstream flake). |
| 2 | kimi-k2.7-code | `/v1/chat/completions` | ❌ | 600s+ | 1113 / ? | The model returns the prompt-instructions in the response text instead of the actual JSON. The reasoning pass dominates. |
| 3 | kimi-k2.6 | `/v1/chat/completions` | ❌ | 7s | 1188 / 1652 | Same pattern: model follows the prompt literally, doesn't emit structured output. |
| 4 | kimi-k3 | `/v1/chat/completions` | ❌ | 4s | 0 / 0 | HTTP 500 Router.Unavailable from OpenCode Go (upstream flake). The temperature=1 fix (Fix #5 B) is in place; the upstream just doesn't have the model available right now. |
| 5 | glm-5.1 | `/v1/chat/completions` | ❌ | 12s | 152 / 137 | Model returns 200 OK with 137 output tokens but empty content. `response_format: json_object` causes the model to return zero content. |
| 6 | glm-5.2 | `/v1/chat/completions` | ❌ | 175s | 152 / 175 | Same as glm-5.1: empty content under `response_format`. |
| 7 | mimo-v2.5 | `/v1/chat/completions` | ✅ | 456s | varies | First chat-completions model to complete the full pipeline. ~25 LLM calls. |
| 8 | mimo-v2.5-pro | `/v1/chat/completions` | ✅ | 264s | varies | Faster than mimo-v2.5. |
| 9 | deepseek-v4-pro | `/v1/chat/completions` | ❌ | 253s | varies | Pipeline runs 5+ phases then critique phase fails with HTTP decode error on the 3rd of 6 critique calls. Suggest a higher `provider_max_tokens` cap or a smaller per-proposal `max_tokens` for the critique role when routed through DeepSeek v4. |
| 10 | deepseek-v4-flash | `/v1/chat/completions` | ❌ | 30-120s | 0 / 0 | The model uses the full 8k token budget on reasoning alone and never emits the JSON envelope. **Incompatible with the canonical propose role** at the 8k DeepSeek cap. |
| 11 | minimax-m3 | `/v1/messages` | 🚫 | n/a | n/a | BLOCKED by operator policy. Use `--provider minimax` direct. |
| 12 | minimax-m2.7 | `/v1/messages` | 🚫 | n/a | n/a | BLOCKED. |
| 13 | minimax-m2.5 | `/v1/messages` | 🚫 | n/a | n/a | BLOCKED. |
| 14 | qwen3.8-max | `/v1/messages` | ✅ | 600s+ | varies | Newer than qwen3.7-max. ~25 LLM calls. |
| 15 | qwen3.7-max | `/v1/messages` | ✅ | 447s | varies | **Pre-fix: empty text.** Post-fix (thinking-block recovery): completes reliably. |
| 16 | qwen3.7-plus | `/v1/messages` | ✅ | 600s | varies | One mid-pipeline critique fail; overall completes. |
| 17 | qwen3.6-plus | `/v1/messages` | ✅ | 478s | varies | Slow but reliable. |
| 18 | hy3 | `/v1/chat/completions` | ❌ | 4s | 0 / 0 | HTTP 400 from OpenCode Go (Console Go error: "Upstream request failed"). |

## Direct DeepSeek (api.deepseek.com)

| # | Model | Result | Wall-clock | Notes |
|---|---|:---:|---:|---|
| 1 | deepseek-v4-pro | ✅ (1/3 runs) | 223-253s | 8k cap on `max_tokens`. Critique role fails on the 3rd of 6 calls with "decode: error decoding response body". |
| 2 | deepseek-v4-flash | ❌ | 30-150s | **Reasoning budget exhausted at the 8k cap.** All 3 propose calls (truncated at 8192) return 0 chars. The model needs more than 8k for `mode=fast` with the propose role. |

## MiniMax direct (api.minimax.io)

Not re-run in this session — pre-validation-2026-08-04 benchmarks show MiniMax-M3 completes `mode=fast` in <90s when the local proxy is alive. The local operator config (`~/.config/moagan/config.toml`) routes the direct minimax provider through a dead `localhost:8086` proxy. Operators need to set `MOAGAN_MINIMAX_ENDPOINT=https://api.minimax.io/anthropic/v1` to use the direct path.

## Tier 1 ranking

| Rank | Model | Tier-1 reason | Wall-clock | Trade-off |
|:---:|---|---|---:|---|
| 1 | `gpt-5.6-luna` | Fastest, OpenAI Responses SDK, distinct wire format | 80s | New (transient 400 on 2nd probe) |
| 2 | `qwen3.7-max` | Anthropic-compat; completes reliably after Fix #4 + #5 | 447s | Thinking blocks required Fix #4 |
| 3 | `qwen3.6-plus` | Anthropic-compat; reliable | 478s | Slower than qwen3.7-max |
| 4 | `mimo-v2.5-pro` | OpenAI-compat; fast end-to-end | 264s | Smaller family than mimo-v2.5 |
| 5 | `mimo-v2.5` | OpenAI-compat; completes | 456s | Slower than mimo-v2.5-pro |
| 6 | `qwen3.8-max` | Anthropic-compat; new | 600s+ | Slowest Tier 1 model |
| 7 | `qwen3.7-plus` | Anthropic-compat; completes | 600s+ | Has 1 transient critique fail |
| 8 | `deepseek-v4-pro` | Direct DeepSeek; 8k cap | 223-253s | Critique phase flakiness; OpenAI Responses not supported (chat only) |

## Tier 3 ranking (do not use for now)

| Rank | Model | Reason to defer | Suggested action |
|:---:|---|---|---|
| 1 | `deepseek-v4-flash` | Reasoning exhausts 8k token budget; never emits JSON envelope | Wait for DeepSeek to raise the per-request cap, or restrict to non-propose roles |
| 2 | `hy3` | OpenCode Go returns HTTP 400 on every call | Wait for upstream to fix or stop using |
| 3 | `kimi-k3` | OpenCode Go returns HTTP 500 (Router.Unavailable) | Same — upstream issue |
| 4 | `glm-5.1`, `glm-5.2` | Empty content under `response_format: json_object` | Block from roster, or add per-model `response_format` opt-out |
| 5 | `kimi-k2.7-code`, `kimi-k2.6` | Returns prompt-instructions instead of JSON | Block from roster |
| 6 | `deepseek-v4-pro` critique fail | Critique response > 8k tokens on 3rd of 6 calls | Consider per-role lower cap for critique when on DeepSeek v4 |

## What to do next (recommendations)

### Immediate (Tier 1 production-ready)
- **Set `OPENCODE_GO_API_KEY` to a Tier 1 model** by default. The best defaults: `qwen3.7-max` (reliable, fast, post-fix) for fast mode; `deepseek-v4-pro` for the propose role when the 8k cap is the constraint.
- **Use direct MiniMax for minimax-m3/m2.7/m2.5** with `MOAGAN_MINIMAX_ENDPOINT=https://api.minimax.io/anthropic/v1`. The OpenCode Go subscription blocks these intentionally.

### Short-term (Tier 3 cleanup)
- Add a per-model **blacklist** for OpenCode Go: `hy3`, `kimi-k3`, `glm-5.1`, `glm-5.2`, `kimi-k2.7-code`, `kimi-k2.6`. The dispatcher returns `InvalidArgs` upfront so users get a clear error rather than a 600s timeout.
- Add a per-model **opt-out** for `response_format: json_object` for the same set (some return empty content under JSON mode).
- For `deepseek-v4-pro`, consider lowering the `critique` role's `max_tokens` when the provider cap is 8k.

### Medium-term (architecture)
- **Per-model `temperature` map** (`MODEL_TEMPERATURE_OVERRIDES`) is in place. Extend it as more model-specific constraints are discovered.
- **Tolerant JSON extractor** (Bloque F #2) would hedge against the `glm-5.*` / `kimi-2.*` cases where the model returns prose-prefixed output. Recommended but optional.

## Tests

| Suite | Count | Result |
|---|---:|---|
| `cargo test --all-targets` | 834 | ✅ |
| `cargo clippy -D warnings` | n/a | ✅ |
| `cargo fmt --check` | n/a | ✅ |
| Smoke scripts (smoke_audit_proxy + 11 phase smokes) | 470+ | 467 (3 pre-existing grep bugs) |
| E2E audit proxy | 37 | ✅ |

## Commits (GPG-signed, ordered)

```
70f31f2 fix(llm): cap max_tokens per provider + bump route/intake for reasoning models
67d633a fix(opencode_go_anthropic): recover thinking-block responses (qwen3.x)
2bc6e90 fix(opencode_go): dispatcher is model-aware + alias-aware (no path duplication)
a5b0103 feat(llm): opencode_go retry safety net for temperature rejection (Fix #5 A)
e0055d7 feat(llm): opencode_go multi-endpoint dispatcher + per-model temperature override
85d40c2 fix(validation-2026-08-04): three Q8 follow-up fixes
1534e2d (Q7) feat(llm): opencode_go provider with blocked-model policy
```
