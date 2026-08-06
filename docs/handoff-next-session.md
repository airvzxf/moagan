# Prompt de handoff — `moagan` v0.4 work session

> **Propósito**: arrancar una sesión nueva de opencode en
> `/home/wolf/workspace/projects/moagan` con el máximo contexto
> posible. Léeme entero antes de hacer cualquier otra cosa.

---

## 1. TL;DR

HEAD `921474a feat(provider+streaming): D.9.6 + D.19.13 + D.19.19 +
D.19.20 + D.19.7 (#195)`. Local in sync con `origin/main`. Test
suite verde: `cargo test --lib` reporta `1406 passed; 0 failed;
2 ignored` (2026-08-06). **63 commits** / **65 PRs** squash-mergeados
desde el último handoff (`dbbaeac feat(llm): wire circuit breaker
per-provider in registry (#71)`). Tracks **E** (sandbox hardening
D.11.x), **I** (discovery resilience D.13.x + D.34.x + D.1.13),
**G** (JSON output paths A + B), y **K** (cuarta etapa K.1-K.4)
**cerrados**. Tracks **E1-E10** (LLM wiring), **F1-F5** (storage +
checkpoint + manifest), **M** (Tier A correctness), **J#5** (dashboard
graph) **cerrados** en esta misma ventana. Crate version
`0.4.0` (Cargo.toml). 27 roles LLM wirados. 19 sub-comandos CLI
top-level. 11 migraciones SQLite (`v001`-`v011`). 199 archivos
`.rs` (~73 600 LoC).

## 2. Tabla de los últimos 20 PRs mergeados

(Orden cronológico inverso. Cada PR cierra uno o más items del
catálogo D.x o del roadmap prospectivo K.x.)

| #   | PR  | Track | D.x / spec | Capacidad |
|---:|----:|---|---|---|
|  1 | #195 | T7   | D.9.6 / D.19.13 / D.19.19 / D.19.20 / D.19.7 | provider + streaming |
|  2 | #193 | T6   | D.35.3-.5 / D.6.4 / D.1.4 | api-key + cache + outbox |
|  3 | #191 | T5   | D.14.6-.21 / D.15.2-.6 | CLI flags batch |
|  4 | #189 | T4   | D.12.10 / D.12.11 / D.16.2 / D.26.5 / D.29.9 | error enrichments |
|  5 | #188 | J#5  | D.33.1-4 / D.33.7 | dashboard graph + manifest extensions |
|  6 | #186 | T3   | D.17.1-4 / D.17.7-10 | exhaustive telemetry |
|  7 | #184 | T2   | D.28.1 | `reconcile(run_id)` per-run |
|  8 | #182 | T2   | D.11.12 / D.28.1 | `tool_versions` + reconcile per-run |
|  9 | #180 | T1   | — | Adversary 5 patterns + `RefineAction` + `StaleArtifact` + `BudgetCascade` |
| 10 | #178 | E8   | — | `PersonaPicker` + `AnglePicker` helpers opt-in |
| 11 | #176 | I+   | D.34.1 / D.13.2 / D.13.7 / D.13.10 / D.13.9 / D.13.19 | discovery resilience bundle |
| 12 | #174 | E7   | — | `TiefighterCritic` sidecar opt-in |
| 13 | #172 | M    | M#1 / M#2 | Tier A correctness + rubric validation |
| 14 | #170 | F4   | — | provider capabilities + wire formats unification |
| 15 | #168 | F5   | — | manifest v2 + `config_hash` + `tar.zst` export |
| 16 | #166 | F3   | — | `BudgetObserver` + optional-phase gating |
| 17 | #164 | F2   | — | run leases + heartbeat + zombie recovery |
| 18 | #162 | F1   | — | `Modify(text)` del checkpoint al rerank feedback |
| 19 | #160 | E5/E10 | — | sketch quality + hostile-prompt policy |
| 20 | #158 | E4/E6/E9 | — | `FinalDisagreement` + DAG layers + Intake normalize |

## 3. Deferred items restantes (no atacados)

Ordenados por valor/costo descendente.

### 3.1 K.4 ampliado (D.43, propuesta cuarta etapa)

- PDFs, JS rendering, auth, advanced rate-limiting.
- Acotado hoy a 4-host allowlist (`docs.rs`, `crates.io/api`,
  `github.com/api` + 1 configurable), 3 URLs/call, 4 KB/response,
  5s timeout.
- Trade-off en `docs/proposal-04-cuarta-etapa.md` §4.

### 3.2 DiscoveryCardinality (D.21.3 / J.2)

- `SelectionPlan::keep_top / keep_diverse / keep_outlier` queda como
  opt-in del catálogo. Hoy `SelectionPlan::apply` opera sobre
  Proposal text (PR #156) pero los helpers de diversity/outlier no
  están wirados.

### 3.3 Cardinality::for_mode (D.21.2 / J.1)

- `Mode -> Range<usize>` con soft/hard ceilings. No wired.

### 3.4 Quorum de judges por modo (D.21.7 / J.4)

- Tier A (PR #172) cubre pesos + validación, no el quorum matrix.

### 3.5 Dashboard `/api/lineage` dedicado (J#5 partial)

- PR #188 extendió `manifest.json` con `lineage_paths` y grafo en
  `/api/runs/<id>/graph`; un endpoint `/api/lineage` cross-run queda
  para una sub-fase posterior.

### 3.6 `cleanup_orphans` + `recover_zombies` startup (D.28.3 / D.28.4)

- `reconcile(run_id)` per-run (#184) cubre el caso de uso inmediato.
  El sweep startup queda para una sub-fase posterior.

### 3.7 `HardIncompat` extensión (D.13.15)

- PR #91 extendió `FailureKind` + `HardIncompat` parcialmente. El
  catálogo completo de pares incompatibles (D.13.15 list exhaustive)
  queda para una sub-fase posterior.

### 3.8 Path C — `Role::JsonRepairV2` re-call loop

- Track G paths A (#117) + B (#119) shippeados. Path C (LLM re-call
  con schema completo) documentado en
  `docs/research-json-structured-output.md` y queda deferred por
  decisión del usuario (no arreglar JSON output forzado, sólo
  hacerlo tolerante).

### 3.9 `seccomp`/`cgroup` Linux-only features on macOS / WSL

- Sandbox hardening (Tracks E) es Linux-only. Cross-platform fallback
  queda para v0.6+.

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
cargo test --all-targets                            # 0 failed (1406 lib + integration)
cargo build                                         # exit 0
```

### 4.4 Smoke gates

```bash
moagan run --mode fast --provider mock              # produce final/portfolio.md + rankings/ranking.json
moagan run --mode fast --provider minimax with MINIMAX_API_KEY  # +telemetry/calls.jsonl.gz
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

Tres tracks abiertos al cierre. Recomiendo atacar en este orden
(priorizado por valor/costo):

### 6.1 `HardIncompat` catálogo completo (D.13.15)

- M-sized (1 PR). Trade-off ya documentado en proposal-03 §D.13.15.
- Wiring: extender `src/domain/constraint.rs` con la lista exhaustiva
  (monolith ↔ microservices, sql ↔ nosql, blocking ↔ async runtime,
  etc.).
- Test: integration test que cubre cada par nuevo.

### 6.2 Dashboard `/api/lineage` cross-run endpoint (J#5 follow-up)

- M-sized (1 PR). PR #188 ya graficó per-run; falta el endpoint
  cross-run.
- Wiring: nuevo handler en `src/telemetry/view.rs`, query SQLite
  cross-run con `parent_run_id` joins.
- Test: integration test con 3 runs encadenados
  (`run_disc → run_deep → run_variant`).

### 6.3 DiscoveryCardinality wiring (D.21.3 / J.2)

- L-sized (1 PR, depende de #156 `SelectionPlan::apply`).
- Wiring: implementar `keep_top`, `keep_diverse`, `keep_outlier` en
  `SelectionPlan`; conectar desde `RankPhase`.
- Test: integration test con cardinalities 20/50/100 sobre el mismo
  set de proposals.

### 6.4 Deferred a v0.6+

- K.4 ampliado (PDFs, auth, JS rendering) — necesita decisión de
  producto.
- Path C JSON output (`Role::JsonRepairV2` re-call) — usuario
  explícito "no arreglar".
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
