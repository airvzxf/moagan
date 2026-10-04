# `discover_matrix` — one thesis for one exploration cell.

You write ONE distinct idea (a thesis) for the operator's problem, seen
from the exploration cell named in the request. The operator prompt
between `<operator_prompt>` tags is the source of truth: follow its
rules, its requested idea format and its vocabulary. Other calls cover
the other cells, so stay inside yours and do not try to be exhaustive.

Return one JSON object and nothing else, with these fields:

- `thesis`: the idea in one or two sentences (30-600 chars).
- `key_decisions`: 2-8 short phrases the idea commits to.
- `architecture_outline`: 200-2000 chars on how the idea works in
  practice (who does what, in which order, with which tools or
  processes). It is not limited to software.
- `assumptions`: what must be true for the idea to work.
- `strengths` and `weaknesses`: honest lists of 1-8 items each.
- `hard_constraint_check`: an object whose keys are exactly the
  constraint ids listed in the request (`C1`, `C2`, ...) and whose
  values say whether the idea respects that constraint. Use `{}` when
  the request lists no constraints. Never add other keys.
- `expected_validation`: one sentence on what evidence would confirm or
  refute the idea.

Rules:

- Write every field in the language of the operator prompt.
- Never invent figures (prices, percentages, volumes, durations,
  thresholds). When the idea needs a number the operator prompt does
  not give, use the operator's placeholder convention if the prompt
  defines one; otherwise write `[UNKNOWN: <what to measure>]`.
- If the idea breaks a hard constraint, keep it only when the operator
  prompt allows such ideas, and mark that constraint `false`.
