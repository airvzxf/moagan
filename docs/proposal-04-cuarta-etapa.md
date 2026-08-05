# Proposal 04 — Cuarta etapa

> **Status**: prospective. Documents four items that survive V4 §13.6
> as orphan roadmap candidates, with trade-offs before any
> implementation. No code changes are proposed in this document.

## Why a fourth stage

The first three stages (v0.1 MVP, v0.2 second, v0.3 catalog overlays)
are closed. The additive catalog (`proposal-03`) covers the rest of
the user's documented needs. Four items in V4 §13.6 were closed
implicitly along the way (their effect is achieved by other
mechanisms) but the underlying capability was not built. This
proposal captures them so future sessions can pick them up with the
full trade-off matrix in hand.

## Items

### 1. Per-domain profiles

**Capability**: domain-specific system prompts, sampling defaults, and
forbidden-tech lists, switchable via `--profile <name>` on `moagan run`.

**Spec reference**: V4 §13.6 (perfiles por dominio).

**Motivation**:
- Today `Config` carries one global forbidden-tech list, one
  temperature map, one quorum.
- Different problem domains (cryptography, ML, distributed systems,
  frontend) want different defaults.
- A `web.toml` profile would forbid React if a brief mentions SSR
  with a different library; a `rust.toml` profile would relax the
  length floor.

**Trade-offs**:

| Aspect | Pro | Con |
|---|---|---|
| Storage | TOML files in `~/.config/moagan/profiles/<name>.toml` are familiar | New config format to document |
| Discovery | Auto-list profiles via `moagan doctor --profiles` | UX surface area grows |
| Inheritance | `extends = "base"` keeps DRY | Inheritance cycles need detection |
| Cost | One config knob per call; no LLM cost | Up to ~10% more boilerplate in `Config::load` |

**Suggested first cut (if implemented)**:
- 2 profiles shipped by default: `default`, `rust-systems`.
- TOML format with `extends`, `gate_forbidden_techs`,
  `gate_min_length`, `gate_max_length`, `temperature_map`.
- ~1 PR (S).

### 2. User preference learning

**Capability**: a feedback loop where the user can rate the final
portfolio, and the ratings feed into a per-user prompt cache that
informs future runs.

**Spec reference**: V4 §13.6 (aprendizaje de preferencias del usuario).

**Motivation**:
- Every run today produces a portfolio + ranking but the user never
  feeds back which proposal they actually used.
- Without feedback, every run starts from cold defaults.
- A per-user cache (keyed by `MOAGAN_USER` env or `~/.config/moagan/user`)
  would let `TiefighterCritic` and `PersonaPicker` weight historical
  ratings when scoring.

**Trade-offs**:

| Aspect | Pro | Con |
|---|---|---|
| Privacy | Stored locally, redacted on export | Requires new `RedactPolicy` for ratings |
| Cold-start | New users see no degradation (defaults) | Network effect: 1-user cache has limited value |
| Decay | Recent ratings weight more | Need to define decay (linear? exponential?) |
| Cost | One extra `cache lookup` per role call | Cache invalidation on profile change |

**Risk**: requires the user to opt in (`MOAGAN_LEARNING=true`,
default false) to avoid surprising behaviour. Until opt-in UX is
designed, this should not be implemented.

**Suggested first cut**:
- Opt-in via env var.
- Simple JSON cache in `~/.config/moagan/ratings/<user>.json`.
- Linear decay over 90 days.
- ~2 PRs (M).

### 3. Cross-process hibernation

**Capability**: `moagan continue --from-pause` detects a serialised
state from a previous paused run and resumes without re-doing
upstream phases.

**Spec reference**: V4 §13.6 (hibernación cross-process).

**Motivation**:
- Today `moagan continue <run_id>` re-enters the pipeline at the
  last checkpoint and re-runs upstream phases that the user might
  have already accepted.
- A serialised "pause point" (`paused.json` in the run dir) would let
  the user pick up exactly where they stopped across CLI invocations
  and even across reboots.

**Trade-offs**:

| Aspect | Pro | Con |
|---|---|---|
| Resume speed | Skip completed phases | Risk of stale upstream state |
| Disk | One small JSON per pause | Already a lot of per-run files |
| Concurrency | Lockfile prevents two resumes racing | New error class (`LockHeld`) |
| Cost | None at runtime | Pause serialisation adds ~50ms |

**Suggested first cut**:
- Add `paused.json` with phase index + serialised inputs.
- `moagan continue --from-pause` reads it and skips to phase N.
- `moagan pause <run_id>` writes it (if state permits).
- ~2 PRs (M).

### 4. External research

**Capability**: a web fetcher that retrieves up-to-date information
about libraries/APIs mentioned in the brief before the Sketch phase,
grounding proposals in current docs instead of training-cutoff knowledge.

**Spec reference**: V4 §13.6 (investigación externa).

**Motivation**:
- LLM training data is months to years stale; new library releases
  (Rust 1.97+ in our case) are often hallucinated.
- A bounded fetcher (max N URLs, max M KB each, allowlist only) would
  inject real snippets into the Sketch prompt.
- Highest value for `mode=deep` and `mode=explore` where token budget
  can absorb the extra context.

**Trade-offs**:

| Aspect | Pro | Con |
|---|---|---|
| Factual accuracy | New APIs / versions correct | Hallucination risk shifts from API to URL |
| Cost | Tokens for snippet injection | Extra HTTP calls per run |
| Security | Allowlist only | New sandbox code path (overlaps with Track E) |
| Privacy | URLs redacted on export | WebFetch result may contain PII |

**Risk**: this is the heaviest item. It intersects Track E (sandbox
hardening) because the fetcher needs the same allowlist / denylist
treatment as subprocess execution. Not recommended for v0.5; belongs
in v0.6+ if at all.

**Suggested first cut**:
- 2-3 domain allowlists (`docs.rs`, `crates.io/api`, `github.com/api`).
- Max 5 URLs per run, max 8 KB each.
- Same redact-on-write as the rest of the system.
- ~3 PRs (L).

## Suggested order (if implemented)

1. **Per-domain profiles** (cheapest, lowest risk, immediate UX value).
2. **Cross-process hibernation** (medium cost, large UX value for long runs).
3. **User preference learning** (medium cost, requires opt-in UX work).
4. **External research** (heaviest; deferred to v0.6+).

## Closing

This proposal is documentation, not commitment. Each item can be
adopted independently. None of them block the current v0.4
production-readiness goal.

## References

- V4 §13.6: orphan items in `docs/proposal-01-concept.md`.
- v0.4 status: `docs/v0.4-status.md` (closed sub-fases G-P).
- Additive catalog: `docs/proposal-03-add-ons.md`.
