# ADR 0001 — Differentiated policy for the no-go list

> **Status**: Accepted
> **Date**: 2026-08-16
> **Deciders**: `airvzxf/moagan` operator + Phase A subagent (session-5)
> **Supersedes**: implicit three-forbidden-crates rule in
> [`AGENTS.md`](../../AGENTS.md) (the prior
> `docs/pending-items-2026-08-13.md §6.2` / §11 B#19 traces of the
> policy gap were retired in the 2026-08-28 docs prune; their
> resolution lives here).
> **Relates to**:
> [`AGENTS.md` §"No-go list"](../../AGENTS.md),
> [`AGENTS.md` §"Differentiated allow-list"](../../AGENTS.md),
> [`scripts/check-no-forbidden-crates.sh`](../../scripts/check-no-forbidden-crates.sh).

## Context

`AGENTS.md` carries a blanket **no-go list** of crates that may not
appear in `Cargo.toml`. Three of those entries — `comfy-table`,
`proptest`, and `petgraph` — have been on the list since v0.5 but the
list itself has never been ratified by an explicit decision document;
the prior `docs/pending-items-2026-08-13.md` had flagged this gap
as Tier B #19 (the doc itself was retired in the 2026-08-28 docs
prune; the resolution lives in this ADR).

The three crates were each documented with a target version, a
target use case, and a normative reference in the (now-retired) patch
catalogue. The current authoritative sources for each verdict are
listed below in the *Decision* section (`Cargo.toml` for the
configurations, `scripts/check-no-forbidden-crates.sh` for the
enforcement). Summary:

| Crate        | Pinned version   | Target use                                            | Current source                                |
|--------------|------------------|-------------------------------------------------------|-----------------------------------------------|
| `petgraph`   | `0.6` + `serde`  | DAG of phases (optional, only `deep` mode)            | `Cargo.toml` `[dependencies]` (optional)       |
| `comfy-table`| `7.1`            | Pretty-printed CLI tables (`inspect`, `telemetry …`) | nowhere (forbidden; deferred to v0.9+)       |
| `proptest`   | `1.4`            | Property-based testing for hashes and serialization   | `Cargo.toml` `[dev-dependencies]`             |

The catalogue ships three different access patterns:

- **`petgraph`** is **optional**: T01-06 keeps `phases/` as a `Vec`
  by default; `petgraph` is only the backend for `DagNode` when
  `--mode deep` is selected (T01-06 §0.5 row 18).
- **`comfy-table`** is **cosmetic**: text-mode tables work today and
  the catalogue notes (D.14.23) that the dependency is a polish item,
  not a functional requirement.
- **`proptest`** is **test-only**: it would only ever appear under
  `[dev-dependencies]` and would not enter the release binary.

Three different risk profiles. A blanket prohibition on all three is
**over-restrictive** for `petgraph` (we already have a `DagNode` trait
sketched in the normative spec) and **`proptest`** (the dev-deps
section does not touch the release binary), but **correct** for
`comfy-table` (no functional value, deferred to v0.9).

This ADR formalises the policy. It is the first ADR in this repo
(`docs/adr/` did not exist prior to this decision) and sets the
template that future ADRs will follow.

## Decision

The three crates receive **differentiated verdicts** with explicit
guard-rails. Each verdict is enforceable by a CI guard in
`scripts/check-no-forbidden-crates.sh`.

### D-1 — `petgraph 0.6` (with `serde`) → **ADMIT, gated**

- **Status**: ADMITTED, behind a Cargo feature flag `dag`.
- **Access pattern**:
  ```toml
  [dependencies]
  petgraph = { version = "0.6", optional = true, default-features = false, features = ["serde"] }
  ```
- **Activation**: only when `--features dag` is passed AND
  `--mode deep` is selected at runtime.
- **Default build** (`cargo build` with no features) does **not**
  pull `petgraph`; the linear `phases/` vector in `src/phases/`
  stays the default path.
- **Rationale**: T01-06 §0.5 row 18 already sketches the
  `DagNode` trait that wraps `petgraph`. Default-off keeps the
  release binary footprint unchanged and the no-go rule
  retro-compatible (no crate is added unless the operator opts in).
- **Cost**: ~3 days (§D.2 catalogue estimate). Value: real DAG
  semantics, parallel-eligible phases, easier visualisation.

### D-2 — `comfy-table 7.1` → **DEFER to v0.9**

- **Status**: stays on the no-go list for the entire v0.8 cycle.
- **No code change** in this PR. Plain-text tables continue to ship
  for `moagan inspect`, `moagan telemetry provider`, and
  `moagan telemetry plan`. There is no `insta` snapshot suite
  in this repo (tests use plain `assert!` / integration scripts),
  so any table-renderer regression would surface as a CLI exit
  code or assertion failure, not a snapshot diff.
- **Rationale**: cosmetic, not blocking. The catalogue estimate
  (D.14.23) is 1 day, but the marginal UX gain over the current
  text output is small. The deferral window has slipped from
  the original "v0.9" target through v0.12 without a re-review;
  a follow-up ADR should formalise the current decision window
  (e.g. "v0.13+ or until a UX-blocking issue is filed").
- **Re-review trigger**: any PR that proposes to add `comfy-table`
  in v0.8 must first amend this ADR with a *Re-evaluation* section
  and re-vote the decision (no silent relaxation).

### D-3 — `proptest 1.4` → **ADMIT in `[dev-dependencies]` only**

- **Status**: ADMITTED in the `[dev-dependencies]` table; remains
  forbidden in `[dependencies]`.
- **Access pattern**:
  ```toml
  [dev-dependencies]
  proptest = "1.4"
  ```
- **Effect**: the crate does **not** enter the release binary; CI
  guard enforces the section split.
- **Rationale**: `proptest` is uniquely valuable for invariants of
  pure functions (hash determinism, serde round-trips, ID generation
  entropy). The catalogue example at D.18.2 uses it to verify
  `CallKey::hash` determinism. Such tests are inherently
  property-based and writing them by hand (as
  `src/ranking/stability.rs:461-466` does today for the monotonicity
  invariants) is brittle and coverage-bounded by the chosen seeds.
- **Coverage targets**: properties for `CallKey::hash`,
  `RunId` UUID-v7 monotonicity, `blake3` short-input stability,
  and `serde_json` round-trips of the public domain types. Other
  invariants stay on hand-written `#[test]` until they actually
  need a property-based harness.
- **Cost**: ~2 days. Value: catches corner cases in serialisation
  and ID generation that the hand-written test grid misses.

## Consequences

### Positive

- **No-go list is no longer aspirational**: each entry now maps to
  either an explicit allow-with-guard or an explicit deferral. The
  policy is enforceable by `scripts/check-no-forbidden-crates.sh`
  with no ambiguity.
- **`docs/adr/` exists** as a directory; future ADRs will follow
  the same template (Status / Date / Deciders / Context / Decision /
  Consequences / Re-evaluation).
- The catalogue patches (§D.2, §D.14.23, §D.18.2) can be opened as
  feature work in the order this ADR recommends: `proptest` first
  (dev-deps, lowest risk), `petgraph` second (gated, default-off),
  `comfy-table` last (post-v0.9).
- Tier B #19 (`pending-items §11`) is now fully resolved.

### Negative / accepted risks

- **`petgraph` default-off gate requires operator discipline.** If
  someone enables the `dag` feature without an opt-in ADR amendment
  the release binary picks up a new transitive-dep tree
  (`petgraph` pulls `fixedbitset`, `ahash`, `indexmap`). The guard
  script checks the `[dependencies]` block for the literal `petgraph`
  string regardless of feature gating; if the ADR is followed the
  only valid appearance is `optional = true`, which the guard treats
  as allowed.
- **`proptest` dev-deps section grows the test compile time.**
  Roughly +5–10 s on a cold cache (measure before/after). Acceptable
  given the gain in invariant coverage.
- **`comfy-table` deferral means the text-mode tables in `inspect`
  and `telemetry …` stay ugly.** Accepted; cosmetic.

### Compliance

After this ADR lands:

| Crate        | Where it may appear                          | Enforced by                                                                                |
|--------------|----------------------------------------------|--------------------------------------------------------------------------------------------|
| `petgraph`   | `[dependencies]` with `optional = true` only  | `scripts/check-no-forbidden-crates.sh` (allows `optional = true`, rejects plain `=`)      |
| `comfy-table`| **Nowhere in `Cargo.toml`**                  | `scripts/check-no-forbidden-crates.sh` (added to the forbidden list, blanket rejection)    |
| `proptest`   | `[dev-dependencies]` only                    | `scripts/check-no-forbidden-crates.sh` (section-aware check, rejects `[dependencies]` row)  |
| `secrecy`    | nowhere                                      | unchanged (already in forbidden list)                                                      |
| `axum`, `hyper`, `sqlx`, `governor`, `figment`, `refinery`, `askama`, `handlebars`, `lettre`, `inquire`, `time` | nowhere | unchanged (already in forbidden list) |

The blanket prohibition rule is **kept** for the thirteen crates that
stay forbidden (`comfy-table`, `secrecy`, `axum`, `hyper`, `sqlx`,
`governor`, `figment`, `refinery`, `askama`, `handlebars`, `lettre`,
`inquire`, `time`). The differentiation in this ADR only opens the
two specific allow-listed cases.

## Re-evaluation

This ADR will be revisited when any of the following happen:

1. A v0.9 cycle opens and `comfy-table` is reconsidered.
2. `petgraph` graduates from "optional" to "default-on" — that
   requires a follow-up ADR and a smoke-gate run on a Linux runner
   that compares binary sizes before/after.
3. The test-suite compile-time budget exceeds 90 s on cold cache,
   and `proptest` is the suspected cause. Mitigation: move the
   property tests behind a `#[cfg(feature = "proptest-tests")]`
   feature that CI enables but local dev does not need to.

Until then, the verdicts above are authoritative.

---

## Appendix A — Why a blanket prohibition was wrong

A blanket prohibition would have foreclosed legitimate uses:

- **`petgraph`**: the normative spec (T01-06 §0.5 row 18) sketches the
  `DagNode` trait as a wrapper over a DAG backend. The wrapper is
  meaningless without *some* DAG library, and `petgraph` is the only
  credible candidate for this codebase (small, `serde`-aware, no
  async runtime). Forbidding it would force the spec to be rewritten
  or the trait to stay a stub.
- **`proptest`**: the hand-written seed grid in
  `src/ranking/stability.rs:461-470` is a workaround, not a strategy.
  The comment in that file says so explicitly:
  *"Phase H originally intended proptest for the monotonicity
  invariants … the brief's rule about following existing libraries
  means we'd rather add a proptest dev-dep in a follow-up."*
  Leaving the prohibition in place makes the workaround permanent.

## Appendix B — Cross-references back to current sources

- `petgraph`: declared in `Cargo.toml` `[dependencies]` with
  `optional = true` and gated by the `dag` Cargo feature. The
  linear `phases/` vector in `src/phases/` is the default build
  path; `petgraph` only activates with `--features dag --mode deep`.
- `comfy-table`: not present anywhere in `Cargo.toml`. The blanket
  no-go list in [`AGENTS.md`](../../AGENTS.md) and the CI guard in
  `scripts/check-no-forbidden-crates.sh` are the enforcement. The
  deferral window has slipped from the original "v0.9" target; a
  future ADR should formalise the current decision window.
- `proptest`: declared in `Cargo.toml` `[dev-dependencies]` only.
  The historical rationale note that argued against `proptest`
  (covered today by hand-seeded unit tests in
  `src/ranking/stability.rs`) is retired by this ADR in favour of
  property-based harnesses for the same invariants.