# Validation report — issue #789 (consolidate RunId parse sites)

> Issue: #789 — `refactor(cli): consolidate the six duplicated RunId CLI parse sites behind one helper`
> Cluster: post-v0.14.10 hygiene (v0.14.11)
> Current HEAD on `main`: `ce524e21ee0028da18384092e60925c69e5172c8`
> Labels: `tech-debt`, `priority:P3`, `size:XS`

## TL;DR

The duplication described in the issue is **real**, but the issue's enumeration of "six sites" is **inaccurate on three counts**:

1. **There are SEVEN `.parse()` sites in `telemetry_cmd.rs`, not six.** The issue misses the second `list` parse at lines 413-416 (the multi-row loop, with the deliberate `"bad run row"` message).
2. **Every line range in the issue is wrong.** The actual `.parse()` bodies are at 385-387, 413-416, 514-516, 663-665, 666-668, 997-999, 1675-1680. The line ranges the issue cites (402-404, 531-533, 680-685, 1014-1016, 1689-1694) all point at `debug!` macros or function bodies — not the parse sites. The issue was clearly written against a stale tree.
3. **The `compare` block contains TWO adjacent `.parse()` sites**, not one — `run_a` at 663-665 and `run_b` at 666-668. The issue collapses them into a single "680-685" range.

The issue's claim that the helper's callers are at `src/cli/diff.rs:125-126` is also **off by one** — the actual callers are at 124-125 and the helper definition is at 219 (the issue says :220).

**Consolidation recommendation: real, but the issue undercounts by one and needs a context-parameter decision.** Six of the seven telemetry sites use the same `"invalid run id '<raw>': {e}"` template (which matches `parse_run_id`'s body in `diff.rs`), but **site #2 at 413-416 deliberately uses `"bad run row: {e}"`** because the input there is a UUID string read out of an index row, not a value that came off the CLI flag. Routing that site through a single-message helper would *degrade* the error message and lie to the operator about the source of the value.

The six existing `cmd.dispatch()` unit tests at `src/cli/telemetry_cmd.rs:2040-2120` assert only the error **variant** (`matches!(err, Error::InvalidArgs(_))`); they do **not** assert the message text. So the refactor has more freedom than the issue suggests — message-text preservation is a stated goal of the issue but is not mechanically enforced by the tests. **This means a single-context helper *could* be rolled out without test churn, but only if the issue's expectation about preserving the differentiated `"bad run row"` message is honored** — and that requires a small design change (either a context parameter or routing the `bad run row` site to its own helper / keeping it inline).

No `pub(crate)` cross-module callers of `parse_run_id` exist outside `diff.rs`. No test references it. Relocating the helper is safe from the visibility side.

## 1. Confirmed — `diff.rs` helper

**Definition (`src/cli/diff.rs:214-229`):**

```rust
/// Parse a `String` coming off the CLI into a [`RunId`]. The error
/// variant is [`Error::InvalidArgs`] so a typo yields exit code 2 —
/// the same code used for the missing-file case in
/// `moagan validate`, giving operators one consistent failure
/// surface across both pre-flight commands.
pub(crate) fn parse_run_id(s: &str) -> Result<RunId> {
    trace!(raw = s, "parse_run_id: enter");
    let res = s
        .parse()
        .map_err(|e| Error::InvalidArgs(format!("invalid run id '{s}': {e}")));
    match &res {
        Ok(id) => debug!(run_id = %id, "parse_run_id: ok"),
        Err(e) => warn!(error = %e, "parse_run_id: error"),
    }
    res
}
```

- Visibility: `pub(crate)` — confirmed.
- Return type: `Result<RunId>` (this crate's `Result` alias for `Result<T, Error>`); `Error::InvalidArgs` is mapped in the body.
- Error message template: `"invalid run id '{s}': {e}"`.

**Callers (the issue says `diff.rs:125-126`, actual lines 124-125):**

```rust
let a = parse_run_id(&run_a)?;
let b = parse_run_id(&run_b)?;
```

Both live in `pub fn run(args: DiffArgs) -> Result<i32>` at `src/cli/diff.rs:103`. No usage anywhere else — `rg 'parse_run_id' src/` returns only this definition and its two intra-module callers. Confirmed the issue's premise that the helper is currently a "self-only" indirection.

## 2. The seven telemetry sites — per-site analysis

The issue claims six sites at:

| Issue line range | Claimed subcommand | What's actually at those lines |
|---|---|---|
| 402-404 | list | end of `print_one_run(...)` call (no parse) |
| 531-533 | summary | `debug!(...)` macro body |
| 680-685 | compare | `debug!(...)` macro body |
| 1014-1016 | export | `debug!(...)` macro body |
| 1689-1694 | cost | start of `fn print_for_run(...)` (no parse) |

**The actual `.parse()` sites are at different lines and one extra site exists.** Full enumeration:

| # | Subcommand | Lines | Parse target | Error message template |
|---|---|---|---|---|
| 1 | list (drill-into-run) | 385-387 | `raw: &str` (from CLI `--run`) | `invalid run id '{raw}': {e}` |
| 2 | **list (multi-row loop) — MISSED BY ISSUE** | 413-416 | `row.run_id: String` (from DB row) | `bad run row: {e}` ← **DIFFERENT** |
| 3 | summary | 514-516 | `run: &str` (from CLI `--run`) | `invalid run id '{run}': {e}` |
| 4 | compare (run_a) | 663-665 | `run_a: &str` (from CLI `--run-a`) | `invalid run id '{run_a}': {e}` |
| 5 | compare (run_b) | 666-668 | `run_b: &str` (from CLI `--run-b`) | `invalid run id '{run_b}': {e}` |
| 6 | export | 997-999 | `run: &str` (from CLI `--run`) | `invalid run id '{run}': {e}` |
| 7 | cost | 1675-1680 | `raw: &str` (from `Option<String>` mapped) | `invalid run id '{raw}': {e}` |

**Important:** Sites 1, 3, 4, 5, 6, 7 all use the **same** template `invalid run id '<value>': {e}` — just with the local variable name interpolated. Site #2 deliberately diverges.

**Site #2 verbatim (`src/cli/telemetry_cmd.rs:412-416`):**

```rust
for row in &rows {
    let run_id: RunId = row
        .run_id
        .parse()
        .map_err(|e| Error::InvalidArgs(format!("bad run row: {e}")))?;
```

The message `"bad run row"` is **intentional**: the value being parsed is a UUID string that was read out of the `runs` index (`row.run_id: String` on the `RunRow` struct), not a value the operator typed on the CLI. The error class is still `Error::InvalidArgs` because a corrupted index row *is* an operator-facing condition (it would prevent the table from being listed), but the message correctly attributes the failure to the index, not to the user's input.

**Pre-processing:** none of the seven sites does `.strip_prefix("run_")`, `.trim()`, or any other pre-processing on the input. The string goes straight into `.parse()` (which is `RunId`'s `FromStr` impl, ultimately delegating to `Uuid::parse_str`).

## 3. Additional sites the issue missed (out of cluster scope)

- **In `src/cli/mod.rs`:** 6 more inline parse sites use a slightly different shape — `format!("{e}")` only, dropping the `invalid run id '<raw>'` wrapper. They are NOT captured by the issue's "five inline + one factored" framing and would require either a context parameter on the helper or staying inline.
- **In `src/cli/audit.rs`:** 1 hand-rolled parse at line 75.
- **In `src/cli/coverage_cmd.rs`:** 1 hand-rolled parse at line 67.
- **In `src/cli/rate.rs:26-27`:** another hand-rolled RunId parse with the same shape but a **different** message:

  ```rust
  let run_id = RunId::from_str(&args.run_id)
      .map_err(|e| Error::InvalidArgs(format!("invalid run_id '{}': {e}", args.run_id)))?;
  ```

  Three small differences vs the canonical `parse_run_id` shape:

  1. Method form (`RunId::from_str(...)` vs `s.parse::<RunId>()`).
  2. Message format string uses `"invalid run_id"` (underscore, no space) vs `"invalid run id"` (space, no underscore).
  3. The raw value is referenced as `args.run_id` instead of being captured into a local first; the error message embeds `args.run_id` directly.

  This site is not in the issue's scope. If the consolidation extends to it, the message format divergence will need a separate decision — the existing `rate` tests (if any) would need to be checked.

- **In `src/telemetry/dashboard.rs`:** 2 sites at lines 326 and 369 use `format!("invalid run id '{raw}': {e}")` (HTTP-API variant).

- **In `src/ids.rs`:** the canonical `RunId::from_str` impl at lines 56-65 — this is what the inline `.parse()` calls actually delegate to. No consolidation can replace this; it is the parse implementation, not a CLI-shaped wrapper.

**Total count: 17 sites in `src/cli/` + 2 in `src/telemetry/dashboard.rs` (out of scope per issue).**

## 4. Error-message uniformity check (in-scope sites)

| Site | Message template | Variable substituted |
|---|---|---|
| `diff.rs:219-229` (`parse_run_id`) | `invalid run id '{s}': {e}` | `s` |
| telemetry 385-387 (list drill) | `invalid run id '{raw}': {e}` | `raw` |
| **telemetry 413-416 (list loop)** | **`bad run row: {e}`** | **none (no value embedded)** |
| telemetry 514-516 (summary) | `invalid run id '{run}': {e}` | `run` |
| telemetry 663-665 (compare run_a) | `invalid run id '{run_a}': {e}` | `run_a` |
| telemetry 666-668 (compare run_b) | `invalid run id '{run_b}': {e}` | `run_b` |
| telemetry 997-999 (export) | `invalid run id '{run}': {e}` | `run` |
| telemetry 1675-1680 (cost) | `invalid run id '{raw}': {e}` | `raw` |

**Findings:**

- **Six of seven telemetry sites + the diff.rs helper all use the same message template, only the interpolated variable name differs.** A single helper that takes `s: &str` and emits `invalid run id '{s}': {e}` would match all six verbatim (the variable name in the message is just `{s}` regardless of the caller's local binding — `raw`, `run`, `run_a`, `run_b` are all just the string that the operator typed, so the message text is functionally identical even if the variable name printed differs).
- **Site #2 (list loop at 413-416) deliberately deviates with `bad run row: {e}`.** This is not a uniformity issue — it is a different failure mode (corrupted DB row vs. malformed CLI input) that happens to share the `Error::InvalidArgs` variant. The issue body itself flags this risk: *"Check first whether the six sites' error messages are actually identical. If any deliberately differs … the helper needs a context parameter rather than a single fixed message."* — and the answer is **yes, one deliberately differs**, so a context parameter (or routing the divergent site to its own helper / keeping it inline) is the right design.

## 5. Unit test impact

The six tests the issue cites (lines 2054, 2066, 2079, 2090, 2101, 2113) plus the test at 2041 are all **structurally identical**:

```rust
#[test]
fn <name>_invalid_run_id_returns_invalid_args() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd = TelemetryCmd::<Variant> { /* … */ run_a/run/run: Some("not-a-uuid".into()) /* … */ };
    let err = pollster::block_on(cmd.dispatch()).unwrap_err();
    assert!(matches!(err, Error::InvalidArgs(_)));
}
```

**Asserted:** only `matches!(err, Error::InvalidArgs(_))` — the error **variant**.

**Not asserted:** the error message text. No test does `assert!(format!("{err:?}").contains(...))` or similar.

`rg 'invalid run id|bad run row' tests/` returns **zero matches** — no integration test pins the message either.

This contradicts the issue's claim that *"the six existing `cmd.dispatch()` unit tests … still assert the same error variants **and** the same message text"*. They assert only the variant. The message-text constraint is therefore a *desired* invariant the refactor must preserve, not a *tested* invariant. If the message text changes, the tests still pass.

**Implication for the refactor:**

- The `Error::InvalidArgs` variant assertion gives a safety net: any refactor that loses the variant-to-error mapping will trip a test.
- The lack of message-text assertion gives the refactor latitude to either preserve every message verbatim (preferred) or normalize them to a single form. **The issue's design preference is verbatim preservation**, and the operator-facing nature of these messages (they reach the human in their shell on failure) makes verbatim preservation the lower-risk choice.

## 6. Helper location recommendation

**Recommended: `src/cli/diff.rs`, kept as `pub(crate)`** — keep the existing location; no module reshuffle needed for the in-scope work.

**Rationale:**

- The current helper at `src/cli/diff.rs:219-229` already has the right shape. Moving it would be churn without benefit.
- The 7 in-scope sites all live in `src/cli/telemetry_cmd.rs`, which imports `crate::cli::diff::parse_run_id` via `use crate::cli::diff;` (or the module path). Visibility stays `pub(crate)` — the helper remains a CLI-internal seam.
- `src/ids.rs` would also work, but the helper is a CLI-shaped policy (picks the error variant + message template) rather than a `RunId` primitive. The `FromStr` impl on `RunId` itself is the public parse surface; the CLI helper is policy that wraps it.
- A future follow-up that wants to consolidate the 9 out-of-scope sites (`cli/mod.rs` ×6, `cli/audit.rs`, `cli/coverage_cmd.rs`, `cli/rate.rs`, `telemetry/dashboard.rs` ×2) is a separate decision and may justify moving the helper to `src/ids.rs` with a context parameter. That decision is out of scope here.

**Visibility:**

- **Keep `pub(crate)`.** The helper is internal-implementation; `RunId` itself stays `pub`, and its `FromStr` impl remains the public parse surface. The CLI helper exists only because the project maps the parse error to a specific error variant and message for CLI use. Making it `pub` would expose a CLI-shaped policy in the public API.

**Signature (recommended for this cluster):**

```rust
// Stays in src/cli/diff.rs
pub(crate) fn parse_run_id(s: &str) -> Result<RunId> {
    s.parse().map_err(|e| Error::InvalidArgs(format!("invalid run id '{s}': {e}")))
}
```

The "bad run row" site is **left inline** — it is a single call, the message is intentionally different, and routing it through the helper would either degrade the message or require a context parameter that adds noise without benefit.

## 7. Concerns / corrections

**Corrections to the issue body:**

1. **Line ranges are all wrong.** The issue cites `402-404`, `531-533`, `680-685`, `1014-1016`, `1689-1694`. The actual `.parse()` blocks are at `385-387`, `413-416`, `514-516`, `663-665` + `666-668`, `997-999`, `1675-1680`. A rewrite from the live tree will not find the code the issue describes.
2. **There are seven parse sites in `telemetry_cmd.rs`, not six.** The list subcommand has *two* parse sites: the single-run drill-into (385-387) and the multi-row loop (413-416). The issue captures only the first.
3. **The `compare` block contains two adjacent `.parse()` calls.** It is one block but two sites, so the consolidated helper is called twice inside `compare::run`.
4. **The helper's callers in `diff.rs` are at 124-125, not 125-126.** The helper definition is at line 219, not 220. Minor, but the issue body quotes specific line numbers — they need to be re-verified before the fix lands.
5. **The unit tests assert only the error variant, not the message text.** The issue states the opposite. The message-text preservation is a real requirement for operator UX but is not mechanically enforced by tests.
6. **`src/cli/rate.rs:26` and 9 other sites are sibling hand-rolled parses** with different message shapes. Out of scope for this issue — they would need a follow-up that reconciles the message shapes.

**Concerns about the proposed consolidation:**

1. **The `bad run row` site at 413-416 has a different message and a different *meaning*.** Consolidating it through a single-message helper would either degrade the message (`bad run row` → `invalid run id '<db value>'`) or require a context parameter. Neither is fatal, but the issue body already warned about this and the warning is correct — the consolidation must acknowledge it.
2. **The helper carries tracing calls** (`trace!` on enter, `debug!` on ok, `warn!` on error) that are useful for debugging CLI parse failures. If we drop the tracing during consolidation (the issue suggests doing so implicitly), we lose debuggability for the 7 in-scope sites. **Recommendation:** keep the tracing calls. The tracing-target scope is `parse_run_id` and the events are sparse.
3. **No tests break.** `rg 'parse_run_id' tests/` returns zero matches; no integration test pins the exact error message. The refactor is invisible to the test suite beyond the variant-preservation guarantee.
4. **The 9 out-of-scope sites are a soft blocker for "no remaining hand-rolled RunId parse in the CLI layer".** If the issue's stated validation step (`rg 'parse\(\)\s*$|\.parse\(\)' src/cli/`) is taken literally, all 9 sites will appear and need to be folded in. For this cluster, **scope-limit to the 7 sites in `telemetry_cmd.rs`** and file the rest as a follow-up. The validation grep must be relaxed to `rg '\.parse\(\)' src/cli/telemetry_cmd.rs`.

## 8. Effort estimate

The issue estimates ~1 hour / 7 sites / ~30 LOC net reduction. Re-estimated against the actual tree:

- Update the issue body / commit message line references: 10 min
- Update the helper body in `src/cli/diff.rs:219-229` (keep tracing calls): 5 min
- Replace the 6 same-message sites (telemetry 385, 514, 663, 666, 997, 1675): 15 min
- Leave the `bad run row` site at 413-416 inline (with a comment explaining why): 2 min
- `make fmt-check guard-deps lint build test-ci` + manual smoke of one subcommand: 15 min
- Update `CHANGELOG.md` if the project keeps that discipline for refactors: 5 min

**Realistic estimate: 1.0-1.5 hours**, matches the issue's `size:XS` label after scope-limiting to `telemetry_cmd.rs`. **If expanded to the 9 out-of-scope sites**, the work grows to ~2.5-3 hours and warrants a size relabel from XS to S. Recommend scope-limiting for this cluster.

No blockers. The refactor is safe and matches the issue's intent. The issue body itself flagged the right concern (message uniformity) and the answer is *almost-uniform-with-one-deliberate-divergence*, which the helper can either ignore (route the divergent site to a sibling helper) or absorb (add a context parameter).
