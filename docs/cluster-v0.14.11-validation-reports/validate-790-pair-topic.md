# Validation report — issue #790 (`pair_topic` ignores its argument)

> Issue: #790 — `refactor(phases): pair_topic ignores its argument and returns a constant`
> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Current HEAD on `main`: `ce524e2` (`chore(release): v0.14.10 — Cargo.toml version bump (#793)`)

## TL;DR

The issue is **real and correctly diagnosed**: `pair_topic` at
`src/phases/discover_contradict.rs:188` takes `&ContradictionFinding`,
ignores it, and returns a fresh `String` containing the literal
`"consistency"` on every call. The line citations are exact, and
the function has exactly one caller (`src/phases/discover_contradict.rs:174`).

**Allocation analysis changes the picture.** The issue frames option A
as "removes the per-call `String` allocation (but `.to_owned()` still
allocates at the call site — is that OK?)". In fact, **option A does
not remove the allocation at all**: `PAIR_TOPIC.to_owned()` is exactly
the same per-call heap allocation that `pair_topic` does today, just
hoisted into the call site. The only thing option A removes is the
function call (and the misleading `_f` parameter); the allocation
itself is unchanged because the receiving field `Contradiction.topic`
is `String` (`src/domain/mod.rs:777`), not `&'static str`. Neither
option in the issue actually avoids the per-finding allocation.

**Recommendation: option A (the `const` form) is still the right pick**
— the issue's reasoning about discoverability ("a reader tracing why
every contradiction has `topic: consistency` has to open this function
to discover the answer") is the load-bearing argument, not the
allocation one. Option A makes the constant-ness *visible in the call
site* (the reader sees `PAIR_TOPIC` right next to `topic:`), removes
the misleading parameter, and shrinks the function. The diff is also
mechanical — 3 line changes in 2 places (plus a second call site at
line 161 that already hardcodes `"consistency".into()` and should be
harmonised).

## 1. Confirmed — function at line 188

Exact code at `src/phases/discover_contradict.rs:183-190`:

```rust
/// Topic tag for a single finding. The legacy sidecar only
/// knows `"consistency"` (and a handful of similar nouns); the
/// new detector returns free-form evidence but not a topic.
/// We pin the topic to `"consistency"` to keep the wire form
/// stable for downstream consumers.
fn pair_topic(_f: &ContradictionFinding) -> String {
    "consistency".to_owned()
}
```

The function:
- takes `&ContradictionFinding` (line 188),
- never names `f` in the body (line 189 just allocates a literal),
- returns `String` (not `&'static str`),
- is a free function at module scope (no `pub`, no `Self::`, lives
  outside the `impl DiscoverContradictPhase` block).

The `_f` underscore prefix confirms the parameter is intentionally
unused — it is not a placeholder for a future argument.

## 2. Confirmed — call site at line 174

Exact code at `src/phases/discover_contradict.rs:167-179`:

```rust
        findings
            .iter()
            .map(|f| Contradiction {
                id: String::new(),
                cluster_a: cluster_a.to_owned(),
                cluster_b: cluster_b.to_owned(),
                representatives: representatives.to_vec(),
                topic: pair_topic(f),
                description: f.evidence.clone(),
                severity: f.severity.legacy_label().to_owned(),
                schema_version: "v1".into(),
            })
            .collect()
```

The receiving struct is `Contradiction` (`src/domain/mod.rs:766`),
and the field at `src/domain/mod.rs:777` is `pub topic: String`. The
field is heap-owned `String`, not a borrow — so neither option in
the issue avoids a heap allocation per finding (see §5).

Note that line 161 already hardcodes `"consistency".into()` for the
empty-findings row (no `pair_topic()` call there), so the constant
form is internally consistent with the rest of the function. If
option A is applied, the empty-findings branch is *already* the
target shape — line 161 should be harmonised to `PAIR_TOPIC.to_owned()`
so the value is spelled exactly one way in the file.

## 3. Caller inventory

`rg '\bpair_topic\b' src/ tests/'`:

```
src/phases/discover_contradict.rs:174:                topic: pair_topic(f),
src/phases/discover_contradict.rs:188:fn pair_topic(_f: &ContradictionFinding) -> String {
```

Exactly **one** production call site and **zero** test call sites.
No integration test, no other module references `pair_topic` directly.

## 4. Similar patterns elsewhere

`rg 'fn \w+_\w+\(_\w+: &?' src/'` returns 4 matches:

| File:line | Function | Status |
|---|---|---|
| `src/phases/discover_contradict.rs:188` | `fn pair_topic(_f: &ContradictionFinding) -> String` | The issue's target |
| `src/sandbox/seccomp.rs:153` | `fn apply_for_target(_kind: SeccompPolicyKind) -> crate::error::Result<()>` | Real implementation, not a stub — just `cfg(not(unix))`. The `match` body at line 141 has only two arms and `kind` *is* read; the `_kind` here is the cfg-not(unix) sibling. Out of scope. |
| `src/llm/response_format_opt_out.rs:110` | `pub fn render_system_prompt_with_prefix(_role: &Role, model: &str, base_prompt: &str) -> String` | Comment at lines 105-109 explicitly says the `role` parameter is reserved for future per-role adjustments. **Not** a constant wrapper — the body actually reads `model` and `base_prompt`. Out of scope. |
| `src/llm/openai_compatible.rs:815` | `fn provider_with_model(_kind: &str, endpoint: &str, model: &str) -> OpenAICompatibleProvider` | Test-only helper inside `mod tests`. The `_kind` argument is unused *by design* because the tests vary endpoint+model but not kind. Out of scope — this is a deliberate test-API shape, not a hygiene smell. |

**No related dead code.** `ContradictionRefinement` (deleted by #781
in v0.14.10) is gone, and the nearby `ClusterRefinement`
(`src/phases/discover_cluster.rs:42`) is a *live* struct — it is the
unrelated `discover_cluster` phase's actual refinement struct, not
contradiction-related. Issue #790 doesn't miss any sibling cleanup.

## 5. Allocation analysis

The issue's framing of option A as "removes per-call `String`
allocation (but `.to_owned()` still allocates at the call site — is
that OK?)" deserves a direct answer.

**Option A (`PAIR_TOPIC.to_owned()`) still allocates.** The
receiving field is `String`, so `.to_owned()` heap-allocates a fresh
`String` per finding, exactly like the current
`"consistency".to_owned()` inside `pair_topic`. The compiler may be
able to optimise repeated `.to_owned()` of the same `&'static str`
to a single allocation if the calls share a call site that LLVM can
collapse, but there is **no language guarantee** of that and no
production benchmark establishes the savings. In practice: identical
allocation cost, ~6 lines of code removed from the function, call
site reads `topic: PAIR_TOPIC.to_owned()` instead of
`topic: pair_topic(f)`.

**Option B (`fn pair_topic() -> String` returning
`"consistency".to_owned()`) has the same allocation cost** as today.
It drops the misleading parameter (the right move on the
discoverability axis), but does nothing for allocation.

**Allocation-wise the two options are equivalent.** Allocation
reduction would require changing the field type to
`Cow<'static, str>` or `&'static str`, which is a separate,
much-larger refactor (touches `discover_summary.rs:457,467,470` and
the JSON serialisation contract in the wire schema). Out of scope
for a hygiene issue.

## 6. Test impact

No test asserts the value `"consistency"` produced by `pair_topic`
specifically. The only assertions that touch the topic value are:

| File:line | What it does |
|---|---|
| `tests/integration_discovery.rs:666` | Constructs `Contradiction { ..., topic: "consistency".into(), ... }` as a fixture for an unrelated test (`discovery_*` checkpoint). The fixture literal `"consistency"` is not coupled to `pair_topic` — both will keep producing the same string. |
| `src/discovery/contradiction.rs:387` | `ContradictionRecord { ..., topic: "consistency".into(), ... }` inside `contradiction_record_round_trips`. Tests the **wire form**, not the production producer. Independent of `pair_topic`. |
| `src/phases/discover_summary.rs:880` | Same — fixture literal in a test for the summary phase. Independent of `pair_topic`. |
| `src/domain/mod.rs:2070` | Same — fixture literal in a domain-level test. Independent of `pair_topic`. |

The two in-module tests in `src/phases/discover_contradict.rs`
(`into_contradictions_empty_findings_yields_low_row`,
`into_contradictions_maps_findings_to_rows`) do **not** assert the
`topic` field — they only check `severity` and `description`. They
will continue to pass under either fix shape.

**Conclusion:** no test changes are required. The validator
recommendation "a discovery smoke run, or the existing unit tests in
the same file" is technically accurate but the existing unit tests
do not exercise `pair_topic`'s output. A smoke run is the only way
to verify the field stays `"consistency"` after the refactor.

## 7. Concerns / corrections

1. **Allocation framing is wrong.** The issue says option A
   "removes the per-call `String` allocation (but `.to_owned()` still
   allocates at the call site — is that OK?)". The framing implies
   there is an allocation choice. There isn't — the field type is
   `String`, so any caller-side `.to_owned()` allocates the same
   bytes. Option A's gain is on the discoverability axis (the call
   site reads `PAIR_TOPIC`, which is greppable and obvious), not
   the allocation axis. The PR commit message should not claim an
   allocation reduction.

2. **Line 161 already hardcodes the value.** The empty-findings
   branch (`src/phases/discover_contradict.rs:161`) writes
   `topic: "consistency".into()` directly without going through
   `pair_topic`. After option A, the constant `PAIR_TOPIC` should
   be used at line 161 too (otherwise the code has the same value
   spelled two different ways). Option A's diff should be 3 edits
   not 2 — both call sites (line 161 and line 174) updated to
   `PAIR_TOPIC.to_owned()`, and the empty-findings branch's
   `"consistency".into()` should become `PAIR_TOPIC.to_owned()` for
   consistency. (`.into()` from `&str` and `.to_owned()` produce
   equivalent `String`s; the stylistic choice is the project's,
   not material.)

3. **No test asserts the value, so the validator's "existing unit
   tests" recommendation is incomplete.** A smoke run is the only
   end-to-end check that `contradictions.json` still carries
   `"topic": "consistency"` on every row. The two in-module tests
   exercise the surrounding `into_contradictions` machinery but
   not the topic value.

4. **No `pub` on `pair_topic`.** Already confirmed — the function
   is fully private (file-scope `fn`, no `pub`, no `pub(super)`).
   No visibility issue, no API surface to worry about.

5. **No CHANGELOG mention required** — the function's behaviour is
   unchanged, only its declaration form changes. The `[Unreleased]`
   section can stay empty for this one unless the cluster convention
   demands an entry for refactors (it does not, based on the
   `CHANGELOG.md` patterns in the v0.14.8 and v0.14.9 clusters).

## 8. Recommendation

**Approve option A** as recommended by the issue, with these
corrections to the commit body / cluster notes:

- Do not claim an allocation reduction. The field is `String`; both
  options allocate per finding.
- Update **both** call sites (line 161 and line 174) to use
  `PAIR_TOPIC.to_owned()`. Replace the line 161 `"consistency".into()`
  with `PAIR_TOPIC.to_owned()` so the constant has one home.
- Diff is small: 1 const declaration at module top, 1 function
  deletion at the bottom, 2 call-site updates. ~6 LOC net.
- Verify the wire form with a discovery smoke run; the in-module
  tests do not exercise the topic value.

**Why A beats B**, despite identical allocation cost:
- A makes the constant visible at every call site (greppable).
- A eliminates the misleading `_f` parameter (the actual hygiene
  complaint).
- A's diff is purely subtractive (function removed); B keeps the
  function as a no-arg wrapper, which is structurally still a
  function that adds nothing.
- If a future detector grows real topic detection, reintroducing a
  function is one line of code (`fn pair_topic(f: &ContradictionFinding) -> String { ... }`)
  and the call sites already say `PAIR_TOPIC` — the diff to swap
  `PAIR_TOPIC` for a function call would be obvious.

**Verdict: APPROVE** — option A with the two call sites harmonised.
