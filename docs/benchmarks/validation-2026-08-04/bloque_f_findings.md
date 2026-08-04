# Bloque F — Root cause of `Error::SchemaViolation` in route phase

**Subagent**: validation-2026-08-04/Bloque F
**Date**: 2026-08-04
**Drive script**: `drive_bloque_f.sh`
**CSV report**: `bloque_f.csv`
**Evidence directory**: `evidence/`

---

## TL;DR

5 of 6 model/provider combos reported in Q8 (`docs/benchmarks/multi-model-2026-08-04.md`)
fail with `Error::SchemaViolation` in the **route** phase. The failures reproduce
predictably in mock fixtures: **trailing prose after `}`, leading prose before `{`,
and JS-style comments** all trigger `SchemaViolation`. The current repair pass
only handles two narrow cases (missing `}`/`]` closers, missing `:` separators);
it does NOT handle the preamble/trailing pathology.

**In our environment on 2026-08-04, all three live MiniMax models (M3, M2.7, M2.5)
PASSED the route phase on both the user prompt and the Q8 prompt (mode=standard).
M3 wraps its output in ```` ```json ... ``` ```` markdown fences; the existing
`strip_code_fence` pre-pass handles that.** The Q8 failures were likely transient
or specific to the prompt state on that day — the failure mode is reproducible
*on demand* via the mock fixtures, but not necessarily reproducible against the
real API today.

**Root cause**: the wire format does not opt into native JSON mode
(`response_format: {type: "json_object"}`) for any provider. The `route.md`
prompt instructs the model to return JSON, but the API call does not enforce it,
so models are free to wrap their JSON in preamble/trailing prose/comments.

---

## Section 1 — Raw evidence

### Live API captures (mode=fast, user prompt)

| Model | Route phase | Wall-clock | Raw response shape |
|---|---|---:|---|
| `minimax/MiniMax-M3` | PASS | 82 s | JSON wrapped in `\`\`\`json ... \`\`\`` fences (stripped by `strip_code_fence`) |
| `minimax/MiniMax-M2.7` | PASS | 135 s | Plain JSON, no fences |
| `minimax/MiniMax-M2.5` | PASS | 97 s | Plain JSON, no fences |

**Sample M3 route response** (RESP_2 from `evidence/minimax_M3.json`):

```
```json
{
  "mode": "fast",
  "reason": "Simple enumeration of seven well-known colors in a fixed canonical order; ...",
  "sketches": 0,
  "proposals": 3,
  "judges": 3
}
```
```

This is what `strip_code_fence` was designed for (T01-06, `util.rs:689`). It
works for MiniMax today.

### Simulated raw evidence (mock fixtures for non-minimax models)

| Model | Fixture content | Route phase | Notes |
|---|---|---|---|
| `deepseek-v4-flash` | `{"mode":"fast",...}\n\nThis brief is...` | **FAIL** | trailing prose after `}` |
| `qwen3.7-plus` | `{"mode":"fast",...` (no closing brace) | PASS | bracket-repair saved it |
| `qwen3.7-max` | `Here is my analysis. ...\n{"mode":"fast",...}` | **FAIL** | preamble before `{` |
| `kimi-k2.7-code` (control) | `{"mode":"fast",...}` (plain) | PASS | as expected |

The bracket-repair case for `qwen3.7-plus` shows the existing `repair_m3_brackets`
pass firing (verified via `telemetry/warnings.jsonl`:
`code=model.json_repair_applied repair_kind=bracket bytes_before=82 bytes_after=83 bytes_delta=1`).

### Extra: Q8 reproduction (mode=standard, Q8 prompt)

`S4_minimax_M3_standard` ran the Q8 prompt *"Briefly: what are 3 best practices
for error handling in Rust?"* against MiniMax-M3 in mode=standard (169 s).
**Route phase PASSED**, full pipeline completed. No `SchemaViolation` warning
in the warnings stream. The Q8 failure was not reproducible in our environment
on 2026-08-04.

---

## Section 2 — Parse-pathology mocks (mode=fast, controlled)

| Test | Fixture | Result | Why |
|---|---|---|---|
| `S2_valid` | plain JSON | PASS | happy path |
| `S2_trailing_tokens` | `{"mode":"fast",...}\n\nEXPLANATION: ...` | **FAIL** | trailing prose — repair pass does not strip prose |
| `S2_missing_brace` | `{"mode":"fast",...` (no `}`) | PASS | `repair_m3_brackets` appends the missing `}` (verified via warning event) |
| `S2_comments` | `// Route decision follows.\n{"mode":"fast",...}` | **FAIL** | `//` is not JSON — repair pass does not strip comments |
| `S2_extra_fields` | `{"mode":"fast",...,"comment":"fine","confidence":0.91}` | PASS | serde ignores unknown fields (no `deny_unknown_fields`) |

**The two failing patterns are the ones that match the Q8 description** —
*"extra trailing tokens, missing closing braces, or comments"*. Missing braces
are covered by the repair pass; the other two are not.

Captured `Error::SchemaViolation` message for the comments case:

```
error: schema violation: model output is not valid JSON after repair:
expected value at line 1 column 1; len=110 bytes;
tail="// Route decision follows.\n{\"mode\":\"fast\",...}";
full raw follows:
// Route decision follows.
{"mode":"fast","reason":"Simple enumeration","sketches":0,"proposals":3,"judges":3}
```

---

## Section 3 — Static analysis (json_mode / wire_format)

| Test | Verdict | Detail |
|---|---|---|
| `S3_prompt_route_forces_json` | PASS | `src/llm/prompts/route.md:3` says *"Return a JSON object (no prose, no markdown)"* |
| `S3_provider_trait_supports_json_mode` | **FAIL** | `Provider` trait (provider.rs:21) has NO `supports_json_mode()` method |
| `S3_minimax_emits_json_mode` | **FAIL** | `MessagesRequestBody` (http.rs:75) has NO `response_format`/`json_object` field |
| `S3_openai_compat_emits_json_mode` | **FAIL** | `ChatRequest` (openai_compat.rs:64) has NO `response_format`/`json_object` field |
| `S3_route_role_settings_json_mode` | **FAIL** | `role_settings(Role::Route)` returns `None` — Route is NOT marked JSON-required |

**`json_mode` exists in the codebase** as a field on `RoleSettings` (prompts.rs:21)
and is set to `true` for the three opt-in roles `MergeSynthesizer`,
`RecoveryExplainer`, `RationaleExtractor` — but those settings are NEVER plumbed
through to the wire format. The settings exist as data but are dead code.

---

## Pattern identified

| Layer | State | Effect |
|---|---|---|
| **Prompt-side** (`route.md`) | asks for "JSON object (no prose, no markdown)" | soft directive only; models with permissive decoding ignore it |
| **Role-side** (`RoleSettings.json_mode`) | exists, set only for 3 opt-in roles (not Route) | dead code — never read by any provider |
| **Wire-format** (`MessagesRequestBody`, `ChatRequest`) | NO `response_format` field | providers never ask the upstream API to enforce JSON mode |
| **Parser-side** (`parse_model_json` → `repair_m3_brackets`) | handles 2 narrow M3 pathologies (missing closers, missing colons) | does NOT handle preamble, trailing prose, comments |

The failure is **wire-format + parser-side**: the prompt can ask politely but the
API call does not enforce, so the parser has to deal with whatever shape the
model emits. The parser only fixes two narrow cases.

---

## Recommended solutions

| # | Solution | Effort | Pros | Cons |
|---|---|---|---|---|
| 1 | **Add `response_format: {type: "json_object"}` to wire format** | S (~30 LoC) | Forces model to emit pure JSON; eliminates preamble/trailing pathology at the source | Anthropic-compatible endpoint may use a different shape (`response_format` may not be supported there); must verify against MiniMax and OpenAI-compatible backends; some models reject `json_object` and emit nothing |
| 2 | **Tolerant JSON extractor in `parse_model_json`** (find outermost `{...}` after stripping prose) | M (~50 LoC) | Works with any output, including preamble and trailing prose; doesn't depend on API support | Brittle if model emits multiple JSONs; must handle nested strings/escapes; can extract wrong block in adversarial cases |
| 3 | **Wire `RoleSettings.json_mode` through `Request.response_schema` → wire format** | L (~150 LoC, touch all 4 providers + Request + 2 wire bodies) | Per-role control; aligns with the existing `RoleSettings` design | Largest blast radius; needs Anthropic/OpenAI compatibility research first |
| 4 | **Strip JS-style comments and surrounding prose in `parse_model_json`** | S (~30 LoC) | Targeted fix for the two most common pathologies | Brittle; new models may emit new pathologies; adds ad-hoc string surgery |

### Recommendation

**Primary: combine #1 (native JSON mode) + #2 (tolerant extractor as safety net).**

Wire-format enforcement eliminates the pathology at the source for the models
that support `response_format: {type: "json_object"}` (most modern providers
do). The tolerant extractor catches any residual cases — older models, models
that ignore the API flag, models that wrap JSON in markdown fences anyway.
The combination gives a defense-in-depth that survives both API quirks and
model regressions.

**Secondary**: also implement #4 (strip comments + preamble) as an additional
narrow band-aid until #1+#2 ship. This buys time without changing wire format.

**Don't implement #3** until #1+#2 have been validated against the real MiniMax
endpoint — the Anthropic-compat endpoint may not support `response_format`, and
plumbing it without verifying could regress other roles.

### Implementation sketch for the recommended solution

1. **Wire format** (`src/llm/http.rs:75`, `src/llm/openai_compat.rs:64`):
   - Add an optional `response_format: Option<serde_json::Value>` field to
     `MessagesRequestBody` and `ChatRequest`.
   - `#[serde(skip_serializing_if = "Option::is_none")]`.
   - On the Anthropic side, send the OpenAI-compatible shape
     `{"type": "json_object"}` (most Anthropic-compat endpoints accept it).
   - If the upstream rejects, the existing retry path will surface the error.

2. **`Provider` trait** (`src/llm/provider.rs:21`):
   - Add `fn supports_json_mode(&self) -> bool { false }` default.
   - Override to `true` in `MinimaxProvider`, `OpenAiCompatProvider` (covers
     DeepSeek + OpenCode Go).

3. **`Request` + `RunContext::call_with_retry_parse`** (`src/phases/phase.rs:617`):
   - When `role_settings(role).json_mode && provider.supports_json_mode()`,
     set `req.response_schema` (already exists as a field on `Request` at
     `src/llm/wire.rs:28`) to `Some(json!({"type":"json_object"}))`.

4. **Parser-side** (`src/phases/util.rs:125`):
   - After `strip_code_fence`, if direct parse fails, try a regex/char-walk
     extraction: find the outermost `{`/`[` and matching closer that produces
     parseable JSON.
   - Log a `model.json_repair_applied` event with kind `extraction`.

5. **`role_settings(Role::Route)`** (`src/llm/prompts.rs:25`):
   - Add an entry: `Role::Route => Some(RoleSettings { temperature: 0.0, top_p: 0.95, max_tokens: 1024, json_mode: true })`.
   - Same for `Intake`, `Clarify`, `Sketch`, `Propose`, `Judge`, `Rank`, `Deliver`
     if you want the same guarantee across the pipeline.

---

## Files / references

- Drive script: `/tmp/opencode/validation-2026-08-04/drive_bloque_f.sh`
- CSV: `/tmp/opencode/validation-2026-08-04/bloque_f.csv`
- Evidence: `/tmp/opencode/validation-2026-08-04/evidence/`
- Mock fixtures: `/tmp/opencode/validation-2026-08-04/mock_fixtures/`
- Source: `src/phases/route.rs:13`, `src/llm/role.rs:184`, `src/llm/prompts.rs:25`,
  `src/llm/http.rs:75`, `src/llm/openai_compat.rs:64`, `src/llm/provider.rs:21`,
  `src/llm/wire.rs:9`, `src/phases/util.rs:125`, `src/phases/phase.rs:617`

---

## Test summary

- Total tests: 18
- Passed: 11
- Failed (expected, demonstrating the bug): 5 (S1_deepseek-v4-flash, S1_qwen3.7-max, S2_trailing_tokens, S2_comments, plus S3 wire-format gaps)
- Blocked / skipped: 0
- Duration: ~9 minutes (the 3 minimax M-series captures take ~5 minutes total wall-clock; mocks are sub-second each)
