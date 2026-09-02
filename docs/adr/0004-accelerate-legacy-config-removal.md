# ADR 0004 — Accelerate legacy config-schema bridge removal to v0.13.1

- **Status**: Accepted
- **Date**: 2026-08-30
- **Deciders**: project owner (private repo)
- **Supersedes**: ADR-0003 §"Re-evaluation trigger #1" (the v0.15
  removal trigger fires at v0.13.1 instead).

## Context

ADR-0003 introduced the v0.13.0 dual-mode deserializer
(`src/config/dual_mode.rs`, 866 LOC) and documented the legacy
single-table `[providers.<name>]` form as "supported until v0.15".
The deprecation window was set per the operator's "deprecate-then-
remove" convention of one minor cycle.

The repo is invisible/private with no external users. The bridge
is a maintenance burden: 866 LOC of deserializer + 73 LOC of
integration tests + the per-model `max_tokens` side-channel
(`ProviderEntry::legacy_model_max_tokens`) all exist only to
accommodate legacy `config.toml` files on the operator's own
machine.

## Decision

Cut the deprecation window short. v0.13.1 (a **patch** release —
semver permits removal of a previously-deprecated surface in any
release type) removes:

- `src/config/dual_mode.rs`
- `src/config/dual_mode_integration.rs`
- `tests/integration_config_dual_mode.rs` + 7 fixtures
- `scripts/check-no-legacy-config-schema.sh`
- `docs/migrations/v0.12-to-v0.13-config.md` (with §4 content
  preserved in `src/llm/max_tokens.rs::resolve_max_tokens`
  module-level docs — the historical `docs/max-tokens-auto.md`
  was migrated to `src/llm/probe_table.rs` module-level docs in
  v0.13.5, closes #694/#695/#696)
- The `legacy_model_max_tokens` side-channel field on
  `ProviderEntry`
- The two `#[serde(deserialize_with = "dual_mode::*")]` attributes
  in `src/config/mod.rs`
- The per-section `tracing::warn!` for legacy TOML
- The `parse_legacy_table` / `deserialize_providers_map` /
  `deserialize_model_list` / `LegacyModel` legacy-input helpers

## What stays

The new-shape types (`ProviderEntry`, `SectionKnobs`) and the
runtime projection (`Config::providers_by_section` + `collapse_providers`)
stay — they are the canonical v0.13 view, not legacy artefacts.
Renaming `providers_legacy` → `providers_by_section` (and the
collapse helper `compute_legacy_providers` → `collapse_providers`)
is a follow-up for v0.14 (~200 LOC refactor across 11 production
files), not part of this acceleration. The names were finalised
in operator decision 2026-09-01 (commit history on
`refactor/rename-providers-legacy-686`).

## Consequences

- Operators with a v0.12 `config.toml` on disk hit a hard
  `toml::from_str` parse error on `Config::load()`. A
  `tracing::error!` in `Config::load` points at the migration
  recipe.
- The `cargo build --release --locked` cold-build path drops ~2
  minutes of compile time (smaller binary; the deleted file is
  generative-code heavy).
- The `tests/integration_audit_e2e.rs` per-model `max_tokens`
  coverage migrates from the side-channel to
  `MOAGAN_<SECTION>_MAX_TOKENS=131072` (Pattern A in PR #683's
  v0.13.0 documentation).
- Per AGENTS.md stop-the-line rule, the PR must be green on
  branch + trunk + release-branch before tagging.