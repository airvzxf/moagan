# Research — JSON structured output across the provider roster

> **Status**: research only. Per user instruction, **do not fix** the JSON
> parser / LLM-output repair in this session. This document captures the
> current behaviour, the catalog gaps that intersect it, and three
> implementation paths for future sessions.

## TL;DR

Most providers in the OpenCode Go roster (and a few in `openai_compat`)
do not reliably honour `response_format: json_object`. Models emit
prose-prefixed JSON, empty content, or structured-but-malformed payloads.
The current local heuristic (`src/phases/util.rs::repair_m3_brackets`)
handles a narrow subset (`:`, `]`, separator fixes) and leaves everything
else to fail.

## Findings (verified during Track C, session before handoff)

- 18 models on the OpenCode Go roster.
- 7 complete `mode=fast` end-to-end without JSON repair intervention.
- 6 models ignore `response_format: json_object` entirely:
  - `glm-5.1`
  - `glm-5.2`
  - `kimi-k2.6`
  - `kimi-k2.7-code`
  - `deepseek-v4-pro`
  - `kimi-k3`
- Failure modes observed:
  - empty `content`
  - prose-prefixed JSON (model writes a sentence then the JSON)
  - prose-suffixed JSON
  - JS-style comments inside the JSON
  - truncated output (token cap hit before closing brace)
- Provider-level `response_format` opt-out is **not** currently wired —
  every provider sends the same JSON request regardless of model.

## Current code state

### Local heuristic
`src/phases/util.rs::repair_m3_brackets` (13-38) only fixes:
- trailing `:` / `]`
- missing separators
- one-line prefix

It does **not** strip prose prefixes/suffixes, JS comments, or stream-aware
truncation. Anything beyond those narrow cases surfaces as
`SchemaViolation` exit 7.

### Catalog gaps
Three D.7.x items are unwired as of `main @ cca0817`:

| D.# | Capability | Status |
|---|---|---|
| D.7.1 | `Role::JsonRepairV2` (LLM re-call with full schema) | enum variant + prompt placeholder only (PR #81 batch-2). No caller. |
| D.7.2 | `strip_control_tokens` (regex pre-parse) | not implemented |
| D.7.3 | streaming parser for early truncation | not implemented |
| D.7.4 | per-model `response_format` opt-out map | not implemented |

The `use_json_repair: bool` flag in `RetryBudget` is currently **informative**
— it sets a marker on the budget but does not trigger a real re-call.

## Three implementation paths for future sessions

### Path A — minimal per-model opt-out (1 PR, S-sized)

Add a `MODEL_RESPONSE_FORMAT_OPT_OUT` table (analogous to the existing
`MODEL_TEMPERATURE_OVERRIDES`) listing the 6 known-bad models. Providers
that honour a model list (notably `opencode_go` and `openai_compat`)
skip `response_format: json_object` for opted-out models and parse the
prose output with a tolerant extractor instead.

Pros:
- Smallest blast radius (no parser changes).
- Unblocks the 6 broken models in `mode=fast` immediately.
- Reuses existing per-model override pattern.

Cons:
- Tolerated for those 6 models only; new broken models need manual opt-in.

### Path B — parser-side tolerant extractor (1 PR, M-sized)

Replace `repair_m3_brackets` with a multi-pass tolerant extractor:
1. Find the first `{` (or `[` for arrays).
2. Brace-balance ignoring content inside strings.
3. Strip `//` and `/* */` comments.
4. Strip a single prose prefix line if the prefix is `< 200 chars` and
   followed by whitespace + `{` / `[`.
5. Return the extracted JSON or `Err(SchemaViolation)`.

Pros:
- Fixes the 6 broken models without any provider changes.
- Future-proof: any new model that produces prose-prefixed JSON works.

Cons:
- Higher risk of false positives (extra `{` in a prose sentence).
- Requires careful test coverage (a dozen adversarial fixtures).

### Path C — `Role::JsonRepairV2` re-call (1 PR, M-sized)

When the local heuristic fails (or when `use_json_repair: true` is set
in the budget), the dispatcher issues a second LLM call with the full
target schema and a "repair this malformed JSON to match schema X"
prompt. Cost: one extra call per failure, ~1-2k extra tokens.

Pros:
- Most robust (LLM is good at structural repair).
- Schema-aware (the re-call knows what fields are required).

Cons:
- Doubles token cost on failure paths (which is when budget is already
  tight).
- Adds latency.

## Recommendation

Ship **Path A first** (1 PR, S) to unblock the 6 known-broken models.
Track **Path B** as a follow-up (1 PR, M) to make the parser more
robust without touching providers. Keep **Path C** as a last resort
when both A and B fail — opt-in via `use_json_repair: true` in the
budget so callers pay the cost only when they choose to.

## Acceptance criteria for any future fix

- [ ] `cargo test --all-targets` PASS with at least 6 new adversarial
      fixtures covering prose-prefix, prose-suffix, JS-comments,
      truncated, multi-line-wrap, and empty-string cases.
- [ ] `scripts/smoke_minimax.sh` PASS end-to-end.
- [ ] At least 10 of the 18 OpenCode Go models complete `mode=fast`
      without falling back to the heuristic.
- [ ] No regression on the 7 currently-working models.

## References

- Spec block: `docs/proposal-03-add-ons.md` §D.7.1-§D.7.4.
- Heuristic: `src/phases/util.rs::repair_m3_brackets`.
- Retry budget: `src/llm/retry_budget.rs`.
- Provider roster: `src/llm/opencode_go.rs::MODEL_TEMPERATURE_OVERRIDES`.
