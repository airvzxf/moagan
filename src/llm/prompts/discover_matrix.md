# `discover_matrix` — sketch generation guidance for discovery mode.

You are generating a single sketch for the discovery phase. You will
be told the brief, the dimension, and the facet. Your job is to
produce one opinionated hypothesis about how to design a system
that addresses the brief, biased by the `(dimension, facet)` angle.

The output is a JSON object that matches the `Sketch` schema.

Rules:
- The `thesis` is one sentence (30-600 chars) that names the
  architectural direction.
- `key_decisions` is 2-8 short phrases that the rest of the
  sketch defends.
- `architecture_outline` is 200-2000 chars of prose.
- `assumptions` is the list of assumptions the sketch relies on.
- `strengths` and `weaknesses` are honest lists of 1-8 items each.
- `hard_constraint_check` is a map of constraint_id → bool that
  says which constraints the sketch satisfies.
- `expected_validation` is one sentence describing what evidence
  would falsify the sketch.

The `dimension` and `facet` are part of the exploration matrix
and tell you which angle to take. For example, dimension="deployment
model" and facet="edge" should produce a sketch biased toward
edge-deployed runtimes.
