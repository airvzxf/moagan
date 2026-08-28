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
> [`docs/proposal-02-rust.md` T01-06 §0.5](../proposal-02-rust.md),
> [`docs/proposal-03-add-ons.md` §D.7, §D.14.23, §D.18.2](../proposal-03-add-ons.md),
> [`AGENTS.md` §"No-go list"](../../AGENTS.md).

## Context

`AGENTS.md` carries a blanket **no-go list** of crates that may not
appear in `Cargo.toml`. Three of those entries — `comfy-table`,
`proptest`, and `petgraph` — have been on the list since v0.5 but the
list itself has never been ratified by an explicit decision document;
the prior `docs/pending-items-2026-08-13.md` had flagged this gap
as Tier B #19 (the doc itself was retired in the 2026-08-28 docs
prune; the resolution lives in this ADR).

`docs/proposal-03-add-ons.md` (the additive patch catalogue) **does**
list each of the three crates with a target version, a target use case,
and a normative reference:

| Crate        | Pinned version   | Target use                                            | Catalogue ref                                 |
|--------------|------------------|-------------------------------------------------------|-----------------------------------------------|
| `petgraph`   | `0.6` + `serde`  | DAG of phases (optional, only `deep` mode)            | §D.2 (T18-06 §0; T16-09 §7.2; T07-10 §1217) |
| `comfy-table`| `7.1`            | Pretty-printed CLI tables (`inspect`, `telemetry …`) | §D.14.23 (T12-09 §6.2; T20-06 §6.3)          |
| `proptest`   | `1.4`            | Property-based testing for hashes and serialization   | §D.18.2 (T00-05 D20)                          |

The catalogue ships three different access patterns:

- **`petgraph`** is **optional**: T01-06 keeps `phases/` as a `Vec`
  by default; `petgraph` is only the backend for `DagNode` when
  `--mode deep` is selected (T01-06 §3.5).
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
  pull `petgraph`; the linear `phases/` vector from T01-06 stays the
  default path (§D.2 in `proposal-03-add-ons.md`).
- **Rationale**: T01-06 §3.5 already sketches the
  `DagNode` trait that wraps `petgraph`. Default-off keeps the
  release binary footprint unchanged and the no-go rule
  retro-compatible (no crate is added unless the operator opts in).
- **Cost**: ~3 days (§D.2 catalogue estimate). Value: real DAG
  semantics, parallel-eligible phases, easier visualisation.

### D-2 — `comfy-table 7.1` → **DEFER to v0.9**

- **Status**: stays on the no-go list for the entire v0.8 cycle.
- **No code change** in this PR. Plain-text tables continue to ship
  for `moagan inspect`, `moagan telemetry provider`, and
  `moagan telemetry plan`. `insta` snapshots already cover
  regressions of those tables.
- **Rationale**: cosmetic, not blocking. The catalogue estimate
  (D.14.23) is 1 day, but the marginal UX gain over the current
  text output is small and any breakage in the table renderer
  ripples into snapshot churn across the snapshot suite.
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

The blanket prohibition rule is **kept** for the eight crates that
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

- **`petgraph`**: the normative spec (T01-06 §3.5) sketches the
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

## Appendix B — Cross-references back to the catalogue

- `petgraph`: `proposal-03-add-ons.md §D.2` (T18-06 §0,
  T16-09 §7.2, T07-10 §1217). The §D.2 crates table specifies
  *"petgraph se usa como opcional; por defecto T01-06 mantiene su
  phases/ vector (DAG solo en deep)"*.
- `comfy-table`: `proposal-03-add-ons.md §D.14.23`
  (T12-09 §6.2; T20-06 §6.3).
- `proptest`: `proposal-03-add-ons.md §D.18.2` (T00-05 D20).
  Pre-existing rationale note in `proposal-02-rust.md:1647-1649`:
  *"proptest no se añade como dep — los invariantes de la
  perturbación (clip, monotonicidad de sigma, fracciones suman 1.0)
  están cubiertos por tests unitarios con seeds fijos."* — this ADR
  retires that note in favour of the property-based harness for the
  same invariants.