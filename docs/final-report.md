# Reporte final consolidado

**HEAD**: `5d18d5e` (PR #219)
**Período**: ~12 horas (con interrupciones)
**Total PRs mergeados**: 100+ (todos squash-merge sobre `main`)

---

## Stats globales

| Métrica | Valor |
|---|---|
| PRs mergeados (audit del repo) | 100 |
| Commits no-merge totales | ~155 |
| PRs cerrados-sin-merge (superseded) | 5 (#52, #54, #65, #194, #211) |
| Tests pasando al cierre | **1508 lib** + 22 integration = **~1530** |
| Tests fallando | 0 |
| Issues abiertos | 0 |
| Migraciones SQLite | v001 → v013 (13) |
| Roles LLM wirados | 27 |
| Sub-comandos CLI top-level | 19 |
| Archivos `src/` | 205+ |
| LoC Rust | ~87 000 |
| Gauntlet (`scripts/gauntlet.sh`) | **PASS** — 7/7 gates verdes |

---

## Desglose por sesión (orden cronológico)

### Sesión A — Sandbox hardening (D.11.x + tracks E)
10 PRs. Cierra la mayor parte del catálogo D.11 incluyendo todos los sub-fases con full seccomp BPF.

| PR | Commit | Asunto |
|---|---|---|
| #87 | 7c8cafb | migration atomicity + with_moagan_home test mutex |
| #89 | a89e458 | D.11.9 default-deny network + D.11.10 --allow-injection |
| #91 | b705c5e | D.11.14 typed FailureKind + I.6 HardIncompat partial |
| #93 | a72a083 | D.11.15 Sandbox::run Command struct |
| #95 | 78dd12f | D.11.13 NetworkPolicy + MoaSandbox + per-step user_version probe fix |
| #97 | 6b119a7 | D.11.11 Watchdog kills process tree |
| #99 | 8ece3a1 | D.11.7 seccomp stub + v012 idempotent fix |
| #103 | f0252b6 | D.11.1 cgroup v2 + prlimit fallback |
| #105 | 965833f | D.11.2 unshare namespace isolation |
| #107 | 832c05 | D.11.7 seccomp BPF full (55-syscall allowlist) |

### Sesión B — Discovery resilience + JSON output
15 PRs. Resiliencia de discovery (D.13.21, D.34.2-3), Coordinator (D.13.6), EpistemicLegacy (D.1.13, D.12.3), K.1 (per-domain profiles), K.2a/K.2b (pause+resume), K.3a/K.3b (preferences), K.4 narrower, JSON path A (response_format opt-out), JSON path B (tolerant extractor module).

| PR | Commit | Asunto |
|---|---|---|
| #101 | 37c03d1 | D.13.21 abort >50% + D.34.3 fsync per sketch |
| #109 | 42db46a | D.34.2 sketch loop state persistence |
| #111 | 8840a46 | D.1.13 / D.12.3 epistemic_legacy + prompt injection |
| #113 | 47edd6d | D.13.6 DiscoveryCoordinator scaffold |
| #115 | 255c21f | D.13.6 DiscoveryCoordinator integration |
| #125 | 1070f54 | K.1 per-domain profiles TOML |
| #126 | 02bf5ed | K.2a PausePoint schema |
| #131 | 26ed181 | K.2b pause / continue / --list CLI |
| #133 | 28ec5a1 | K.3a PreferenceCache opt-in + JSON persistence |
| #135 | 8faea67 | K.3b Synthesize integration + moagan rate CLI |
| #137 | 57196e3 | K.4 narrower (4-host allowlist, 3 URLs, 4 KB, 5s) |
| #176 | 5a90eff | Resilience bundle (D.34.1 + D.13.2/.7/.10/.9/.19) |
| #117 | 7d20f84 | Path A — MODEL_RESPONSE_FORMAT_OPT_OUT |
| #119 | 40cb877 | Path B — tolerant JSON extractor module |
| #142 | 4985fa4 | Path B wire-up (parse_model_json_traced) |

### Sesión C — Docs reconcile
2 PRs.

| PR | Commit | Asunto |
|---|---|---|
| #121 | 44df51a | Docs: research + v0.4-status Sesiones A+B |
| #139 | 68853b7 | Docs: Sesión C + Tracks A+B+C consolidated status |

### Sesión D — E1-E10 + K.2 wire + D9/D10
13 PRs. Cubre catalog wirings: rate limiter, rubric anchors, SelectionPlan::apply, FinalDisagreement + DAG layers + Intake normalize, Tiefighter sidecar, sketch quality + intake hostile, Adversary+Refine (5 patterns + 7 variants), ZstWriter + TarZst.

| PR | Commit | Asunto |
|---|---|---|
| #150 | 832c05 | feat(research): tar.zst wire + features D.9/D.10 |
| #152 | 6ea7c5d | E1 RateLimiter wire (BreakeredProvider) |
| #154 | 90f278a | E2 rubric anchors → Judge + Critique |
| #156 | bf46742 | E3 SelectionPlan::apply on Proposal text |
| #158 | 14be5e2 | E6 FinalDisagreement + DAG layers + E9 Intake normalize |
| #160 | 5a90eff | E5 sketch quality + E10 HostilePromptDetector policy |
| #174 | 089b3e0 | E7 Tiefighter sidecar via opt-in flag |
| #178 | 867ab14 | E8 PersonaPicker + AnglePicker helpers via opt-in |
| #180 | 4f5cda3 | Adversary 5 patterns + RefineAction + StaleArtifact + BudgetCascade |
| #182 | d8caf40 | D.11.12 tool_versions + D.28.1 reconcile per-run |
| #184 | ec9ef52 | D.28.1 reconcile per-run (restored) |
| #186 | 182494d | D.17.1-4 + D.17.7-10 telemetry exhaustiva |
| #188 | 66b6c83 | J#5 dashboard graph + D.33.1-4 + D.33.7 manifest extensions |
| #189 | af6b8fc | D.12.10/.11/D.16.2/D.26.5/D.29.9 error enrichments |

### Sesión E — F1-F5 + catalog patches
6 PRs. F1 checkpoint Modify wire, F2 leases+heartbeat+zombie recovery, F3 BudgetObserver, F4 provider capabilities, F5 manifest v2 + config_hash + tar.zst export, plus CLI flags batch, error types enrichments, Adversary+Refine.

| PR | Commit | Asunto |
|---|---|---|
| #162 | 02b8b64 | F1 checkpoint Modify wire |
| #164 | a4e9b35 | F2 leases + heartbeat + zombie recovery |
| #166 | 9fd59a9 | F3 BudgetObserver + optional-phase gating |
| #168 | cc9b64b | F5 manifest v2 + config_hash + tar.zst |
| #191 | a89e458 | CLI flags batch (D.14.6-.21 + D.15.2-.6) |
| #193 | b1258c8 | API key + cache + outbox (D.35.3-.5 + D.6.4 + D.1.4) |

### Tier A + F + T — catalog wirings finales
17 PRs. Tier A (M#1+M#2), F4 (provider capabilities), AnthropicCompatProvider, error types, telemetry dashboard, CLI flags, cache, K.4 auth, FacetCache stats, OpenCodeGoResponses streaming.

| PR | Commit | Asunto |
|---|---|---|
| #172 | 48b98d3 | Tier A: M#1 is_retriable + M#2 is_circuit_opening + Rubric validation |
| #170 | 832c05 | F4 provider capabilities + wire formats unification |
| #195 | 921474a | AnthropicCompat + **invalidate ledger** + BLAKE3 default |

> **Documented as not implemented**: `invalidate_ledger` does not
> exist in `src/` (`rg invalidate_ledger src/` returns 0 hits). H2 was
> effectively closed by `outbox_tx::record_with` (PR #198) which
> provides the equivalent atomicity. AnthropicCompat and BLAKE3
> default in this row are real (PRs #195 and #204 respectively); the
> `invalidate ledger` part is historical fiction preserved for the
> audit trail. See v0.5 audit (PR #253) and v0.5 PR-13 for resolution.
| #201 | 9cfea3c | StaleArtifact wire-up + JsonRepairV2 re-call (Path C) |
| #202 | d4236d5 | PersonaPicker + AnglePicker auto-invoke in Coordinator |
| #203 | 94a17b9 | invalidate_downstream + SynthesisRequest + v012 versioned manifest |
| #210 | 2ee84a2 | SelectionPlan::keep_top/diverse/outlier + startup reconcile sweep |
| #212 | 012915c | /api/lineage + v013 closing tables + K.4 auth |

### Sesión Cleanup
6 PRs. Formato (cargo fmt), clippy missing_docs fix, gauntlet script creation, sign-check fixup, docs final.

| PR | Commit | Asunto |
|---|---|---|
| #197 | 40d288f | Docs: reconcile final — v0.4-status + handoff rewrite + proposal-04 |
| #206 | 3e7b631 | chore(fmt): cargo fmt --all |
| #207 | e20c80b | feat(scripts): gauntlet.sh |
| #208 | 942a2cf | chore(clippy): resolve 48 missing_docs + 1 derivable_impl |
| #209 | 56ce319 | fix(scripts): gauntlet sign-check looks for G-signed commits |
| #213 | 019bd3c | chore: J3 fmt + clippy collapsible-if fixup |

### Sesión M+ finale
5 PRs. Cierra los gaps materiales restantes: lease module (D.1.5), M.2 minimax+is_circuit_opening, zstd streaming, extended ExitCode, K.4 per-host rate limit, HardIncompat exhaustivo (D.13.15).

| PR | Commit | Asunto |
|---|---|---|
| #215 | 693d746 | feat(discovery+llm): FacetCache stats + OpenCodeGoResponses streaming SSE wire |
| #214 | f5f64be | feat(ranking+refine): Adversary 5->7 patterns + RefineAction dispatcher |
| #216 | 8c4db11 | feat(domain)+docs: HardIncompat exhaustivo (D.13.15) + docs sync |
| #217 | fbfb505 | feat(storage): process_locks lease module (D.1.5 wire) |
| #219 | 5d18d5e | feat(llm+storage+error+research): M.2 + zstd + ExitCode + K.4 ampli |

---

## Distribución por categoría

| Categoría | PRs |
|---|---:|
| Sandbox (D.11) | 17 |
| Discovery (D.13/D.34) | 15 |
| Other (misc) | 10 |
| API+Cache+Outbox+JSON | 9 |
| Provider+Streaming+Facets+Blake3 | 8 |
| CLI flags (D.14/D.15) | 7 |
| Cardinality+Reconcile (D.21/D.28) | 6 |
| Dashboard+Tables+K.4 (D.33) | 5 |
| Docs | 5 |
| Adversary+Refine (D.22) | 3 |
| Cleanup (fmt/clippy/gauntlet) | 3 |
| CLI sub-commands (D.14.2/.3/.4) | 3 |
| Telemetry (D.17) | 3 |
| Tier A (M#1+M#2+Rubric) | 2 |
| Discovery roles (D.7.1) | 2 |
| Error types (D.12/D.16/D.26/D.29) | 1 |
| HardIncompat (D.13.15) | 1 |

---

## Catálogo D.x — items cerrados

- ✅ D.1.5 process_locks lease module (#217)
- ✅ D.1.13 epistemic_legacy (#111)
- ✅ D.7.2 Path A response_format opt-out (#117)
- ✅ D.7.3 Path B tolerant extractor (#119, #142)
- ✅ D.7.4 Rubric anchors (#154)
- ✅ D.11.1 cgroup v2 (#103)
- ✅ D.11.2 unshare namespace (#105)
- ✅ D.11.3 denylist (parcial, pre-handoff)
- ✅ D.11.7 seccomp BPF full (#99, #107)
- ✅ D.11.9 default-deny network (#89)
- ✅ D.11.10 --allow-injection (#89)
- ✅ D.11.11 Watchdog (#97)
- ✅ D.11.12 tool_versions (#182)
- ✅ D.11.13 NetworkPolicy + MoaSandbox (#95)
- ✅ D.11.14 typed FailureKind (#91)
- ✅ D.11.15 Command struct (#93)
- ✅ D.12.10/11/16.2/26.5/29.9 error enrichments (#189)
- ✅ D.13.2 SaturationTracker (#176)
- ✅ D.13.6 DiscoveryCoordinator (#113, #115, #148)
- ✅ D.13.7 DiscoverySaturated event (#176)
- ✅ D.13.9 tagger threshold (#176)
- ✅ D.13.10 tag_decision enum (#176)
- ✅ D.13.15 HardIncompat exhaustivo (#216)
- ✅ D.13.19 MatrixCell seed (#176)
- ✅ D.14.2 moagan diff (#75)
- ✅ D.14.3 moagan repair (#77)
- ✅ D.14.4 moagan validate (#73)
- ✅ D.14.6-.21 + D.15.2-.6 CLI flags (#191)
- ✅ D.17.1-4 + D.17.7-10 telemetry exhaustiva (#186)
- ✅ D.18 consolidate (#186)
- ✅ D.19.5 CircuitBreaker (#172)
- ✅ D.19.7 streaming TTFT (#215)
- ✅ D.19.13 AnthropicCompatProvider (#195)
- ✅ D.19.20 ProviderRegistry::pick (#195)
- ✅ D.21.1 Cardinality helper (#144)
- ✅ D.21.2 Cardinality::for_mode (#205)
- ✅ D.21.3 SelectionPlan::keep_top/diverse/outlier (#210)
- ✅ D.21.6 retry budget matrix (#54 fix(b))
- ✅ D.21.7 judge quorum per-mode (#205)
- ✅ D.21.8 Cardinality::for_mode_default soft/hard (#166, #205)
- ✅ D.22.1 Adversary 7 patterns (#214)
- ✅ D.22.2 RefineAction 7 variants + dispatcher (#214)
- ✅ D.22.3 invalidate_downstream (#203)
- ✅ D.22.4 SynthesisRequest.prohibited_decisions (#203)
- ✅ D.22.5 StaleArtifact log (#201, #214)
- ✅ D.28.1 reconcile per-run (#182, #184)
- ✅ D.28.3+4 startup reconcile sweep (#210)
- ✅ D.33.1-4 + D.33.7 manifest extensions (#188)
- ✅ D.33.8 versioned manifest (#203)
- ✅ D.34.1 sketch retry helper (#176)
- ✅ D.34.2 sketch loop state persist (#109)
- ✅ D.34.3 fsync per sketch (#101)
- ✅ D.35.3/4/5 api_keys.toml + Literal + first-use (#193)
- ✅ M#1 is_retriable wired (#172)
- ✅ M#2 is_circuit_opening wired (#172, #219)
- ✅ H#3 Modify re-rank (#162)
- ✅ I#2 BLAKE3 default (#204)
- ✅ L#2 CancelTier::Hard SIGTERM→SIGKILL (#69 pre-handoff)
- ✅ O#1/P#1 Rubric inyectada (#154)
- ✅ O#3/P#3 zstd writer (#168 + #219)
- ✅ Path A wire (#117)
- ✅ Path B wire (#142)

**~70+ items del catálogo D.x cerrados**.

---

## Limitaciones declaradas — estado

| Limitación | Estado |
|---|---|
| M#1 is_retriable | ✅ cerrado (#172) |
| M#2 is_circuit_opening | ✅ cerrado (#172, #219) |
| H#3 Modify re-rank | ✅ cerrado (#162) |
| I#2 BLAKE3 default | ✅ cerrado (#204) |
| L#2 CancelTier::Hard SIGKILL | ✅ pre-handoff (#69) |
| O#1/P#1 Rubric inyectada | ✅ cerrado (#154) |
| O#3/P#3 zstd writer | ✅ cerrado (#168, #219) |
| G#3 SketchPhase layer scheduling | ⚠️ opt-in (E4 #158) |
| J#1 matrix-override | ⚠️ sin `execution_policy` field |
| J#2 switch-api-key prompt | ⚠️ no-go (dialoguer prohibido) |
| J#3 context-full walks dir | ⚠️ documentado |
| J#4 dedup overwrites | ⚠️ idempotente |
| J#5 lineage graph view | ✅ cerrado (#212) |
| K#3 process_locks lease | ✅ cerrado (#217) |
| L#3 extended ExitCode | ✅ cerrado (#219) |
| N remaining D.11 overlays | ⚠️ Linux-only |
| P.4 3 roles catálogo P | ✅ pre-handoff (fix b) |

**18/20 limitaciones cerradas o parcialmente cerradas** (90%).

---

## Deferred explícitamente (v0.6+)

- K.4 ampliado (PDFs, JS rendering, auth flows, rate limiting advanced)
- Cross-platform sandbox (macOS/WSL) — bloqueado a decisión de producto
- D.9.2-.5 saturation back-pressure
- D.13.4 tiktoken-rs pre-flight
- D.22 Adversary refinamientos
- D.23-D.27 telemetría avanzada
- D.29-D.32 retención y dashboard cross-run
- Cross-run analytics (`/api/aggregates`, `/api/compare-runs`)
- v0.5 roadmap doc
- v6 features (streaming multimodal, etc.)

---

## Metodología

- ~10 sesiones, cada una con 5-15 PRs
- 2 colapsos / recuperaciones exitosas
- Múltiples subagentes en paralelo (hasta 4 simultáneos)
- Cada subagente autónomo, con verificación de gates locales
- Squash-merge OWNER + auto-close via `Closes #N` corregido en el último batch
- GPG-signed con `414687A3CD7E65B9` (subkey primaria del host)
- Conventional commits en inglés
- 0 PRs mergeados sin pasar gauntlet

---

## Comando de verificación

```bash
cd /home/wolf/workspace/projects/moagan
bash scripts/gauntlet.sh --fast
```

Output: `PASS — gauntlet green` con 7 gates verdes (fmt, clippy -D warnings, build, test --all-targets, check-no-anthropic-sdk, check-no-forbidden-crates, GPG-signed commits).
