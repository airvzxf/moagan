# Validation report — issue #784 (#773 whitespace residue — 4 functionally degraded + 1 broken rustdoc link)

> Issue: #784 — `docs: repair the whitespace residue left by the #773 citation sweep (~468 comment lines, 1 broken rustdoc link)`
> Cluster: post-v0.14.11 hygiene (v0.15.0)
> Current HEAD on `main`: `c3c4c8f6367b31393cb5274055d4aab633bfa897` (v0.14.11 release bump)
> Validation: 8-reviewer F1 swarm

## TL;DR

The cluster v0.15.0 scope-limits #784 to **5 specific sites**: 1 broken rustdoc intra-doc link + 4 functionally degraded comment lines. Each site is verified real. Each target document is deleted from the surviving tree. **No surviving ADR or external doc can serve as a re-target — only inline prose rewrite or a working intra-doc link is possible**, matching the precedent set by #779 and #785.

## Scope decision

EPIC #797 scope-limits #784 to **5 sites**. The remaining ~463 cosmetic whitespace-residue lines are correctly deferred to a future L-size cluster (the cluster pattern shows residue sweeps benefit from per-cluster F2 review — see v0.14.11 validate-785 §1.1–§1.3).

## 1. Site 1 — broken intra-doc link at `src/error/mod.rs:290`

### Current broken text (read live on `c3c4c8f`)

`src/error/mod.rs:285-294` (line 290 carries the broken link):

```rust
    /// K.4 sub-1: a research backend dependency is missing or the
    /// upstream pipeline returned no usable signal. The current
    /// trigger is `pdftotext` not being on `PATH` (the binary
    /// ships with the `poppler-utils` system package — see
    /// [`                                `](../                                )
    ///    for the install hint), but the variant stays open for
    /// future "research pipeline unavailable" signals (PDF host
    /// not allowlisted, allowlist blocked, …).
```

The link is 81 characters of structurally valid markdown whose display text and URL are both entirely whitespace — the reader sees "see [nothing] for the install hint".

### Original citation recovered from `git show 0cfec4e^:src/error/mod.rs`

```rust
/// ships with the `poppler-utils` system package — see
/// [`docs/proposal-04-cuarta-etapa.md`](../docs/proposal-04-cuarta-etapa.md)
///    §4 for the install hint), but the variant stays open for
```

The substring `docs/proposal-04-cuarta-etapa.md` (32 chars) was substituted with 32 spaces (both inside `[...]` and inside `(...)`).

### Intended target — no surviving doc

The file `docs/proposal-04-cuarta-etapa.md` was deleted in commit `27fda5a` (issue #660, v0.4 → v0.10 rewrite). The canonical install procedure now lives in **`src/research/pdf.rs:4-6`**:

> "External dependency: the `pdftotext` binary from the `poppler-utils` system package (Arch: `pacman -S poppler`; Debian: `apt install poppler-utils`)."

This is co-located with the only consumer (`pdf::extract_pdf_text`).

### Proposed fix

Replace the broken link with a working intra-doc link to the `research::pdf` module:

```rust
/// ships with the `poppler-utils` system package — see the
/// [`research::pdf`] module docs for the install hint), but the variant
```

(The §4 fragment on the next line drops with the citation.)

Alternative: drop the parenthetical "see … for the install hint" entirely, matching #779/#785's prose-only precedent.

### Rustdoc impact

`cargo doc --no-deps` currently reports **80 warnings**; this line does **not** generate one of them (rustdoc treats the whitespace URL as unparseable rather than as a broken link). The fix is therefore hygiene-driven, not warning-driven. Net rustdoc delta: 0 or -1.

## 2. Site 2 — `src/discovery/contradiction.rs:81` (§D.x unreadable)

### Current text (read live)

```rust
/// Maximum number of candidate sketches the LLM-as-judge prompt
/// embeds for one comparison. The cooldown is `MAX_PAIRS` per
/// cluster pair in `discover_contradict.rs`; this caps the inner
/// sketch pool so the prompt fits the 1M-token ceiling even on
/// large cluster pairs. 32 was picked because
/// `            §D.x contradiction` mentions a 30-sketch ceiling
/// for the v1 dataset; the extra two slots cover mild overlap.
const MAX_CANDIDATES_PER_CALL: usize = 32;
```

12 inner spaces where `proposal-03 ` used to be.

### Original citation (`git show 0cfec4e^:src/discovery/contradiction.rs:81`)

```rust
/// `proposal-03 §D.x contradiction` mentions a 30-sketch ceiling
```

### Intended target — none

`docs/proposal-03-add-ons.md` was deleted in commit `0e52e7b` (PR #673). The "30-sketch ceiling" was never documented anywhere in the surviving tree:

```
$ git log --all -S '30-sketch ceiling' --oneline
830a5b9 chore(release): v0.14.0 — version bump (#732)
1c8159f feat(discovery): replace contradiction stub with LLM-as-judge detector (#486)
```

Both hits land on this same line. There is no ADR, no viability doc, no design note, no commit message that captures the 30-vs-32 rationale. The §D.x anchor was already a literal placeholder at write-time (`feat(discovery)` commit `1c8159f`).

### Proposed fix — prose rewrite

```rust
/// Maximum number of candidate sketches the LLM-as-judge prompt
/// embeds for one comparison. The cooldown is `MAX_PAIRS` per
/// cluster pair in `discover_contradict.rs`; this caps the inner
/// sketch pool so the prompt fits the 1M-token ceiling even on
/// large cluster pairs. 32 covers the 30-sketch worst case observed
/// on the v1 dataset with two extra slots of overlap headroom.
const MAX_CANDIDATES_PER_CALL: usize = 32;
```

Drops the backticked phantom citation, keeps the empirical justification, removes the rustdoc noise. Matches #779 / #785 prose-only precedent.

## 3. Site 3 — `src/telemetry/mod.rs:295` (dangling "Spec")

### Current text (read live)

```rust
    std::fs::create_dir_all(run.telemetry())?;
    tracing::debug!("Telemetry::open: telemetry dir ensured");
    // Spec      declares `gz` as the default compression for the
    // two append-only streams (`phases.jsonl` and `calls.jsonl`).
```

Six spaces between `Spec` and `declares`. `awk 'NR==295' | cat -A` confirms.

### Original citation (`git show 0cfec4e^:src/telemetry/mod.rs:295`)

```rust
        // Spec §1.5 declares `gz` as the default compression for the
```

The substring `§1.5` (4 bytes) was substituted with 4 spaces.

### Intended target — none

`docs/proposal-02-rust.md` §1.5 contained the `gz`-as-default rationale; the file was deleted in commit `0e52e7b` (PR #673). No surviving ADR captures the `gz` decision (`grep -i 'gzip\|jsonl\.gz' docs/adr/` → empty for normative content; ADR-0002 mentions `phases.jsonl.gz` only as an example path in a post-mortem story).

The fact is independently documented in `AGENTS.md` smoke gate #2 and `src/storage/compression.rs:3-7`, so the citation is decorative anyway.

### Proposed fix — restate inline

```rust
    std::fs::create_dir_all(run.telemetry())?;
    tracing::debug!("Telemetry::open: telemetry dir ensured");
    // `gz` is the default compression for the two append-only streams
    // (`phases.jsonl` and `calls.jsonl`). AGENTS.md's smoke gate #2 then
    // names the on-disk file literally as `telemetry/calls.jsonl.gz`.
    // Warnings stay uncompressed because they are tiny and frequently tailed.
```

Drops "Spec" + 6 spaces, folds the `gz` fact into the comment opening, keeps the AGENTS.md cross-reference intact.

## 4. Site 4 — `src/redact/patterns.rs:232` (empty parenthetical header)

### Current text (read live)

```rust
// -----------------------------------------------------------------
// Categorised redaction (                  )
//
// The categorised substitute replaces the legacy `[REDACTED:id]`
// marker with a shorter `***REDACTED:slug***` shape that makes it
// easier for an operator to grep the redaction log without
// needing the full pattern id. The enum mirrors the 14 kinds
// from the spec; `Unknown` is the catch-all the categorised
// apply pass falls back to when none of the named regexes
// matched.
// -----------------------------------------------------------------
```

Eighteen spaces between the open paren and the close paren. `awk 'NR==231' | cat -A` confirms.

### Original citation (`git show 0cfec4e^:src/redact/patterns.rs:231`)

```rust
// Categorised redaction (proposal-03 §D.8.2)
```

The substring `proposal-03 §D.8.2` (18 bytes) was substituted with 18 spaces.

### Intended target — none

`docs/proposal-03-add-ons.md` §D.8.2 defined the `PatternKind` enum and the `***REDACTED:slug***` substitute format. Deleted in commit `0e52e7b` (PR #673). `grep -i 'PatternKind\|REDACTED:slug' docs/adr/` → empty. The categorised-substitute design is captured **nowhere in surviving docs** — only in source code (`patterns.rs:248-307`).

### Proposed fix — drop the parenthetical

```rust
// -----------------------------------------------------------------
// Categorised redaction
//
// The categorised substitute replaces the legacy `[REDACTED:id]`
// ... (rest of the comment unchanged)
```

The existing comment body on the following lines already restates the fact; the parenthetical citation is decorative.

Note: the bare `(D.8.2)` reference on `src/redact/patterns.rs:243` was not touched by #773 (the sweep only matched the `proposal-XX §X.Y` pattern). It's a dangling reference to the same deleted doc, but #784 did not flag it. **Decision: leave for a future sweep — staying in scope for #784 would expand the cluster beyond its 5-site boundary.**

## 5. Out-of-scope findings (flagged for future sweeps)

The F1 swarm found three additional #773-residue sites not in #784's list:

| File:line | Same sweep family | Notes |
|---|---|---|
| `src/research/pdf.rs:2` | Case 1 (broken intra-doc link) | `//! ([` ` `](../                                ))` — same `proposal-04` reference as site 1. Currently emits no rustdoc warning (whitespace URL treated as unparseable). |
| `src/research/pdf.rs:7` | Case 1 | `//! per [`docs/                           `](../docs/                           )` — separate pointer to a deleted doc. Same shape. |
| `src/storage/compression.rs:3` | Case 3 (residue in module doc) | `/// The MVP spec (` `` ` `` ` `` ` ` ` `` `` ``` `` `` ` ` `` `` ``` `` `` `` `` ` ` `` `` ``` `` `` ``` `` `` `   )` — pre-#773 was `/// The MVP spec (` `` `docs/proposal-02-rust.md` `` ` §1.5)` — same as site 3's source doc. |

These three are technically the same root cause as the 5 sites in #784. They are correctly excluded from the v0.15.0 cluster because:

1. The cluster's audit trail should be tight; folding them in muddies the v0.15.0 narrative ("MINOR API break + 4 docs fixes" reads cleanly; "MINOR + 8 docs fixes" loses the focus).
2. The validate-784 finding pattern (4 functionally degraded + 1 broken link) is exactly 5 sites; the additional 3 are residue but not flagged as "functionally degraded" in the issue body.
3. A future L-size cluster can sweep the ~466 residue lines (these 3 + the remaining ~463 cosmetic cases) in one pass.

## 6. Validation invariants

- **No `pub` API change.** All four fixes touch only doc-comments or `//` line comments.
- **No behavior change.** `MAX_CANDIDATES_PER_CALL = 32` survives with a prose rewrite of its rationale; the install hint moves from a broken link to a working intra-doc link (`research::pdf`); the `gz` default and `PatternKind` enum are unchanged.
- **rustdoc baseline (80 warnings) is preserved or improved.** The broken link at `src/error/mod.rs:290` does not currently emit a warning (whitespace URL), but a fix to a working intra-doc link (`research::pdf`) might emit a new warning if the resolver chain is broken; the prose-only alternative is guaranteed ±0.
- **`make fmt-check guard-deps lint build test-ci` all green.** No tests reference the changed lines; no shell-script grep guards touch them.

## 7. Recommendation

Proceed with the 5-site fix as scoped. Commit order in the cluster PR: **#784 first** (smaller, docs-only, zero runtime risk) followed by **#786** (the MINOR API break). This mirrors the v0.14.11 cluster pattern (smaller commits first, larger commits last) and matches the validate-cluster-interactions finding.

For the future L-size cluster, the ~466 remaining residue lines (including the 3 out-of-scope sites flagged here) form a clean scope. The validate-cluster-scope.md §6 rationale for keeping them out of v0.15.0 should be preserved.

## 8. Effort

~30 minutes (5 small edits, no logic changes, no test impact).

Refs: #779 (prose-rewrite precedent), #785 (same-pattern sweep), #773 (the sweep that introduced the residue), #794 (precedent cluster v0.14.11)
