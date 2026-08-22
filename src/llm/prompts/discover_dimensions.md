# `dimension_deriver` — derive exploration-matrix dimensions and facets from the brief.

You are reading a discovery brief (problem, objectives, constraints, etc.)
and proposing the **exploration-matrix dimensions** the discovery run will
fan out across. Each dimension is a high-level axis of variation
(deployment model, storage strategy, auth, observability, cost model,
…) and each dimension carries one or more **facets** — the concrete
values that dimension can take. The integrator phase uses these
dimensions to extract per-facet markdown.

The output is a single JSON object. Do not include any text outside
the JSON.

Always respond with the JSON object as the very first character of your
reply.

Schema (a `Dimensions` envelope):

```json
{
  "dimensions": [
    {
      "id": "kebab-case-id",
      "label": "Human readable label",
      "facets": [
        { "id": "kebab-case-id", "label": "Human readable label", "description": "1-2 sentences" }
      ]
    }
  ]
}
```

Rules:

- Propose between **2 and 6 dimensions**. Fewer than 2 makes the
  matrix degenerate; more than 6 dilutes the fan-out.
- Each dimension carries between **1 and 5 facets**. Asymmetric counts
  are welcome (one dimension may have 2 facets, another 4) — the
  matrix cells are the **sum** of per-dimension facets, not a
  Cartesian product.
- Every `id` is kebab-case, lowercase, alphanumeric, dashes between
  words, ≤ 32 chars. No spaces, no capitals, no underscores.
- Every `label` is short (1-4 words) and human-readable.
- Every facet `description` is 1-2 sentences explaining what an
  extractor's markdown body should cover.
- Pick dimensions that genuinely span the brief's design space.
  Generic axes like "deployment-model" / "storage" are fine when
  relevant, but bias toward axes the brief actually cares about
  (e.g. "consistency-model" for a distributed system brief,
  "auth-strategy" for an API brief, "failure-recovery" for an
  availability-critical brief).
- Facet ids MUST be unique within their dimension. Facet ids MAY
  repeat across dimensions (different facets can share a slug) —
  the matrix pins uniqueness via `(dimension_id, facet_id)`.
- Do NOT include empty `dimensions` or empty `facets` arrays. The
  integrator requires at least one facet per dimension.

Deterministic (temperature 0.0, top_p 0.2). The output drives the
`discovery_dimensions.json` sidecar; two runs against the same brief
must produce identical dimension lists so the cross-run cache is
meaningful.
