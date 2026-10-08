# ADR 0013 — Discover ends in a curated catalogue, built by one idempotent pipeline

> **Status**: Accepted (shipped in v0.20.0 and v0.21.0)
> **Date**: 2026-10-07
> **Deciders**: `airvzxf/moagan` operator
> **Supersedes**: nothing (the post-sketch chain it replaces had no ADR).
> **Superseded by**: nothing.
> **Relates to**:
> [ADR-0006](0006-discover-test-structural-validation.md) (structural
> tests for discover),
> [ADR-0012](0012-llm-client-trait-migration.md) (the client layer the
> JSON-prefill fix lives in),
> PRs #984–#992.

## Context

`moagan discover` fans a prompt out over a `dimension × facet` matrix:
one model call ("sketch") per cell, temperature and replica, each
returning a short thesis with key decisions, an outline and a
constraint self-check. Up to v0.19 the sketches fed a seven-phase LLM
chain (`discover_tag → cluster → contradict → facet → extract →
integrate → summary`) that was meant to turn them into a knowledge
base by category.

A real run on 2026-10-02 (MiniMax-M3, 15 cells × 7 temperatures × 10
sketches = 1,050 calls, a Spanish business prompt) exited 0 and was
unusable. Measured, not inferred:

| Symptom | Measure |
|---|---|
| The chain collapsed 1,028 theses into 2 clusters, both with the same label | largest cluster 63.9 % |
| The final summary cited a small share of what was generated | 153 of 1,028 theses (14.9 %) |
| The chain cost more calls than it saved | 1,377 calls after the sketches; the tagger alone read 1.4 M input tokens and its output was never used for grouping |
| The sketches never saw the operator's rules | `clarify` rewrote `brief.json` to a fragment: 10 intake constraints → 0; the verbatim prompt and the facet descriptions were not sent |
| Model ids leaked into sketch ids | 21 non-canonical ids (`642`, `sketch-402`, …) |
| Resume and fresh runs were two code paths | a coordinator state machine, a never-wired matrix phase, and a separate resume builder |

Three spikes settled what the replacement could rely on:

* **S1 — JSON prefill.** Replaying moagan's exact request body, the
  `{` assistant prefill made MiniMax-M3 answer intake with a fragment
  in 9 of 10 calls (sketches: 8 of 10); without it, 10 of 10 (9 of 10)
  were complete. The Anthropic-compatible body now sends the prefill
  only to models whose JSON strategy asks for it (#986).
* **S2 — per-cell curator.** Asked to group one cell's ~70 theses,
  MiniMax-M3 kept every id exactly once in 13 of 15 cells only when the
  answer was a list of `{n, g}` pairs over numbered theses (group lists
  with member ids: 7 of 15; positional arrays: 0 of 15). With one
  retry, 15 of 15 cells and 21 of 21 chunks (up to ~137 theses)
  reached full coverage. Every failure was a JSON syntax slip, never a
  wrong assignment.
* **S3 — rules carried by the prompt.** With the verbatim prompt, the
  brief's constraints as `C1..Cn` and the facet description in every
  sketch payload, sketches kept the prompt's language (100 %), used
  the operator's placeholder for unknown figures (80–84 %) and had
  canonical ids (100 %).

## Decision

1. **A matrix cell is the category.** Every sketch belongs to the cell
   it was generated for; nothing re-derives categories after the fact.
   A sketch whose angle names no cell goes to an "Outside the matrix"
   cell.
2. **The product is a deterministic catalogue, not a synthesis.**
   `discover_render` writes `final/` from the sketches on disk with no
   model call: `README.md` (problem, run numbers, profiles, matrix
   table, coverage), one `<dimension>/<facet>.md` per cell listing every
   thesis, `constraints-annex.md` (theses that mark a brief constraint
   as not met), and `catalog.json` (`discover-catalog-v1`). A
   catalogue that does not list every sketch exactly once fails the
   run instead of being written. Rendering twice is byte-identical.
3. **One curator call per cell, validated in Rust.** `discover_curate`
   sends the numbered theses of each cell with two or more theses
   (chunks of at most 140) to `Role::Curator` (T=0.2, no prefill, no
   `top_p`). The answer only proposes groups, `{n, g}` assignments,
   duplicate pairs and tensions; Rust normalises it so every thesis
   appears exactly once (unassigned theses go to an "Ungrouped" group),
   every group has a representative listed first, duplicates are
   folded under one head and never dropped, and tensions name two real,
   distinct theses. A call that fails, does not parse or leaves more
   than 20 % of the theses unassigned is retried once; a second
   failure stores `status: failed` and the cell is rendered flat with a
   warning. A cell curated into a single group is information (an
   attractor), not an error.
4. **One idempotent pipeline; resume is "run it again".**
   `intake → discover_dimensions (only when the model derives the
   matrix) → discover_sketches → discover_curate → discover_render`.
   A fresh run records the operator's choices in `discover_run.json`;
   `moagan continue --kind discovery` runs the same pipeline on the same
   run dir. Each phase skips the work whose artefact already exists: a
   valid `brief.json`, the dimensions sidecar, `sketches/sk_NNNN.json`
   (point `n` of a fixed fan-out order), an `ok` curation that covers
   the cell's current sketch ids. Failed or missing work is redone.
5. **The sketch call sees the operator's prompt.** Intake persists
   `prompt.md` verbatim; each sketch payload carries it, the brief's
   constraints as `C1..Cn` and the facet description of its cell.
   Clarify no longer rewrites the brief. Sketch ids are always
   `sk_NNNN` from the fan-out position.

## Consequences

**Removed** (each was dead weight or actively harmful on the 2026-10-02
run):

* Phases `discover_tag`, `discover_cluster`, `discover_contradict`,
  `discover_facet`, `discover_extract`, `discover_integrate`,
  `discover_summary`, the never-wired `DiscoverMatrixPhase`, the
  `DiscoveryCoordinator` and its resume builders.
* Roles `tagger`, `facet_deriver`, `extractor`, `integrator`,
  `contradiction_judge`, `persona_picker`, `angle_picker`; the embedder
  (`src/llm/embed`, `[embedder]`); the saturation tracker, stop policy
  and outlier detector.
* Config keys `[discovery] tag_threshold | persona_enabled |
  angle_enabled | angle_clusters_min | auto_pickers` and
  `MOAGAN_DISCOVERY_AUTO_PICKERS` (ignored when present).
* Flags `--cluster-threshold` and `--cache-facets` (hidden no-ops in
  v0.20.0, rejected since v0.21.0).
* Run-dir folders `tags/`, `clusters/`, `facets/`, `extractions/`,
  `contradictions/`; `final/cat_*` and `final/summary.*`.
* `CheckpointKind::Discovery` keeps its `"discovery"` token so persisted
  rows parse, but carries no counts and no phase raises it: discover
  asks the operator nothing after the catalogue.

**Added:** `Role::Curator` and its prompt; `curation/<dim>__<facet>.json`
per curated cell; `discover_run.json`; `prompt.md`; the `final/`
layout above. `catalog.json` gains `representative`, `curation`
(`flat | grouped | failed`) and `tensions` per cell. `moagan discover
--explain` counts the curator calls next to the sketch calls.

**Costs and limits:**

* A complete run costs `cells × sketches_per_cell × temperatures ×
  replicas` sketch calls plus one curator call per cell with two or
  more theses (plus at most one retry each). On a 27-cell, 108-sketch
  replay: 27 curator calls and 3 retries; 26 of 27 cells curated on the
  first run, the last one on the next `continue`.
* The curator is non-deterministic: the same cell can come back in a
  different number of groups. Validation guarantees completeness, not
  stable groupings; `catalog.json` records what this run decided.
* The JSON repair still accepts some fragments and does not fix a
  stray `]` (backlog). A failed cell is visible (⚠) and is redone by
  the next `continue`.
* Old run dirs render best-effort: sketches without provenance say so;
  nothing reads the removed folders.

## Alternatives considered

* **Fix the clustering chain** (better embeddings, a lower threshold).
  Rejected: categories already exist as matrix cells, and every extra
  LLM stage after the sketches lost theses (14.9 % cited).
* **One curator call over the whole run.** Rejected: ~1,000 theses do
  not fit one reliable call (S2 worked up to ~140), and cross-cell
  grouping mixes categories the operator chose on purpose.
* **Let the model write a synthesis.** Rejected for now (decision D1):
  a catalogue the operator can read and grep beats prose that may drop
  or invent ideas. A synthesis can be built later on top of
  `catalog.json`.
