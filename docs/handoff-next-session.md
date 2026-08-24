# Prompt de handoff — `moagan` v0.4 work session

> **Propósito**: arrancar una sesión nueva de opencode en
> `/home/wolf/workspace/projects/moagan` con el máximo contexto
> posible. Léeme entero antes de hacer cualquier otra cosa.

---

## 1. TL;DR

HEAD `693d746 feat(discovery+llm): FacetCache stats + OpenCodeGoResponses
streaming SSE wire (#215)`. Local in sync con `origin/main`. Test
suite verde: `cargo test --lib` reporta `1492 passed; 0 failed;
2 ignored` (2026-08-06). `cargo test --all-targets` reporta
`1492 + 1 + 14 + 3 + 3 = 1513 passed; 0 failed` across all targets.
**63 commits** / **~70 PRs** squash-mergeados desde el último handoff
(`dbbaeac feat(llm): wire circuit breaker per-provider in registry (#71)`).
Tracks **E** (sandbox hardening D.11.x), **I** (discovery resilience
D.13.x + D.34.x + D.1.13), **G** (JSON output paths A + B + C), y
**K** (cuarta etapa K.1-K.4) **cerrados**. Tracks **E1-E10** (LLM
wiring), **F1-F5** (storage + checkpoint + manifest), **M** (Tier A
correctness), **J#5** (dashboard graph), **T1-T7** (telemetry + CLI +
error) **cerrados** en la misma ventana. PRs **H1+H2+H3** (AnthropicCompat + **invalidate ledger** + BLAKE3 default,
#204; **invalidate ledger is documented as not implemented — see note below**),
**J1**
(Cardinality::for_mode + judge quorum, #205), **J3** (dashboard
/api/lineage + K.4 auth, #212), y el **D.13.15 exhaustivo** (HardIncompat
13 variantes, +this commit) **cerrados**. Crate version `0.4.0`
(Cargo.toml). 27 roles LLM wirados. 19 sub-comandos CLI top-level.
13 migraciones SQLite (`v001`-`v013`). 202 archivos `.rs` (~75 200 LoC).

## 2. Tabla de los últimos 30 PRs mergeados (Sesiones D-J)

(Orden cronológico inverso. Cada PR cierra uno o más items del
catálogo D.x o del roadmap prospectivo K.x.)

### Sesión D — E1-E10 (LLM wiring hardening)

| #   | PR  | Track | D.x / spec | Capacidad |
|---:|----:|---|---|---|
|  1 | #152 | E1   | — | `RateLimiter` wire-up en `BreakeredProvider::send` |
|  2 | #154 | E2   | — | rubric anchors inyectados en Judge + Critique prompts |
|  3 | #156 | E3   | — | `SelectionPlan::apply` opera sobre Proposal text |
|  4 | #158 | E4/E6/E9 | — | DAG layers + `FinalDisagreement` + Intake normalize |
|  5 | #160 | E5/E10 | — | sketch quality scoring + hostile-prompt policy |
|  6 | #174 | E7   | — | `TiefighterCritic` sidecar opt-in (T=0.0, top_p=0.1) |
|  7 | #178 | E8   | — | `PersonaPicker` + `AnglePicker` helpers opt-in |

### Sesión E — F1-F5 (storage + checkpoint + manifest)

| #   | PR  | Track | D.x / spec | Capacidad |
|---:|----:|---|---|---|
|  8 | #162 | F1   | — | `Modify(text)` del checkpoint al rerank feedback |
|  9 | #164 | F2   | — | run leases + heartbeat + zombie recovery |
| 10 | #166 | F3   | — | `BudgetObserver` + optional-phase gating |
| 11 | #168 | F5   | — | manifest v2 + `config_hash` + `tar.zst` export |
| 12 | #170 | F4   | — | provider capabilities + wire formats unification |

### Sesión F — M + J#5 + T1-T7 (telemetry + reconcile + cli + error)

| #   | PR  | Track | D.x / spec | Capacidad |
|---:|----:|---|---|---|
| 13 | #172 | M    | M#1 / M#2 | Tier A correctness + rubric validation |
| 14 | #188 | J#5 + D.33.x | D.33.1-4 + D.33.7 | dashboard graph + manifest extensions |
| 15 | #180 | T1   | — | Adversary 5 patterns + `RefineAction` + `StaleArtifact` + `BudgetCascade` |
| 16 | #182 | T2   | D.11.12 / D.28.1 | `tool_versions` + reconcile per-run |
| 17 | #184 | T2   | D.28.1 | `reconcile(run_id)` standalone command |
| 18 | #186 | T3   | D.17.1-4 + D.17.7-10 | exhaustive telemetry |
| 19 | #189 | T4   | D.12.10 / D.12.11 / D.16.2 / D.26.5 / D.29.9 | error enrichments |
| 20 | #191 | T5   | D.14.6-.21 + D.15.2-.6 | CLI flags batch |
| 21 | #193 | T6   | D.35.3-.5 + D.6.4 + D.1.4 | api-key + cache + outbox |
| 22 | #195 | T7   | D.9.6 + D.19.13 + D.19.19 + D.19.20 + D.19.7 | provider + streaming |

### Sesión G + H + J — Catalog wiring completion (12 PRs)

| #   | PR  | Track | D.x / spec | Capacidad |
|---:|----:|---|---|---|
| 23 | #198 | outbox wire | D.1.4 wire | `record_with` wired into call sites |
| 24 | #199 | streaming | D.19.7 closure | SSE streaming parser real |
| 25 | #200 | provider | D.9.6 | wire per-provider semaphores |
| 26 | #201 | refine + llm | D.22.5 + Path C | `StaleArtifact` + `JsonRepairV2` re-call |
| 27 | #202 | discovery | E8 wire | auto-invoke `PersonaPicker` + `AnglePicker` |
| 28 | #203 | ranking + storage | — | `invalidate_downstream` + `SynthesisRequest` + v012 |
| 29 | #204 | llm + storage | H1 + H2 + H3 | `AnthropicCompat.send` + **invalidate ledger** (no implementado, ver nota) + BLAKE3 |

> **Documented as not implemented (v0.5 PR-13, docs-only resolution):**
> `invalidate_ledger` does not exist in `src/` (`rg invalidate_ledger src/`
> returns 0 hits). The H2 atomicity guarantee is provided by
> `outbox_tx::record_with` (wired in `src/telemetry/mod.rs` and
> `src/reconcile/mod.rs` since PR #198). H1 (`AnthropicCompat.send`) and
> H3 (BLAKE3 default) in this row are real. Historical claim preserved
> for the audit trail. See v0.5 audit (PR #253) and v0.5 PR-13.
| 30 | #205 | domain + phases | J1 | `HardIncompat` extended + `Cardinality::for_mode` + quorum |
| 31 | #210 | phases + cli | D.21.3 + D.28.3+4 | `SelectionPlan::keep_top/diverse/outlier` + startup reconcile sweep |
| 32 | #212 | telemetry + storage + research | J3 | dashboard `/api/lineage` + v013 + K.4 auth |
| 33 | #214 | ranking + refine | — | Adversary 5→7 patterns + `RefineAction` dispatcher |
| 34 | #215 | discovery + llm | — | FacetCache stats + OpenCodeGoResponses streaming SSE |
| 35 | #216 | domain | D.13.15 | `HardIncompat` exhaustivo (13 variants) — this commit |

## 3. Deferred items restantes (no atacados)

Ordenados por valor/costo descendente. El estado refleja HEAD
`693d746` (2026-08-06).

### 3.1 K.4 ampliado (D.43, propuesta cuarta etapa)

- PDFs, JS rendering, auth, advanced rate-limiting.
- Acotado hoy a 4-host allowlist (`docs.rs`, `crates.io/api`,
  `github.com/api` + 1 configurable), 3 URLs/call, 4 KB/response,
  5s timeout.
- PR #212 añadió auth básico para los 4 hosts; lo que queda es
  extender a PDFs / JS rendering.
- Trade-off en `docs/proposal-04-cuarta-etapa.md` §4.

### 3.2 Cross-platform sandbox fallback (macOS / WSL)

- Sandbox hardening (Tracks E + D.11.x) es Linux-only. Las primitivas
  `unshare(CLONE_NEW*)` + cgroup v2 + seccomp BPF no tienen
  equivalente en macOS / WSL.
- Trade-off importante: spec §11.4 asume Linux; v0.6+ queda
  pendiente.

### 3.3 `process_locks` lease module (D.1.5)

- Schema `process_locks` está creada (v008). Las funciones
  `acquire_process_lock` / `release_process_lock` son scaffolding
  hasta que exista un `src/storage/lease.rs` module con fence
  monótono (`holder` keyed con FencingToken).
- Diferido por falta de demanda concreta.

### 3.4 Track C path B wire-up to dispatcher

- Track C (#202, E8 wire) shippeado. La path B (LLM-side re-call
  con schema completo) documentada en
  `docs/research-json-structured-output.md` y queda deferred por
  decisión del usuario (no arreglar JSON output forzado, sólo
  hacerlo tolerante — path A + B + C shipped en #117, #119, #201).

### 3.5 Dashboard cross-run analytics

- PR #212 cerró `J3` (`/api/lineage` cross-run endpoint). Quedan
  otras vistas cross-run (e.g. `/api/compare-runs`,
  `/api/aggregates`) como opt-in del catálogo.

### 3.6 `seccomp`/`cgroup` Linux-only features on macOS / WSL

- Idéntico a §3.2. Documentado por separado para tracking.

### 3.7 Catalog items J4 follow-ups

- `Judge quorum per-mode` matrix (J.1 / D.21.7) ✅ cerrado en #205.
- `judge_consensus` threshold tuning queda como opt-in del catálogo.

### 3.8 HardIncompat extensions beyond catalog I.6

- PR #216 (this commit) cerró la lista exhaustiva de 6 variantes
  del catálogo D.13.15. Variantes adicionales (e.g. `ClusterLocalInGlobal`,
  `PullInPushOnly`, `StatelessInStateful`) quedan como opt-in para
  una sub-fase futura si surge demanda.

## 4. Convenciones

### 4.1 Commit signing (mandatory)

- GPG key global: `414687A3CD7E65B9` (program `gpg2`).
- `commit.gpgsign = true` en `~/.gitconfig` (global, heredado por
  este repo).
- Conventional Commits en inglés: `feat`, `fix`, `refactor`, `docs`,
  `test`, `chore`, `ci`, `build`, `perf`. Scope opcional
  (`feat(cli):`, `fix(build):`). Subject ≤ 72 chars, body explica el
  *por qué* (no el *qué*).
- **Nunca** `--no-gpg-sign`, `--amend`, `--force` (incluyendo
  `--force-with-lease`).
- Pre-push verification:
  `git log --pretty="%H %G? %s" origin/<branch>..HEAD` — todas las
  líneas deben empezar con `G`.

### 4.2 No-go list

- **NO** Anthropic SDK crates (`anthropic-*`, `claude-*`).
  `scripts/check-no-anthropic-sdk.sh` lo enforce.
- **NO** `secrecy` crate. Usar `moagan::secret::SecretString` con
  `zeroize`.
- **NO** `axum`, `hyper`, `sqlx`, `governor`, `figment`, `refinery`,
  `askama`, `handlebars`, `lettre`, `inquire`, `time` crate.
- **NO** mutable globals, no `lazy_static`, no `Mutex<Option<T>>` para
  state.
- **NO** `tokio::spawn` sin un `JoinHandle` grabado o un
  `CancellationToken` parent.
- **NO** secret literals en código, CLI flags, o config files
  committed.

### 4.3 Validation gauntlet (los 4 gates)

```bash
cargo fmt --all -- --check                          # formatter diff = empty
cargo clippy --all-targets -- -D warnings           # 0 warnings
cargo test --all-targets                            # 0 failed (1492 lib + 21 integration)
cargo build                                         # exit 0
```

### 4.4 Smoke gates

```bash
moagan run --mode fast --provider mock:mock-model              # produce final/portfolio.md + rankings/ranking.json
moagan run --mode fast --provider minimax:MiniMax-M3 with MINIMAX_API_KEY  # +telemetry/calls.jsonl.gz
```

## 5. PR protocol 10-step

Repo OWNER (`airvzxf/moagan`, permission ADMIN). Squash-merge
habilitado. Copilot review habilitado pero a menudo quota-exhausted.

```bash
# 1. Branch off main
git checkout main && git pull --ff-only
git checkout -b <type>/<scope>          # feat/, fix/, refactor/, docs/, test/, chore/, ci/, build/, perf/

# 2. Cambios + gauntlet local
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --lib
cargo build

# 3. Commit firmado GPG
git add -A && git commit -m "..."
git log --pretty="%H %G? %s" HEAD~1..HEAD   # debe ser G

# 4. Push + verify
git push -u origin <branch>
git log --pretty="%H %G? %s" origin/main..HEAD   # todas G

# 5. Issue
gh issue create --title "..." --label "..." --body "..."

# 6. PR con Closes #N
gh pr create --base main --head <branch> --title "..." --body "..."

# 7. CI + Copilot review
~/.config/opencode/scripts/gh-pr-wait.sh <N>

# 8. Apply feedback / resolve threads
gh api /repos/<owner>/<repo>/pulls/<N>/comments | jq -c '.[] | {id, path, body}'
gh-pr-resolve-thread.sh <N> <comment-id> "<reply-body>"

# 9. Squash-merge OWNER
gh pr merge <N> --squash --delete-branch

# 10. Verify
gh issue view <issue> --json state    # CLOSED
git fetch origin main && git log --oneline -3
```

El squash-merge commit en `main` aparece firmado por la web-flow key
de GitHub (`C` en `git log --pretty="%G?"`). Es esperado.

## 6. Plan para la siguiente sesión (delegar al subagent planning)

Status al cierre de HEAD `693d746` (2026-08-06):

- ✅ **HardIncompat catálogo completo (D.13.15)** — cerrado en
  PR #216 (this commit). 6 variantes exhaustivas reemplazan la
  lista parcial de 5.
- ✅ **Dashboard `/api/lineage` cross-run endpoint (J#5 follow-up)** —
  cerrado en PR #212 (J3).
- ✅ **DiscoveryCardinality wiring (D.21.3 / J.2)** — cerrado en
  PR #210. `SelectionPlan::keep_top/diverse/outlier` ya wired.
- ✅ **Cardinality::for_mode (J.1)** — cerrado en PR #205.
- ✅ **Judge quorum per-mode (J.4)** — cerrado en PR #205.
- ✅ **Path C JSON output (`Role::JsonRepairV2` re-call)** — cerrado
  en PR #201 (research-json Path C).
- ✅ **HardIncompat exhaustivo (D.13.15)** — cerrado en PR #216
  (this commit).

Tracks abiertos al cierre (ordenados por valor/costo):

### 6.1 K.4 ampliado (PDFs, JS rendering)

- L-sized (1 PR). PR #212 añadió auth básico; falta extender a
  PDFs / JS rendering.
- Wiring: `src/research/fetcher.rs` con nuevos content-type handlers.
- Test: integration test que cubre PDF parsing + JS-rendered HTML.

### 6.2 Cross-platform sandbox fallback (macOS / WSL)

- XL-sized (1+ PR). Las primitivas `unshare(CLONE_NEW*)` + cgroup v2
  + seccomp BPF son Linux-only.
- Trade-off: spec §11.4 asume Linux; v0.6+ queda pendiente.

### 6.3 `process_locks` lease module (D.1.5)

- M-sized (1 PR). Schema `process_locks` (v008) ya existe; queda
  implementar `src/storage/lease.rs` con FencingToken + heartbeats.
- Wiring: completar `acquire_process_lock` / `release_process_lock`
  para que dejen de ser scaffolding.

### 6.4 Deferred a v0.6+

- K.4 ampliado (PDFs, JS rendering) — necesita decisión de producto.
- Cross-platform sandbox fallback (macOS / WSL) — trade-off
  importante.

---

## Anexo A — Topología del código (recordatorio)

```
src/
├── lib.rs                 # entry público, dispatch al CLI
├── main.rs                # entry binario, dotenvy::dotenv() auto-load
├── cancel.rs              # Cancel + CancelTier (3-tier: Soft/Normal/Hard)
├── config.rs              # Config (bloques: server, retention, stability,
│                          # circuit_breaker, profiles, sandbox_allow_injection)
├── error.rs               # Error + ExitCode mapping
├── error_code.rs          # ErrorCode enum (60+ variantes SCREAMING_SNAKE_CASE)
├── fs_layout.rs           # RunPaths::resolve() + MoaganHome
├── ids.rs                 # RunId UUID v7
├── secret.rs              # SecretString con zeroize
├── atomic/                # AtomicWriter
├── audit/                 # audit proxy + verify (sidecar HTTP recorder)
├── checkpoint/            # checkpoint humano (stdin-driven, no dialoguer)
├── cli/                   # 19 sub-comandos top-level + AuditCmd + TelemetryCmd
├── context/               # ContextRef + Loader (--context run_id|file|dir)
├── discovery/             # 9 archivos: matrix, tagger, clusterer, facet,
│                          # facet_cache, extractor, integrator, contradiction,
│                          # epistemic_legacy, pause, coordinator
├── domain/                # Tipos: Brief, Proposal, Sketch, Judge, Ranking,
│                          # Manifest, FinalReport, AdversaryReport, etc.
├── execution/             # Parallelism (semáforo)
├── llm/                   # Providers + CircuitBreaker + RateLimiter +
│                          # RetryBudget + Cache + Wire + Embed + 27 roles
├── phases/                # 28 archivos, fase por fase del pipeline
├── ranking/               # pareto + diversity + cluster + rubric + stability
├── redact/                # RedactPolicy + patterns + writer
├── sandbox/               # Sandbox subprocess + allowlist + denylist +
│                          # strip_secrets + verify_binary_exists + BPF
├── storage/               # SQLite + migrations/ + compression + leases
├── telemetry/             # dashboard + export + verify + retention + redact
└── validators/            # structural + rust + python + typescript + sql +
                           # schema + constraints
```

## Anexo B — Documentos normativos (orden de lectura)

1. `docs/proposal-01-concept.md` — visión conceptual (V4).
2. `docs/proposal-02-rust.md` — spec técnica (T01-06).
3. `docs/proposal-03-add-ons.md` — catálogo aditivo (D.x).
4. `docs/proposal-04-cuarta-etapa.md` — cuarta etapa (K.1-K.4
   shippeados, K.4 ampliado deferred).
5. `docs/v0.4-status.md` — status board vivo (sub-fases G-P + Tracks
   A/B/C/D/E/F).
6. `docs/research-json-structured-output.md` — research-only (no
   arreglar Path C por decisión del usuario).
7. `docs/handoff-next-session.md` — este archivo.

## Anexo C — Entorno local

- **OS**: Arch Linux rolling release, kernel 7.1.3-arch2-1.
- **Shell**: bash 5.x + tmux 3.x.
- **Pinentry**: GUI (`pinentry-gtk`); `default-cache-ttl 31536000` (1 año).
- **SSH**: `github_airvzxf_ed25519` cargado en ssh-agent (GitHub).
- **No CGO**: `rusqlite` se compila bundled (`bundled` feature).
- **Test runtime**: `cargo test --lib` ~32s; integration ~50s.

### Helpers útiles

```bash
# Verificar firma de un commit
git log --show-signature -1 <SHA>

# Contar tests
cargo test --lib 2>&1 | grep "test result:"

# Diff entre origin/main y HEAD
git diff origin/main..HEAD --stat

# Squash-merge CI status
gh pr view <N> --json statusCheckRollup

# Issue tracking
gh issue list --label "enhancement" --state open
```

---

Signed-off-by: opencode
