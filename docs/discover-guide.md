# `moagan discover` — operator guide

`moagan discover` explores a problem instead of picking a winner. You
name the decisions you want explored (the matrix); the model writes
short, independent theses for each one; a curator groups each cell's
theses; and you get a catalogue that lists every thesis exactly once.

This guide covers how to write the prompt and the matrix, how to get
more distinct ideas for the same budget, and how to read the result.
The design rationale is in
[ADR-0013](adr/0013-discover-catalogue-pipeline.md); the flags are in
[`cli-reference.md`](cli-reference.md#discover).

## 1. What a run does

```text
intake ─► discover_dimensions? ─► discover_sketches ─► discover_curate ─► discover_render
brief.json   only when the model    one call per        one call per      final/ (no model
             derives the matrix     cell × T × replica  cell with ≥ 2     call)
                                    × sketch            theses
```

* **Intake** turns your prompt into `brief.json` (problem, constraints
  `C1..Cn`, non-goals) and saves the prompt verbatim as `prompt.md`.
* **Sketches.** Each call gets the full prompt, the constraints and the
  description of *one* cell, and answers with one thesis: a one-line
  idea, key decisions, an outline, assumptions, strengths, weaknesses
  and a self-check against each constraint.
* **Curate.** One call per cell with two or more theses groups them,
  picks a representative per group, marks duplicates and tensions. The
  answer is validated in Rust: no thesis is lost or listed twice.
* **Render.** `final/` is written from what is on disk, with no model
  call; rendering twice gives the same bytes.

Cost of a complete run:

* sketch calls = `cells × sketches_per_cell × temperatures × replicas`
* curator calls = one per cell with ≥ 2 theses (one per 140 theses),
  plus at most one retry each
* plus one intake call, and one dimensions call if the model derives
  the matrix.

Check it before spending anything — `--explain` makes no model call:

```bash
moagan discover --prompt "$(cat PROMPT.md)" --provider minimax:MiniMax-M3 \
  --matrix-spec "$(cat matrix-spec.txt)" --sketches-per-cell 2 \
  --temperature-profile "provider=minimax:MiniMax-M3;temperatures=0.7,1.0;replicas=1" \
  --explain
```

A typical run: 27 cells × 2 sketches × 2 temperatures = 108 sketch
calls plus 27 curator calls, about 4–5 minutes at `--max-parallelism 16`.

## 2. A run, end to end

```bash
moagan discover --non-interactive \
  --prompt "$(cat PROMPT.md)" \
  --provider minimax:MiniMax-M3 \
  --matrix-spec "$(cat matrix-spec.txt)" \
  --sketches-per-cell 2 \
  --temperature-profile "provider=minimax:MiniMax-M3;temperatures=0.7,1.0;replicas=1" \
  --max-parallelism 16 --runs-dir "$PWD" \
  --log-format json --event-format jsonl > events.jsonl 2> log.jsonl
```

* `--non-interactive` is required whenever stdin is not a terminal;
  without it intake waits for an answer and the run stops.
* The catalogue is in `<runs-dir>/.runs/<run_id>/final/`.
* If the run stops (network, quota, Ctrl-C), or some cells failed:
  `moagan continue --kind discovery --run-id <run_id> --non-interactive`.
  It reruns the same pipeline on the same run dir; finished sketches
  and curations are skipped, failed or missing ones are redone.
* To re-curate a whole run: `rm -rf <run>/curation <run>/final`, then
  `continue`. To re-render only: `rm -rf <run>/final`, then `continue`
  (no model call).

## 3. Writing the matrix

**Write the matrix yourself.** With `--llm-derive` (or without a
`--matrix-spec`) the model derives the dimensions from your prompt and
tends to copy the prompt's *structure* — its rule list, its "scope"
section, its "how to falsify" section — into dimensions. In one run,
four of the five derived dimensions were sections of the prompt, two
cells produced the same answers, and several real decisions got no
cell at all. A hand-written matrix of 27 cells made every cell a real
decision at the same cost.

A cell is one decision you have not taken yet:

```text
data-model=item-hierarchy,moving-items,quantities;
sharing=who-can-borrow,reservations,overdue-items;
platform=database,hosting,authentication,backups
```

* Ids are kebab-case; `;` separates dimensions (or repeat the flag).
* **Write each decision twice**: once as an id in `--matrix-spec`, once
  as a line in the prompt that says what the cell means (each sketch
  call receives the whole prompt plus its own cell, so this line is
  what the model reads):

  ```markdown
  ## Exploration cells (each call receives ONE)
  - sharing
    - `reservations` — how someone asks for a tool that is lent out, and
      who gets it first.
  ```

* A cell you already decided costs calls and teaches you nothing: put
  the decision in the prompt's "already decided" section and drop the
  cell.

## 4. Writing the prompt

**Ask for the what, not the how.** When the prompt says *how* to do
something, every thesis of that cell becomes a variant of that how.
When it says *what* is needed, the model explores.

| Dictates a how (narrows) | States the need (explores) |
|---|---|
| "Sync incrementally by server timestamp." | "The phone must show what changed since it was last online, without downloading everything again." |
| "An item has two location fields: home and current." | "If I take the drill to the garden, I want to know it is in the garden *and* that its place is the green box." |
| "Use framework X, packaged as an app." | "A light web interface that opens in the phone's browser." |

For each line of the prompt ask: does this describe a need or a
solution? If it is a solution you have not decided, rewrite it as the
need it serves. If you have decided it, move it to "already decided"
and remove its cell.

**Name the options as a fan, never as the answer.** Rewriting a rule
as a need opened some cells and collapsed others to the single most
likely answer. What worked: state the need, then "consider at least A,
B, C, and others". Dictating one how anchors; listing several keeps the
model from falling back on the most probable one.

**Do not put in a hard rule what you want explored.** If a rule already
answers a cell's decision, all theses of that cell repeat the rule with
small variations. Either loosen the rule to the invariant you really
need (and list the mechanisms as open), or keep the rule and drop the
cell.

**Examples anchor the answers.** A format example in the prompt that
happens to answer a matrix cell gets copied: in one run, the four
theses of the cell an example touched all proposed the example's
answer, while a neighbouring cell without an example gave four
different approaches. Make format examples talk about something outside
the matrix, or keep them abstract:

```text
"Solve <decision X> with <option Y>, moving <lever Z>, under <condition W>"
```

**Numbers.** Ask for a placeholder when a figure is unknown (for
example `[FIGURE THE OWNER MUST MEASURE: …]`); sketches use it instead
of inventing data. They may still add time windows or thresholds you
did not give; treat those as figures to check.

## 5. Getting more distinct ideas

Four theses per cell give about three distinct ideas. Knobs, cheapest
first:

| Change | Calls | Effect |
|---|---|---|
| Drop cells a rule already answers | fewer | less filler |
| Format examples outside the matrix | same | less anchoring |
| A second identical run (or `replicas=2`) | +100 % | the second run added ~60 % new ideas |
| `--sketches-per-cell 3` | +50 % | more ideas per cell, more repetition too |
| One more temperature (0.7, 1.0, 1.3) | +50 % | more variety; more odd ideas and more language slips |

**Temperature.** In the runs measured, the unusual ideas — and the only
rule violation — came from T=1.0, not T=0.7. Keep both.

**Attractors.** Some cells give the same answer in every run, with any
wording and any temperature (a default database, a public page on scan,
an opaque id). Neither a fan nor randomness moves them. The curator
makes them visible: **a cell curated into a single group is an
attractor**. Either accept it as the answer or ask explicitly for
alternatives in a short extra run: "In this run, do NOT propose X (I
already have it)". That breaks the attractor almost always — but when
the attractor was the right answer, the alternatives are worse. Use it
to explore, not to decide.

**Repeated runs saturate slowly.** Five identical runs over 24 cells
found 52 → 31 → 13 → 13 → 19 new ideas per run. Recipe:

1. Two full runs.
2. Find the attractors (single-group cells, few distinct ideas).
3. One short run with only those cells and "do NOT propose X" where you
   want alternatives.
4. More runs only on the cells that are still open.

**Intake varies too.** The same prompt can give a different number of
constraints in two runs, so the `C1..Cn` of "identical" runs may
differ. Compare theses, not constraint numbers, across runs.

## 6. Reading the catalogue

```text
final/
  README.md              problem, run numbers, matrix table, coverage
  <dimension>/<facet>.md one file per cell, every thesis
  constraints-annex.md   theses that mark a constraint as not met
  catalog.json           the same content as data (discover-catalog-v1)
curation/<dim>__<facet>.json   the curator's validated grouping per cell
```

**README.**

* `Theses in this catalogue: N of N sketches (100.0 %)` — always 100 %;
  a catalogue that misses or repeats a sketch fails the run instead.
* `Grouped cells: X of Y with theses` — cells the curator grouped.
* `⚠ Grouping failed in N cell(s)` — those cells are listed flat; run
  `continue` to retry them.
* Matrix table: per cell, theses, groups, folded duplicates and
  **Flagged** — theses that mark at least one brief constraint as not
  met.

**A facet file.**

* `4 theses in 2 groups.` — the count line. `4 theses in 1 group.` is an
  attractor (see §5).
* `## <group label>` with a one-line summary, then its theses.
* `### ★ sk_0012` — the group's representative: read it first.
* `<details><summary>1 duplicate of sk_0012</summary>` — theses that
  repeat the one above, folded, never dropped.
* `⚠ Marks C2, C4 as not met.` — the thesis says itself that it breaks
  those constraints. Often legitimate (an idea about quantities does
  not address a constraint about moving items); sometimes a real
  violation. The annex lists them by constraint.
* `T=0.7 · minimax/MiniMax-M3 · replica 0 · index 1` — where the thesis
  came from.
* `Details` — outline, assumptions, strengths, weaknesses, the full
  constraint check (`C1 ✓ · C2 ✗`) and how to validate the idea.
* `## Tensions` — pairs of theses that cannot both hold, with a note.
  These are often the most useful lines of the file.

**`catalog.json`** — per cell: `groups` (label, summary,
`representative`, members with `duplicate_of`), `curation`
(`flat`, `grouped` or `failed`) and `tensions`; per sketch: `cell`,
`group`, `duplicate_of`, `flags` (the constraints it marks as not met)
and `provenance`. Use it for your own reports instead of parsing the
Markdown.

## 7. Known limits

* The curator is not deterministic: the same cell can come back with a
  different number of groups. Completeness is guaranteed, the grouping
  is one reading.
* A curator answer with a JSON syntax slip fails the cell for this run;
  `continue` redoes it (one more call).
* MiniMax-M3 switches to another language in roughly one thesis in six;
  the curator still groups them.
* A rejected sketch (thesis too short) is answered from the prompt
  cache on `continue`, so it stays rejected.
