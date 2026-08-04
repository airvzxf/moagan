# Bloque E — OpenCode Go multi-model validation findings

**Date**: 2026-08-04 (run 05:19–05:36 local, total ≈ 17 min wall)
**Scope**: 10 OpenCode Go models × {connectivity, mode=fast, mode=standard} +
3 BLOCKED_MODELS negative tests (no audit proxy; direct endpoint).
**CSV**: `/tmp/opencode/validation-2026-08-04/bloque_e.csv`

## Result matrix (10 models, deduped)

| Model            | Connectivity | mode=fast   | mode=standard |
|------------------|--------------|-------------|----------------|
| kimi-k2.7-code   | PASS (200)   | SCHEMA_VIOLATION (rc=7, 38s, 449/2355 tok) | SKIPPED |
| kimi-k3          | PASS (200)   | FAIL (rc=40, 3s, 0/0 tok) | SKIPPED |
| glm-5.1          | PASS (200)   | TIMEOUT (180s, 1305/1583 tok) | SKIPPED |
| glm-5.2          | PASS (200)   | TIMEOUT (181s, 1071/1450 tok) | SKIPPED |
| hy3              | PASS (200)   | FAIL (rc=40, 25s, 143/851 tok) | SKIPPED |
| hy3-preview      | FAIL (500 ModelNotFound) | SKIPPED | SKIPPED |
| mimo-v2.5        | PASS (200)   | FAIL (rc=40, 39s, 534/2392 tok) | SKIPPED |
| mimo-v2.5-pro    | PASS (200)   | FAIL (rc=40, 57s, 501/2065 tok) | SKIPPED |
| qwen3.6-plus     | PASS (200)   | TIMEOUT (180s, 974/6475 tok) | SKIPPED |
| qwen3.7-plus     | PASS (200)   | TIMEOUT (181s, 2310/12398 tok) | SKIPPED |

**Totals**: PASS=13, FAIL=5, TIMEOUT=4, SCHEMA_VIOLATION=1, SKIPPED=9 (8 standard + 1 hy3-preview due to connectivity fail).

**blocked_model_tests_passed**: 3/3 (minimax-m3, minimax-m2.7, minimax-m2.5 all rejected with rc=2 InvalidArgs + "is blocked for opencode_go" message).

**Hypothesis delta**: the operator's hypothesis was "kimi-k2.7-code is the only one that completes; others either time out or SchemaViolation in route." Reality is **0/10 models complete mode=fast**, and the failure mix is more diverse (ProviderError, SchemaViolation, TIMEOUT, ModelNotFound). kimi-k2.7-code — the expected survivor — also fails with SchemaViolation.

---

## Findings

### CRITICAL-1: 0/10 OpenCode Go models complete a full `mode=fast` run

Every model either hits a temperature constraint error, decodes failure,
runs out of tokens mid-JSON, or times out at 180s. Only 3/10 even reach
the second phase (clarify). The Q8-benchmark expectation that
`kimi-k2.7-code` would be the sole survivor was wrong — even
kimi-k2.7-code fails on the intake call because its output exceeds
the per-role max_tokens budget (1024 for Intake) and the truncated
JSON does not parse.

This is a structural problem with the OpenCode Go provider config and
the role-level sampling knobs in `moagan`, not a one-model regression.

### CRITICAL-2: kimi-k3 only accepts `temperature=1` and is hard-incompatible with the pipeline

```
HTTP 400 invalid_request_error: "only 1 is allowed for this model"
```

The pipeline uses `temperature_for_role(Role)` which is 0.0 for
`Clarify`/`Route`/`Gate`/`Rank`, 0.4 for `Intake`, 1.0 for `Sketch`.
Since the first call (Intake, temp 0.4) fails with 400, the run is dead
on arrival. There is no per-model temperature override — the
`ProviderConfig.temperature` field in `src/config.rs:275` is
`Some(1.0)` for `opencode_go` but **is never used**: `phase.rs:264`
sets `temperature: Some(temperature_for_role(role))`, which clobbers
the config value. So `kimi-k3` cannot work without either (a) a
per-model temperature override, or (b) OpenCode Go relaxing the kimi-k3
constraint. Source: `src/phases/phase.rs:264` vs
`src/llm/opencode_go.rs:48–58`.

### CRITICAL-3: `hy3-preview` returns HTTP 500 Router.ModelNotFound

```
{"type":"Router.ModelNotFound","modelID":"hy3-preview"}
```

Either OpenCode Go has removed `hy3-preview` from the available model
list, or the model id has been renamed (e.g. `hy3-preview` → `hy3`).
`hy3` itself works at connectivity level but fails downstream with
`decode: error decoding response body`. The allowed-list in
`src/llm/opencode_go.rs:11–18` advertises both `hy3` and `hy3-preview`
as available; the live API disagrees. Update the source-of-truth list
or remove `hy3-preview` from the docs.

### MAJOR-1: 4/10 models time out at 180s during propose phase

`glm-5.1`, `glm-5.2`, `qwen3.6-plus`, `qwen3.7-plus` all reach the
`propose` phase and emit output tokens (1071–12398) but never finish.
The propose phase uses `max_tokens_for_role(Role::Propose) = 32768`,
which is generous, so the model is genuinely slow or stuck. The 180s
timeout is too tight for these models in `mode=fast`. Two options:

1. Raise the per-call timeout to 300s (the user-side budget allows it:
   $12/5h tier is well within reach).
2. Switch the operator's expectation: OpenCode Go's heavy models are
   not "fast" — they're "deep". Do not include them in the
   `mode=fast` allow-list.

### MAJOR-2: `mimo-v2.5`, `mimo-v2.5-pro`, `hy3` — decode failures on the `route` call

```
error: provider error: decode: error decoding response body
```

These models return a payload that the `openai_compat` decoder cannot
parse. Either (a) the model wraps the response in a non-OpenAI-compat
envelope, or (b) the `route` phase prompt produces a non-JSON reply
that fails the JSON parser. The intake/clarify phases succeed, so the
model is wired correctly; the route phase's JSON-mode forcing is the
likely culprit. This matches the open finding from Bloque F
(`S3_provider_trait_supports_json_mode FAIL`) — the OpenCode Go
provider does not advertise a `json_mode` capability, so the route
prompt cannot force JSON. Two paths:

- Add `response_format: {type: "json_object"}` to the OpenCode Go
  outgoing payload when the role is `Route` (matches
  `src/llm/openai_compat.rs:140`).
- Drop the JSON-mode requirement for OpenCode Go and let the route
  prompt do best-effort JSON.

### MINOR-1: `kimi-k2.7-code` Intake call produces 2355 tokens despite per-role cap of 1024

The provider config in `src/config.rs:274` says `max_tokens: Some(8192)`
but `phase.rs:263` overrides with `max_tokens_for_role(Role::Intake) =
1024`. The model still emits 2355 tokens (4.2 KB of valid JSON
truncated mid-string in the `risks` array). OpenCode Go's kimi models
appear not to honour `max_tokens` strictly — they emit until they run
out of internal context. Two fixes:

- Detect `finish_reason: "length"` (which we already log) and treat it
  as a retry-worthy failure rather than as a parse failure.
- Increase Intake `max_tokens` from 1024 to 4096 and retry once on
  length-truncated JSON.

### MINOR-2: kimi-k2.7-code is the only model with non-trivial fast-run tokens (449 in / 2355 out)

The other models either fail fast (0 in / 0 out for kimi-k3) or
timeout well into the propose phase. If kimi-k2.7-code's
length-truncation can be fixed (MINOR-1), it likely IS the sole viable
`mode=fast` model — the hypothesis was right about which model but
wrong about the failure mode.

### NOT_A_FINDING: BLOCKED_MODELS gate is intact

All three minimax-* models (m3, m2.7, m2.5) are correctly rejected
with `Error::InvalidArgs` and the message
`"model '<name>' is blocked for opencode_go; use direct minimax provider instead"`.
Source: `src/llm/opencode_go.rs:63–69`.

---

## Recommendations

1. **Block 5 models at the provider level until they can complete a fast
   run**: `kimi-k3`, `hy3-preview`, `mimo-v2.5`, `mimo-v2.5-pro`, plus
   treat `glm-5.1`, `glm-5.2`, `qwen3.6-plus`, `qwen3.7-plus` as
   `mode=deep` candidates only.
2. **Add a per-model temperature override map** in
   `src/llm/opencode_go.rs` so models like `kimi-k3` (only accepts
   temp=1) can be made compatible, or block them outright.
3. **Fix the `response_format` omission** on the route call (or
   mark Route as JSON-non-required for opencode_go) — this single
   change unlocks `hy3`, `mimo-v2.5`, `mimo-v2.5-pro` and probably
   several others.
4. **Raise Intake `max_tokens` from 1024 to 4096** (and retry on
   `finish_reason: "length"`) so kimi-k2.7-code can actually finish
   the intake call without truncation.
5. **Update the allowed-list comment** in `src/llm/opencode_go.rs:11`
   to remove `hy3-preview` (no longer routable on the live API).
