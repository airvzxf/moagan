# ADR 0006 — Structural validation for `integration_discover_minimax`

> **Status**: Accepted
> **Date**: 2026-09-08
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: nothing (additive to existing
> `tests/integration_discover_minimax.rs`).
> **Relates to**:
> [`tests/integration_discover_minimax.rs`](../../tests/integration_discover_minimax.rs),
> [`src/telemetry/stdout_events.rs`](../../src/telemetry/stdout_events.rs)
> (the `Event<'a>` enum, NDJSON stream shape),
> [`docs/events-v1.md`](../events-v1.md) (event schema reference),
> [`scripts/e2e_audit_proxy.sh`](../../scripts/e2e_audit_proxy.sh)
> (the same `run_start`/`run_end`/`phase_end` grep vocabulary
> used in shell-side asserts),
> issue #761 (root-cause analysis surfaced this gap).

## Context

The real-API integration test
`tests/integration_discover_minimax.rs::discover_minimax_writes_four_subdirs`
asserts only that four filesystem subdirectories
(`tags/`, `facets/`, `extractions/cat_*`, `drafts/` soft) contain at
least one entry after running `moagan discover`. It does **not**
assert any structural property of the NDJSON event stream that
`moagan` emits on stdout (see
[`src/telemetry/stdout_events.rs`](../../src/telemetry/stdout_events.rs)
for the `Event<'a>` enum, schema version 1, `phase_start`/`phase_end`
discriminators).

The 2026-09-07 16:55 UTC failure
([run `34145427514`](https://github.com/airvzxf/moagan/actions/runs/34145427514),
commit `7f0c3f0`) surfaced this gap. The run panicked at
`tests/integration_discover_minimax.rs:97` with exit code 101, but
the actual upstream cause was a MiniMax-M3 response truncation:

```
schema violation: model output is not valid JSON:
expected value at line 1 column 1; len=12 bytes;
tail=\"iberties\\": }\"
```

The model returned only the 12-byte tail fragment of a JSON object
that had been clipped mid-word. `phases::util::parse_model_json_traced`
does not match this pathology (it handles `"problem":` bare-prefix
fragments, not tail-prefix fragments). The cached response body was
22 tokens of garbled pseudo-JSON with `finish_reason: "end_turn"` —
the model declared success while emitting a sentence fragment.

The filesystem check at `integration_discover_minimax.rs:156-196`
would have caught this **only** if the response was so truncated
that no subdirectory was produced. In the failing run, the `intake`
phase completed (producing one tag entry) before the `clarify` phase
failed, so the test's first assertion `tags/ ≥ 1 entry` passed
before the panic. A more structured assertion surface would have
emitted a clear "phase_error at clarify: schema violation…"
diagnostic instead of the generic `panicked at line 97:5` panic.

## Decision

Add a second `#[test]` in the same file — **`discover_minimax_structural_validation`** — that:

1. Redirects `moagan discover` stdout to a file in the artifact
   root (`<artifact_root>/moagan-stdout.ndjson`) and stderr to
   `<artifact_root>/moagan-stderr.ndjson`. The current test uses
   `Command::output()` which captures both into memory; redirecting
   to file lets the new test parse the NDJSON without changing the
   existing one.
2. Passes `MOAGAN_EVENT_FORMAT=jsonl` and `--event-format=jsonl`
   (belt-and-braces — the test runs under `cargo test`, which is
   non-TTY, so the auto-detect at
   `src/telemetry/stdout_events.rs:65-78` already activates JSONL;
   the explicit env var makes the intent visible in CI logs).
3. Passes `--decision-format=all` to surface the high-volume
   `Decision` events (`category_assigned`, `judge_verdict`, etc.)
   that are otherwise suppressed under the default `Summary` level.
4. Parses the NDJSON file with a small `parse_events` helper
   (~40 LOC, defined at the top of the new test).
5. Asserts four load-bearing invariants after the existing
   filesystem checks (the new test is **strictly additive** — the
   existing test is untouched, and a regression in directory layout
   still fails first with the existing diagnostic).

The four hard asserts:

| # | Invariant | Failure mode it catches |
|---|---|---|
| 1 | `run_start` and `run_end` events present; `run_end.status == "ok"` | Binary crashed before dispatch, or pipe to stdout broken |
| 2 | All 9 discover-mode phases emitted `phase_start`: `intake`, `clarify`, `discover_tag`, `discover_cluster`, `discover_contradict`, `discover_facet`, `discover_extract`, `discover_integrate`, `discover_summary` | Pipeline skipped a phase (regression in phase dispatcher). Note: `discover_matrix` is the orchestrator and does NOT itself emit `phase_start` — its children (tag, cluster, contradict, facet, extract, integrate, summary) each emit their own. |
| 3 | At least one `llm_call` event with `ok == true` | All LLM calls failed silently; filesystem could still be empty |
| 4 | Zero `phase_error` events | Any phase exited with `exit_code != 0` — the **exact failure mode** of run `34145427514` |

Plus two soft asserts (via `eprintln!("NOTE: …")`, not
`assert!`):

| # | Invariant | Why soft |
|---|---|---|
| 5 | `Decision{category_assigned}` present under `--decision-format=all` | The `discover_summary` phase sometimes emits zero `category_assigned` for trivial clusters — same precedent as the existing `drafts/` soft-check at `tests/integration_discover_minimax.rs:175-185` |
| 6 | `Warning` code allow-list excludes `rate_limit.*`, `throttle.*`, `circuit_open.*` | Upstream saturation is informational; a hard fail here re-creates the issue #761 false-positive churn (every quota blip turns the commit page red) |

## Why this is additive, not a replacement

The existing `discover_minimax_writes_four_subdirs` test still runs
first. If a future regression drops a subdirectory, the existing
asserts fire with the familiar diagnostic. The new test only adds
surface — it does not change any existing pass/fail behaviour.

Both tests share the same artifact root, the same binary path, the
same `MOAGAN_MAX_TOKEN_AUTO=0` short-circuit, and the same `--runs-dir`
argument. They differ only in:
- stdout/stderr redirection (new test redirects to file; existing
  test uses `.output()` capturing into memory),
- the `MOAGAN_EVENT_FORMAT=jsonl` env var on the new test only,
- the new `parse_events` helper + 4 hard asserts + 2 soft asserts.

## What this does NOT do

- **Does not modify the existing test.** No behaviour change for
  developers who run only `discover_minimax_writes_four_subdirs`.
- **Does not add new event kinds.** The 10 existing variants in
  `src/telemetry/stdout_events.rs:222-315` are sufficient; the new
  test reads them via the documented NDJSON schema
  ([`docs/events-v1.md`](../events-v1.md)).
- **Does not gate merge.** The existing 8-context `protect-main`
  required-checks list (`docs/branch-protection.md:69-77`) is
  unchanged. `test-ignored-minimax.yml` remains informational, not
  required, so a flake on this new assertion does not block merges.
- **Does not require new dependencies.** The `serde_json` crate is
  already in `Cargo.toml:43-48`; the parser uses it directly.
- **Does not apply to `integration_discover_deepseek.rs` or
  `integration_discover_opencode.rs` in this PR.** The OpenCode
  test was removed in PR #816 (operator decommissioning OpenCode
  subscription); the DeepSeek test could land the same change in a
  future PR if the operator wants parity.

## Anti-flake measures

1. **Stray non-JSON line tolerance.** The `parse_events` helper
   uses `serde_json::from_str` per line and skips failures via
   `continue`. A `tracing` event leaking onto the NDJSON stream
   (should never happen because `MOAGAN_EVENT_FORMAT=jsonl` is set)
   is silently dropped instead of failing the test.
2. **Consecutive-dedup on `decision_kinds`.** High-volume kinds
   (`cache_hit`, `cache_miss`, `category_assigned`) emit dozens of
   times. The dedup keeps the assert log readable without changing
   pass/fail.
3. **Soft vs hard.** The `drafts/` soft-check precedent at
   `tests/integration_discover_minimax.rs:175-185` is preserved.
   Anything that would be a hard fail in the existing test stays
   a hard fail; anything that would be a soft signal stays soft.
4. **Tolerant `rate_limit` warning.** Up to N warnings (count
   configurable) are tolerated; only a sustained burst of upstream
   saturation fails the test. The existing `model.retry_provider`
   self-heal path already catches single-shot 429s and retries, so a
   few warnings on the happy path are normal.

## Validation cost

- **Test wall-clock**: identical to the existing test (~22 min on a
  warm cache for both tests, since both share the binary build).
  The new test piggybacks on the same `cargo test` invocation —
  they run sequentially because both write to the same
  `CARGO_TARGET_TMPDIR` subdir. If a developer runs only one via
  `--test integration_discover_minimax -- --ignored
  discover_minimax_structural_validation`, the other is skipped.
- **CI minutes**: +0 net, because the workflow already runs the
  binary once per push. The second test reuses the cached binary
  build from `actions/cache`.
- **LLM tokens**: identical to today — both tests use the same
  upstream call pattern. The new test adds a `Decision` event log
  on stdout, which is a few KB; the LLM call cost is unchanged.

## Rollout

The new test lands in this same PR
([`investigation/issue-761-root-cause` branch](../..)). It runs
behind the same `#[ignore = "requires MINIMAX_API_KEY; run with --ignored"]`
flag as the existing test, so local `cargo test` without the env var
is unaffected. Operators who already run the workflow manually
(`gh workflow run test-ignored-minimax.yml`) will see the new test
appear in the next run's artifact list with no change to wall-clock.

## ADR note: not creating new tests/common/

The new test could share helpers with the existing
`integration_discover_deepseek.rs` (and the now-removed
`integration_discover_opencode.rs`) via a `tests/common/` module.
The project does not currently use that convention — the three
sibling files are near-verbatim copies with the provider/model
swapped (verified by side-by-side comparison). Introducing a
`tests/common/` module here would establish a new project
convention in a PR whose primary purpose is structural validation.
Per AGENTS.md "one logical change per commit", this is deferred to
a future PR specifically for the `tests/common/` refactor.

## Decision rationale (not preference)

The 34145427514 failure produced an opaque panic message that took
~30 min of triage to identify as a JSON-truncation upstream bug.
With the new asserts in place, the same failure would emit
`"phase_error events present: [(\"clarify\", \"schema violation:
model output is not valid JSON: ...\")]"`. The triage time goes from
30 min to 30 seconds, and the next agent or human reading the log
sees the structural violation directly instead of inferring it from
a `panicked at …:97:5` panic site.

This is not about preventing the upstream truncation itself
(PRs #799, #810, #811 already fixed four concurrency bugs in this
area; the truncation is a MiniMax response-quality issue that no
client-side fix can prevent). It is about making the test surface
catch **any** class of pipeline-shape regression — truncation,
missing phase, all-llm-failed, dispatcher-crash — with a
self-describing diagnostic.

## Status

Prototype only at the time of this ADR. Implementation lands in
the same PR as this ADR (see
[`tests/integration_discover_minimax.rs`](../../tests/integration_discover_minimax.rs)
for the live code). The new test is named
`discover_minimax_structural_validation` and lives at the bottom of
the file (after the existing `discover_minimax_writes_four_subdirs`).
