# Moagan CLI — Cheatsheet

> Quick-reference for every `moagan` subcommand. Each section answers four questions: **what it does** (human description), **which flags interact** (combination matrix), **which Rust modules it touches** (internal flow), and **how it can fail** (exit codes).

The CLI surface lives in `src/cli/mod.rs`; each subcommand has its own `src/cli/<name>.rs` with the implementation details. The dispatcher in `src/cli/mod.rs::dispatch` wires clap → `Cmd` enum → handler function.

---

## 0. How to read this document

- **👁 What it is** — short prose (1–2 paragraphs), written for a human who has never run `moagan` before.
- **🧩 Flag matrix** — shows which flags interact (conflicts, dependencies, defaults). Read this before combining flags.
- **⚙️ Internal flow** — Rust modules called in order, plus which phases the pipeline runs for each mode. Read this when debugging.
- **❌ Errors / exit codes** — every `Error` variant that can surface from this command, mapped to its process exit code.

## 0.1 Global flag (applies to every subcommand)

| Flag | Type | Default | Notes |
|---|---|---|---|
| `--runs-dir <PATH>` | path | `$MOAGAN_RUNS_DIR` → `$MOAGAN_HOME` → `~/.local/share/moagan` | `global = true`. Aliases both env vars. |

**👁 What it is** — A single flag that lives on `Cli` and applies to every subcommand. Lets you point at a different `MOAGAN_HOME` to isolate experiments, move runs between machines, or avoid contaminating the primary home directory.

**⚙️ Internal flow** — `cli::Cli::runs_dir` → `MoaganHome::at(path)` or `MoaganHome::resolve()` (env cascade) → propagates to `MoaganHome` for `home.runs_dir()`, `home.meta_db_path()`, `home.run_dir(uuid)`, etc.

**❌ Errors** — `--runs-dir <relative-with-..>` → `Error::PathTraversal` (exit 2). Missing path with creatable parents: `home.ensure()` creates it.

## 0.2 Environment variables (resolution order)

```
1. CLI flag (when present)
2. MOAGAN_* env var
3. ~/.config/moagan/config.toml (merged with defaults)
4. Built-in defaults
```

Variables honoured by `src/config/mod.rs::apply_env_overrides` and `src/cli/flags_batch.rs`:

| Env var | Default | Scope |
|---|---|---|
| `MOAGAN_HOME` | `~/.local/share/moagan` | every subcommand |
| `MOAGAN_RUNS_DIR` | (alias of `MOAGAN_HOME`) | every subcommand |
| `MOAGAN_MAX_PARALLELISM` | `4` | `run`, `discover`, `rerun` |
| `MOAGAN_SKETCH_TIMEOUT` | `120` | sketch phase |
| `MOAGAN_PHASE_TIMEOUT` | `0` (infinite) | every phase |
| `MOAGAN_TOTAL_TIMEOUT` | `0` (infinite) | whole run |
| `MOAGAN_DEFAULT_PROVIDER` | `minimax` | `run`, `discover`, `rerun` |
| `MOAGAN_MINIMAX_ENDPOINT` | (empty) | providers kind=`minimax` |
| `MOAGAN_MINIMAX_MODEL` | (empty) | providers kind=`minimax` |
| `MOAGAN_REPAIR_MAX_ROUNDS` | `5` | repair phase |
| `MOAGAN_GATE_FORBIDDEN_TECHS` | (empty) | gate + validate |
| `MOAGAN_STARTUP_RECONCILE` | `true` | run/continue/discover boot |
| `MOAGAN_SANDBOX_ALLOW_NETWORK` | `false` | sandbox network |
| `MOAGAN_SANDBOX_ALLOW_INJECTION` | `false` | sandbox argv strip |
| `MOAGAN_SANDBOX_NETWORK_POLICY` | `off` | sandbox network |
| `MOAGAN_SANDBOX_NAMESPACES` | (empty) | sandbox |
| `MOAGAN_SANDBOX_SECCOMP` | `permissive` | sandbox |
| `MOAGAN_SANDBOX_CGROUP` | (empty) | sandbox |
| `MOAGAN_RESEARCH_ENABLED` | `false` | sketch phase |
| `MOAGAN_RESEARCH_URLS` | (empty) | sketch phase |
| `MOAGAN_RESEARCH_API_KEY` | (empty) | research bearer token |
| `MOAGAN_CRITIQUE_TIEFIGHTER_ENABLED` | `false` | critique phase |
| `MOAGAN_DISCOVERY_AUTO_PICKERS` | `true` | discovery coordinator |
| `MOAGAN_DISCOVERY_PERSONA_ENABLED` | `false` | discovery |
| `MOAGAN_DISCOVERY_ANGLE_ENABLED` | `false` | discovery |
| `MOAGAN_SELECTION_PLAN` | `keep_top(10)` | rank phase |
| `MOAGAN_HASH_ALGO` | `blake3` | export checksums |
| `MOAGAN_JSON_REPAIR_V2_ENABLED` | `false` | LLM JSON repair |
| `MOAGAN_LEARNING` | (unset) | `rate` (no-op when unset) |
| `MOAGAN_USER` | `default` | `rate` |
| `MOAGAN_QUIET` | (unset) | silence `.env` notice |
| `MOAGAN_FACET_CACHE_TTL_SECS` | `604800` (7d) | `discover --cache-facets` |
| `MOAGAN_INSPECT_JSON` | `0` | inspect JSON output |
| `MOAGAN_CONTINUE_FROM_PHASE` | (empty) | resume from phase |
| `MOAGAN_FORCE_EVAL` | `0` | force eval pass |
| `MOAGAN_BATCH_PROPOSALS` | (empty) | batch cardinality |
| `MOAGAN_TELEMETRY_VACUUM` | `0` | `telemetry cleanup` analogue |
| `MOAGAN_PHASE_L_TEST_PANIC` | (unset) | debug: forced panic |
| `MOAGAN_RATE_LIMIT_<provider>` | (empty) | per-provider token bucket |
| `MOAGAN_RESEARCH_RATE_LIMIT_<host>` | (empty) | per-host token bucket |

---

# Top-level subcommands

## 1. `moagan run`

**👁 What it is** — Starts the linear pipeline against your prompt. This is the binary's main entry point: it orchestrates `intake → clarify → route → [decompose] → sketches → proposals → validate → cluster → synthesize → gate → critique → repair → judge → adversary → rank → deliver`. It prints the run UUID at the end and leaves every result under `<MOAGAN_HOME>/.runs/<uuid>/` (manifest, proposals, evaluations, ranking, final portfolio).

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--mode deep` | adds `decompose` before sketch and turns `--adversary` on automatically |
| `--mode explore` | stops at sketches; no proposals/judge/rank/deliver |
| `--mode batch` | JSON-stable, no human pauses (overrides `--non-interactive`) |
| `--adversary` + `--mode deep` | redundant (deep already enables it) |
| `--no-replace-sources` | only applies in modes with `SynthesizePhase` (`standard`/`deep`/`batch`) |
| `--context-summary` or `--context-full` without `--context` | **error** → exit 2 (`InvalidArgs`) |
| `--context <uuid>` | must be a valid UUID v7 or a path to a `.md` file or directory |
| `--provider mock` + `--mock-dir` | loads JSON fixtures; without `--mock-dir` the mock exhausts immediately |
| `--profile <name>` | looks up `<name>.toml` under `$MOAGAN_HOME/profiles/` or `~/.config/moagan/profiles/` |
| `--hash-algo <x>` | only `sha256` or `blake3`; anything else → exit 2 |
| `--prompt -` | reads the prompt from stdin |
| `--max-parallelism > 64` | rejected by `validate_max_parallelism` |
| `--allow-injection` | disables the sandbox's secret-strip pass |
| `--model <alias>` (e.g. `minimax-m3`) | resolves to canonical `MiniMax-M3` when the alias is in `cfg.providers` and matches the kind |

**⚙️ Internal flow**

```
main → lib::run → cli::dispatch → match Cmd::Run { ... }
  → forbidden::check_local_cargo_toml()              // runtime guard
  → MoaganHome::resolve() / ::at(runs_dir)
  → should_reconcile_at_startup? yes → Config::load() + run_startup_reconcile()
  → validate (--context-summary without --context → InvalidArgs)
  → Config::load() → CLI flags apply (sandbox_allow_injection, hash_algo, profile, model)
  → if prompt == "-" → read_prompt_from_stdin()
  → cli::run::run(RunOptions{...}, &cfg)
       → MoaganHome + run_dir.ensure()
       → if context: context_resolver + context_loader
       → Db::open() + db.register_run() + db.add_context_ref() x N
       → run_full_pipeline(home, db, cfg, mock_dir, non_interactive, adversary, stub, prompt, ctx_block, max_parallelism)
            → parse_mode() + build_registry_for()
            → RedactPolicy { telemetry: cfg.redact_in_telemetry, ... }
            → Telemetry::open()
            → Parallelism::new(max_parallelism.unwrap_or(cfg.max_parallelism))
            → RunContext::new_with_config(...) + with_timeouts + with_interactive(!non_interactive && mode!=Batch) + with_context(...)
            → build_pipeline_for_mode(mode, cfg, replace_sources, adversary)
            → pipeline.run(&ctx) [tokio::select! with shutdown_signal]
            → telemetry.flush()
            → build_manifest() → read_intake_config_hash() → redact cli_prompt → AtomicWriter::write()
            → db.update_run_status("completed")
            → db.record_manifest_event("run.completed")
  → println!("run id: {uuid}")
```

Phases executed (via `build_pipeline_for_mode`):

| Mode | Phases |
|---|---|
| `fast` | intake, clarify, route, propose, gate, critique, repair, judge, [adversary OFF], rank, deliver |
| `standard` | + sketch, validate, cluster_proposals, synthesize, [adversary OFF] |
| `deep` | + decompose, [adversary ON auto] |
| `explore` | intake, clarify, route, sketch — ends here |
| `batch` | standard + validate (JSON-stable, no pauses) |

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| Missing `--prompt` | clap parse error | 2 |
| `--hash-algo foo` | `InvalidArgs` | 2 |
| `--context-summary` without `--context` | `InvalidArgs` | 2 |
| `--max-parallelism > 64` | `InvalidArgs` | 2 |
| Provider not in config | `InvalidArgs` ("provider 'x' is not in config") | 2 |
| API key missing (provider=minimax) | `InvalidApiKey` | 3 |
| Quota / HTTP 429 upstream | `PlanExhausted` | 4 |
| Phase / total timeout | `Timeout` | 5 |
| Ctrl-C | `Cancelled` / `Cancel` | 6 |
| Schema JSON invalid | `SchemaViolation` | 7 |
| I/O failure | `Io` | 8 |
| Token budget exhausted | `BudgetExhausted` | 9 |
| Budget exceeded | `BudgetExceeded` | 20 |
| Provider 5xx | `Provider` | 40 |
| Hostile prompt | `HostilePrompt` | 80 (ContextError) |
| SQLite poison | `Provider("sqlite: ...")` | 40 |
| SigInt (raw signal) | (n/a) | 130 |

---

## 2. `moagan continue`

**👁 What it is** — Resumes an existing run from where it left off (paused or failed). Reads the manifest, asks SQLite which phase was last completed, rebuilds the canonical pipeline for the run's mode, and restarts from there. Accepts extra flags to switch provider or API key mid-run, or to skip the "are you sure?" checkpoint.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| no `--run-id` | uses the most recent run in the DB |
| `--from-pause` | ignores `--kind` (always linear); requires `--run-id` |
| `--from-pause` + `--kind discovery` | clap conflict → exit 2 |
| `--kind discovery` + `manifest.mode = "linear"` | `InvalidArgs` ("requires manifest.mode = 'discover'") |
| `--kind discovery` + `manifest.mode = "discover"` | enters `discover::run_resume` |
| `--switch-provider <x>` not in config | `InvalidArgs` (validated up-front) |
| `--switch-api-key prompt:*` | `InvalidApiKey` (no-go list forbids interactive) |
| `--skip-checkpoint` | records `checkpoint:skipped` event without blocking |
| `--non-interactive` | checkpoints become `<skipped:non_interactive>` |
| Run with no completed phases | `InvalidState` ("nothing to continue") |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Continue { ... }
  → if from_pause: pause_cmd::run_continue_from_pause(home, parsed_run_id)
       → PausePoint::load(<run_dir>/paused.json)
       → println!("resume plan: paused at phase ...")
       → resume_paused_run() → Pipeline::resume(canonical, paused_at_phase)
            → continue_cmd::resume_pipeline(home, manifest, paused_at_phase, None, true)
       → finalize_resume(success) → removes paused.json + paused.lock
  → else: continue_cmd::run_continue(home, parsed, ContinueOptions{...})
       → home.ensure() + Db::open()
       → load_manifest(home, run_id)
       → validate --switch-provider ∈ cfg.providers (if present)
       → apply_continue_options(home, manifest, &opts, &db)
            → resolve --switch-api-key: env:/file:/literal → redact_api_key
            → db.next_provider_change_seq → db.record_provider_change (continue)
            → db.record_provider_change (api_key)
            → if skip_checkpoint → db.record_provider_change (skipped)
            → write_manifest_to_disk(home, &manifest)
       → db.last_completed_phase(run_id)? → last_phase
       → match kind:
            Linear   → resume_pipeline(home, manifest, last_phase, api_key, non_interactive)
            Discovery → discover::run_resume(home, manifest, last_phase, api_key, non_interactive)
```

`resume_pipeline` → `build_canonical_for_resume(cfg, mode)` → `Pipeline::resume_with_kind(canonical, last_phase, kind)` → `RunContext::new(...)` + `resumed.run(&ctx)` → `telemetry.flush()` → `build_manifest` + `write_manifest_to_disk` + `db.update_run_status`.

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `run_id` malformed | `InvalidArgs` | 2 |
| `--switch-provider` unknown | `InvalidArgs` | 2 |
| `--switch-api-key prompt:*` | `InvalidApiKey` | 3 |
| Run with no completed phases | `InvalidState` | 80 (ContextError) |
| `manifest.json` missing | `InvalidState` | 80 |
| Provider `minimax` + no API key at runtime | `InvalidApiKey` | 3 |
| Pipeline timeout / cancel | `Timeout` / `Cancelled` | 5 / 6 |

---

## 3. `moagan resume`

**👁 What it is** — Shortcut for `continue` without the provider / API-key switch flags. Useful for CI or any time you just want to "continue from where I was".

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| no flags | equivalent to `continue` with defaults |
| `--non-interactive` | checkpoints become non-blocking |

**⚙️ Internal flow** — `cli::dispatch → Cmd::Resume → continue_cmd::run_resume(home, parsed, non_interactive)` → `run_continue(home, run_id, ContinueOptions{non_interactive, ..Default::default()})`.

**❌ Errors** — Identical to `continue` (same internal path).

---

## 4. `moagan rerun`

**👁 What it is** — Clones a finished run under a new UUID and re-runs the full pipeline from intake (not a resume — starts from scratch). Useful for trying variations (different mode, provider, context) without losing the original run; the new one is linked back via `parent_run_id`.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--override-json` + `--matrix-override` | `--matrix-override` wins (preferred alias) |
| `--same-config` (default `true`) | keeps the original `execution_policy` + applies the override patches |
| JSON malformed in override | `InvalidArgs` ("invalid JSON: …") |
| Source run missing | `InvalidState` ("manifest.json not found") |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Rerun → continue_cmd::run_rerun(home, parsed, matrix_override.or(override_json))
  → load_manifest(home, run_id)
  → clone_manifest_for_rerun(old) → new_uuid, parent_run_id=old, status="created"
  → if override: apply_matrix_override(manifest, raw_json) + AtomicWriter::write(overrides_json_path)
  → home.run_dir(new_uuid).ensure()
  → write_manifest_to_disk(home, &new_manifest)
  → Db::open + register_run(new, mode, "created", ...) + add_run_sibling_relation(old, new, "rerun")
  → read_parent_raw_prompt() (from intake.json) + read_parent_context_block() (from brief.json)
  → run::run_full_pipeline(home, db, cfg, None, non_interactive=true, adversary=(mode==Deep), new_manifest, raw_prompt, context_block, max_parallelism=None)
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `--run-id` malformed | clap / `InvalidArgs` | 2 |
| JSON invalid in override | `InvalidArgs` | 2 |
| Source manifest missing | `InvalidState` | 80 |
| Parent intake missing | accepts empty (prompt empty + no context) | 0 (run ends with empty portfolio) |
| Pipeline timeout/cancel | `Timeout` / `Cancelled` | 5 / 6 |

---

## 5. `moagan import`

**👁 What it is** — Brings a complete run directory from another `MOAGAN_HOME` into the current one. Moves the folder and registers it in the local SQLite index. Useful for moving runs between machines or consolidating multiple workspaces.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--source-path <dir>` without `manifest.json` | `InvalidArgs` ("source manifest not found") |
| `--source-path ../foo` with `..` | `PathTraversal` (exit 2) |
| `--target-runs-dir` different from current | `InvalidArgs` ("must be <current runs dir>") |
| `run_id` already exists in destination | `InvalidState` ("rerun or remove first") |
| cross-device move | falls back to `copy_dir_recursive` + `fs::remove_dir_all` |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Import → continue_cmd::run_import(home, source_path, target_runs_dir)
  → safe_path(source_path.parent(), source_path)             // D.29.1
  → fs::read(source/manifest.json) → serde_json::from_slice::<Manifest>()
  → target_runs = target_runs_dir.unwrap_or(home.runs_dir())
  → if target_runs != home.runs_dir() → InvalidArgs
  → dest = target_runs / manifest.run_id
  → if dest.exists() → InvalidState
  → fs::create_dir_all(target_runs) + move_dir(safe_source, dest)
       → fs::rename (same FS) or copy_dir_recursive + fs::remove_dir_all (cross-device, EXDEV=18)
  → Db::open + register_run(...) + add_context_ref(...) x N
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| path with `..` | `PathTraversal` | 2 |
| `manifest.json` missing / invalid | `InvalidArgs` | 2 |
| `target_runs_dir` ≠ `home.runs_dir()` | `InvalidArgs` | 2 |
| Run already exists | `InvalidState` | 80 |
| I/O move/copy | `Io` | 8 |

---

## 6. `moagan inspect`

**👁 What it is** — Quick view of recent runs. Without `--run-id` it lists the last N runs (short id, mode, status, timestamps). With `--run-id` it drills into one and shows a summary of the warnings / auto-corrections that fired during the pipeline. Read-only and cheap — the right tool for "what did I do yesterday?".

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| no flags | top 10 runs |
| `--limit <N>` | limit to N |
| `--run-id <x>` | overrides `--limit`; shows run warnings |
| `--run-id` malformed | `InvalidArgs` |
| `--run-id` not in DB | `InvalidState` |
| `--verbose` with `--run-id` | adds timeline of every warning event |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Inspect → Db::open(home.meta_db_path)
  → if run_id: parsed.parse() → inspect::summarize_run(&db, parsed)
       → db.get_run(run_id)? → None → InvalidState
       → db.warnings_summary(run_id) + db.list_warnings(run_id)
       → inspect::print_run_summary(&summary, verbose)
  → else: inspect::list_recent(&db, limit) → println per row
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `run_id` malformed | `InvalidArgs` | 2 |
| Run not found | `InvalidState` | 80 |
| DB won't open | `Io` / `Provider("sqlite: ...")` | 8 / 40 |

---

## 7. `moagan refine`

**👁 What it is** — Works on a finished run. Two flavours: the **legacy** form (`--proposal`) re-issues the deliver prompt for a single proposal and writes the result to `final/refined_<id>.md`; the **new** form (`--action`) applies one of seven catalogue actions (tighten constraint, add evidence, split / merge proposals, etc.) and modifies the manifest in place. Useful for post-mortem iteration on a run.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--proposal` + `--action` | clap conflict → exit 2 |
| neither `--proposal` nor `--action` | clap `required_unless_present` → exit 2 |
| `--action tighten-constraint` + `--verdict-detail ""` | detail ignored (empty) |
| `--action drop-proposal` without a proposal_id | dispatcher cannot apply (enum doesn't carry one) → message |
| `--mock-dir` + `--action` | ignored (legacy only) |
| `--run-id` malformed / run missing | `InvalidArgs` / `InvalidState` |
| `--action rerun-critique` | log-only today (future wire-up) |

Supported actions (kebab-case): `tighten-constraint`, `add-evidence`, `split-proposal`, `merge-proposal`, `rerun-critique`, `drop-proposal`, `request-human-input`.

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Refine
  → parsed run_id
  → if action:
       continue_cmd::run_refine_action(run_id, action, verdict_detail, &home)
         → RefineContext::from_run(home, run_id)
         → inject verdict_detail (non-empty)
         → phases::refine::dispatch_refine_action(action, ctx)
         → if TightenConstraint: load_manifest + write prohibited_decisions + manifest_blake3_recompute + write_manifest_to_disk
         → plan.emit_telemetry() (StaleArtifact on RequestHumanInput)
         → return RefineActionOutcome
  → else if proposal:
       continue_cmd::run_refine(run_id, proposal, &cfg, &home, mock_dir)
         → load_manifest
         → resolve proposal_path or revision_path (repair if it exists)
         → Telemetry::open + Parallelism + RunContext
         → ctx.call_with_retry_parse(Role::Deliver, system, user, "FinalReport:{...}", 5)
         → std::fs::write final/refined_<id>.md + refined_<id>.json
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `proposal_id` missing in run | `InvalidArgs` | 2 |
| Manifest missing | `InvalidState` | 80 |
| Retry parse exhausted | `SchemaViolation` | 7 |
| Upstream 5xx | `Provider` | 40 |

---

## 8. `moagan rerank`

**👁 What it is** — Re-computes the ranking of a finished run using the current judge configuration. Reads the existing `evaluations/p_*.json`, runs `RankPhase` with the current weights, and overwrites `rankings/ranking.json`. Useful when you change `ranking_weights` in config and want to see the effect without spending LLM tokens.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `run_id` malformed | `InvalidArgs` |
| Run not found | `InvalidState` |
| `evaluations/p_*.json` missing | RankPhase produces an empty ranking |

**⚙️ Internal flow** — `cli::dispatch → Cmd::Rerank → continue_cmd::run_rerank(run_id, &cfg, &home)` → `load_manifest` (uses the run's original provider) → `phases::RankPhase { config, replace_sources_enabled: true, stability_enabled: cfg.stability.enabled }` → `Telemetry::open` + `Parallelism` + `build_registry_for` + `RunContext` → `phase.execute(&ctx)`.

**❌ Errors** — Same as `continue` for resolving provider/model; schema mismatch in evaluations → `SchemaViolation` (7).

---

## 9. `moagan validate`

**👁 What it is** — Pre-flight gate without the LLM. Reads a brief JSON from disk, runs the 12 structural checks from `GatePhase`, and returns exit `0` on pass or `1` on fail (with issues printed to stderr). Designed for CI: rejects bad briefs before spending tokens on a real run.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `<brief>` with `..` or symlink outside parent | `PathTraversal` (exit 2) |
| file missing | `InvalidArgs` ("brief not found") (exit 2) |
| JSON malformed | `InvalidArgs` ("not valid JSON") (exit 2) |
| hard / soft issues present | exit 1 + issues to stderr |
| brief valid | exit 0 + "PASS" |
| `--mode` (any value) | informational today; doesn't affect the check |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Validate → validate::run(ValidateArgs { brief_path, mode })
  → parse_brief(path):
       → safe_path(parent, path)                                  // D.29.1
       → fs::read_to_string → if NotFound → InvalidArgs
       → serde_json::from_str::<Brief> → on failure → InvalidArgs
  → Config::load() → forbidden techs (lowercased)
  → synthetic_proposal(brief) = Proposal{ summary: brief.problem, ... }
  → phases::gate::structural_check(proposal, brief, &forbidden, min_length, max_length)
  → if pass: println("PASS") → Ok(0)
  → else: eprintln issues/missing + "FAIL" → Ok(1)
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| path traversal | `PathTraversal` | 2 |
| file missing | `InvalidArgs` | 2 |
| JSON malformed | `InvalidArgs` | 2 |
| Other I/O | `Io` | 8 |
| hard / soft issues | (no error) | 1 |
| pass | (no error) | 0 |

---

## 10. `moagan diff`

**👁 What it is** — Side-by-side comparison of two runs. Calculates eleven base metrics (tokens, calls, errors, providers, phases, warnings, duration) plus four filesystem-aware metrics (`proposals`, `evaluations`, `phases_visited`, ranking delta). Three output formats: `text` for the terminal, `md` to paste into an issue / PR, `json` for pipelines. Self-diff is rejected.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `<run_a>` == `<run_b>` | `InvalidArgs` ("cannot diff a run against itself") |
| run not in DB | `InvalidState` |
| `--format` absent | default `text` |
| `--format md` | markdown table |
| `--format json` | parseable JSON |
| `--include-proposals` with `format=text` | adds lines per proposal |
| `--include-proposals` with `format=md` | adds sub-table with score delta |
| `--include-proposals` with `format=json` | adds `changed[]` with score_a / score_b / delta |
| `<run_a>` or `<run_b>` not UUID v7 | `InvalidArgs` |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Diff → diff::run(DiffArgs { run_a, run_b, format, include_proposals, home_override: None })
  → parse_run_id(run_a) + parse_run_id(run_b) + reject self-diff
  → MoaganHome::resolve() + Db::open
  → row_a, row_b, agg_a, agg_b (sqlite)
  → count_files_in(<run_a>/proposals/), evaluations/  (filesystem)
  → db.list_completed_phases(<id>).len()
  → ranking_delta(<home>, a, b): load_ranking() + diff_rankings()
  → match format:
       Text → print_text (uses compare_helpers::print_side_by_side + print_diff + filesystem metrics)
       Md   → print_md (pasteable table)
       Json → print_json (stable object)
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `run_id` malformed | `InvalidArgs` | 2 |
| self-diff | `InvalidArgs` | 2 |
| run not found | `InvalidState` | 80 |
| filesystem I/O | `Io` | 8 |
| DB won't open | `Io` / `Provider` | 8 / 40 |

---

## 11. `moagan repair`

**👁 What it is** — Reconciles the filesystem (authority) against the SQLite index when they have drifted. Three orthogonal operations you activate independently: clean up orphans (`*.tmp.*`, stale `*.lock`), re-index artefact counters, and recover "zombies" (runs marked `running` but stale for 2 h). All support `--dry-run`; destructive ones require `--yes` or return exit `10`.

**🧩 Flag matrix** — **At least one of** `--cleanup-orphans`, `--reindex-artifacts`, `--recover-zombies` (no flag → `InvalidArgs` exit 2).

| Operation | Flags | Without `--yes` | Without `--dry-run` |
|---|---|---|---|
| `--cleanup-orphans` | destructive | exit `10` (`NeedsInput`) if plan non-empty | applies directly |
| `--reindex-artifacts` | not destructive | applies directly | applies directly |
| `--recover-zombies` | mutates DB | emits `outbox` events | mutates DB directly |

Additional:

| Combination | Behaviour |
|---|---|
| `--run <id>` malformed | `InvalidArgs` |
| `--dry-run` + `--cleanup-orphans` | prints plan without deleting |
| `--dry-run` + `--recover-zombies` | lists zombies without touching DB or outbox |
| no operation flag | `InvalidArgs` |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Repair → repair::run(RepairArgs { cleanup_orphans, reindex_artifacts, recover_zombies, yes, run, dry_run, home_override: None })
  → MoaganHome::resolve() + Db::open
  → if cleanup_orphans: handle_cleanup_orphans(home, dry_run, yes)
       → reconcile::plan_cleanup_for_report(home) → if empty: "nothing to do"
       → if dry_run: return count
       → if !yes: NeedsInput (exit 10)
       → reconcile::cleanup_orphans(home) → deletes
  → if reindex_artifacts: handle_reindex_artifacts(home, db, dry_run)
       → resolve_target_runs_for_reindex (walk filesystem)
       → for each (run, kind ∈ proposals/sketches/evaluations/critiques):
            → count_artefacts_in_dir(disk) vs db.count_<kind>
            → if drift: reindex_<kind>(db, run_id, run_root)
  → if recover_zombies: handle_recover_zombies(db, dry_run)
       → reconcile::list_zombie_run_ids(db) (status='running' AND updated_unix < now-7200s)
       → if dry_run: print, no mutation
       → reconcile::recover_zombies(db) → db.update_run_status("interrupted") + outbox event "run.zombie_recovered"
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| no operation flag | `InvalidArgs` | 2 |
| `--run <id>` malformed | `InvalidArgs` | 2 |
| `--cleanup-orphans` with plan, no `--yes` | `NeedsInput` | 10 |
| walk / DB I/O | `Io` / `Provider` | 8 / 40 |

---

## 12. `moagan doctor`

**👁 What it is** — Quick environment check before spending an API key: that `MINIMAX_API_KEY` is set, that `MOAGAN_HOME` resolves and is writable, that `meta.sqlite` opens, and a list of providers + models configured. Prints one `[OK]/[WARN]/[FAIL]` line per check for easy grepping. Exit `0` if everything OK, `1` if any FAIL.

**🧩 Flag matrix** — No flags.

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Doctor → doctor::run()
  → Config::load()
  → emit(check_provider_config)              // {N} provider(s) configured or WARN if empty
  → for each (kind, models) in models_per_provider(cfg):
       emit(Check { label: "models for provider '<kind>'", status, detail: models.join(", ") })
  → emit(check_api_key)                       // MINIMAX_API_KEY if any provider kind="minimax"
  → emit(check_home)                          // MoaganHome::resolve + ensure + write probe + cleanup
  → emit(check_sqlite)                        // Db::open
  → exit:
       any_fail → "doctor: FAIL" + Ok(1)
       any_warn → "doctor: WARN" + Ok(0)
       else     → "doctor: OK"   + Ok(0)
```

**❌ Errors / exit codes**

| Case | Exit |
|---|---|
| any `[FAIL]` (api_key, home, sqlite) | 1 |
| only `[WARN]` (providers empty, home missing → sqlite skipped) | 0 |
| everything OK | 0 |

---

## 13. `moagan audit`

**👁 What it is** — External, transparent audit trail via a sidecar HTTP process on loopback. **`proxy`** sits between the binary and the upstream provider, appending every request / response to `external_audit.jsonl.gz`. **`verify`** cross-checks that log against the internal `calls.jsonl.gz` + SQLite and emits a TSV with the verdict. Useful when you need independent proof of what calls actually happened.

### 13.1 `moagan audit proxy`

**👁 What it is** — Sidecar process that listens on loopback and forwards every request to the real upstream, leaving an append-only JSONL trail of everything that crossed the boundary.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--listen-host` not loopback | `InvalidArgs` ("audit proxy must listen on a loopback address") |
| `--run-id` absent | "auto" mode: discovers new runs as they appear |
| `--exclude-bodies` | logs without `body_canonical`, only `body_sha256` |
| `--max-body-bytes <N>` | rejects bodies > N (default 32 MiB) |
| `--timeout-secs <N>` | upstream HTTP timeout (default 180 s) |
| SIGINT / SIGTERM | clean shutdown + flush |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Audit → audit::proxy_cmd(ProxyArgs)
  → resolve_run(runs_dir, run_id, require_run_id=false)
       → MoaganHome::resolve/at + ensure
       → if run_id Some: parse → Some(RunId)
       → if None: None (auto mode)
  → SocketAddr parse
  → validate is_loopback()
  → ProxyConfig { listen, upstream, runs_dir, run_id, include_bodies=!exclude_bodies, upstream_timeout, max_body_bytes, refuse_loopback_forward=false, ... }
  → proxy::start(cfg) → handle
  → println!("proxy listening on http://... -> ... run id: ... runs dir: ...")
  → tokio::signal::ctrl_c + unix::SIGTERM → handle.shutdown()
```

### 13.2 `moagan audit verify`

**👁 What it is** — Cross-check: compares the sidecar JSONL against `calls.jsonl.gz` + SQLite, writes a TSV with the result.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--run-id` absent | picks the latest run from the filesystem; if none exists → TSV with `audit_file_missing=true`, exit 2 |
| `run_id` malformed | `InvalidArgs` |
| TSV write fails | exit 2 |
| mismatches / orphans | exit 1 |
| everything OK | exit 0 |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Audit::Verify → audit::verify_cmd(VerifyArgs)
  → resolve_run(runs_dir, run_id, require_run_id=true)
  → if no run_id: VerifyReport{audit_file_missing:true} → print TSV → Ok(2)
  → run_dir.telemetry/calls.jsonl.gz
  → verify_mod::verify_with_db(&run_dir, &calls_path, &db)
       → on SQLite failure: report.internal_file_invalid=true + verify(&run_dir, &calls_path)
  → write_tsv(&report, &run_dir/external_audit_verify.tsv)
  → println TSV + eprintln path
  → exit report.exit_code() (0 ok / 1 mismatch / 2 missing/invalid / 90 export_failed)
```

**❌ Errors `audit`** — Read / write I/O → `Io` (8); mismatch → exit 1; missing / invalid → exit 2.

---

## 14. `moagan discover`

**👁 What it is** — "Knowledge base" mode instead of "pick a winner". Generates an exploration matrix (roles × models × temperatures), fans out ≥ 80 sketches via a coordinator with saturation control, tags them, clusters them with SimHash, detects cross-cluster contradictions, derives facets, and integrates them into `final/cat_NN.md` + `final/summary.md`. Does **not** produce `ranking.json` — the output is the map, not the verdict.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--cardinality < 80` | `InvalidArgs` ("below the discovery minimum of 80") |
| `--cardinality 0` | `InvalidArgs` |
| `--cardinality` absent | default 80 |
| `--dimensions × --facets-per-dimension` | matrix size = `dimensions × facets` |
| `--cluster-threshold` outside `[0, 1]` | parse error → unexpected behaviour |
| `--cache-facets` | cross-run cache keyed by `sha256(brief + category_id)`, TTL `MOAGAN_FACET_CACHE_TTL_SECS` |
| `--non-interactive` | intake without TTY → every checkpoint becomes skipped |
| `--provider mock` + `--mock-dir` | loads JSON fixtures |
| `--mode`-style flag | n/a; discovery is its own subcommand |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Discover → discover::run(DiscoverOptions, &cfg)
  → MoaganHome + run_dir.ensure()
  → build_registry_for(cfg, default_provider, mock_dir)
  → Db::open + register_run(mode="discover", status="running")
  → Telemetry::open
  → RunContext::new(...) + with_timeouts + with_interactive(!non_interactive)
  → build_pre_matrix_pipeline() = intake + clarify → pipeline.run()   [tokio::select! Ctrl-C]
  → DiscoveryCoordinator::new(home, run_id, cancel, Brief::default(), "deployment-model:serverless", Mode::Fast)
  → coordinator.run_with_ctx_and_target(ctx, Some(cardinality))
       → persona_picker (if cfg.discovery.persona_enabled)
       → angle_picker (if cfg.discovery.angle_enabled && angle_clusters_min)
       → loop with SaturationTracker until cardinality or saturation:
            → spawn sketch LLM call (matrix entry)
            → save sketches/sk_NN.json
       → DiscoveryOutcome { sketches_completed, sketches_failed }
  → build_post_matrix_pipeline(opts) = tag → cluster → contradict → facet → extract → integrate → summary
       → pipeline.run()  [tokio::select! Ctrl-C]
  → telemetry.flush() + db.update_run_status("completed")
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `--cardinality < 80` | `InvalidArgs` | 2 |
| DiscoveryQualityTooLow (> 50% failed with min attempts) | `DiscoveryQualityTooLow` | 80 |
| HostilePrompt detected in intake | `HostilePrompt` | 80 |
| Provider 5xx sustained | `Provider` + circuit-open | 40 |
| `TelemetryVacuum` during cleanup | n/a (different sub) | n/a |
| SigInt | `Cancelled` | 6 |

---

## 15. `moagan telemetry`

**👁 What it is** — Read-only inspection and export of telemetry. **`list`** (improved alias of inspect), **`summary`** (totals + per phase + per model), **`compare`** (diff between two runs), **`provider`** (configured plans + recent usage), **`view`** (local HTTP dashboard), **`export`** (bundle the run as `tar.gz` / `tar` / `zip` / `tar.zst` + SHA256SUMS), **`cleanup`** (apply retention), **`verify`** (re-hash against SHA256SUMS), **`config`** (print effective config without filtering API keys).

### 15.1 `moagan telemetry list`

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--run <x>` malformed | `InvalidArgs` |
| `--run` not found | `InvalidState` |
| `--limit <N>` with `--run` | ignored |
| no flags | top 10 |

**⚙️ Internal flow** — `resolve_home(runs_dir)` → `Db::open` → if `--run`: drill into one (`get_run`, `run_aggregate`, `list_phase_summaries_for_run`, `list_provider_usage_for_run`); else: `db.list_runs(limit)` + table. Exit 0.

### 15.2 `moagan telemetry summary`

**🧩 Flag matrix** — `--run` required; malformed → `InvalidArgs`; missing → `InvalidState`.

**⚙️ Internal flow** — `db.get_run` + `run_aggregate` + `provider_usage` + `phase_summaries` + `dir_bytes(run_root)` → print Run / Mode / Status / Duration / Tokens / Calls / Phases / Warnings / Checkpoints / Disk + "By model" + "By phase" (durations aggregated).

### 15.3 `moagan telemetry compare`

**🧩 Flag matrix** — `run-a` / `run-b` required; malformed → `InvalidArgs`; missing from DB → `InvalidState`.

**⚙️ Internal flow** — `compare::run` → `print_side_by_side` + 11 `print_diff` (tokens, calls, ok / error / timeout / cancelled, providers, phases, warnings, checkpoints, duration_secs).

### 15.4 `moagan telemetry provider`

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| no flags | lists (default action) |
| `--list` | lists (same as no flags) |
| `--plan <x>` not in config | `InvalidArgs` |
| `--plan <x>` in config | shows kind / endpoint / model / max_tokens / temperature / top_p / hard_incompatibilities + last 20 runs |

**⚙️ Internal flow** — `Config::load()` + `Db::open` + match.

### 15.5 `moagan telemetry view`

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--port 0` | kernel-assigned |
| `--port 4096` (default) + `cfg.server.port` different | honours `cfg.server.port` |
| `--port N` (≠ 4096) | CLI wins |
| `cfg.server.host` invalid | `InvalidArgs` |
| `cfg.server.ensure_home = false` + `runs_dir/` missing | `InvalidState` |
| Ctrl-C | shutdown |

**⚙️ Internal flow** — `Config::load()` + `home.ensure()?` + `SocketAddr::new(host.parse, port)` + `DashboardConfig { bind, home, db_path: None }` + `dashboard::start(cfg).await` → loop 60 s until Ctrl-C. Endpoints: `/api/runs`, `/api/runs/<id>`, `/api/runs/<id>/phases|calls|provider_usage|hashes|export`.

### 15.6 `moagan telemetry export`

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--level <x>` | `summary` (default) or `full`; other → `InvalidArgs` |
| `--format <x>` | `tar.gz` (default) / `tar` / `zip` / `tar.zst` (aliases: `tgz`, `tzst`); other → `InvalidArgs` |
| `--out` absent | next to the run dir as `<run_short>_<level>.<ext>` |
| run not found | `InvalidState` |
| `run_id` malformed | `InvalidArgs` |

**⚙️ Internal flow** — `run_dir` + `export::export_run(run_dir, run_id, level, format, out_path)` → `ExportResult { file_count, payload_bytes, archive_bytes, archive_sha256, archive_path }` → println.

### 15.7 `moagan telemetry cleanup`

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `--dry-run` | lists without touching |
| `--archive` | moves to `<root>/archive/YYYY-MM-DD/<run_id>/` instead of deleting |
| nothing | applies `Config::retention.policy` (delete by default) |
| `MOAGAN_TELEMETRY_VACUUM=1` | future hint analogue; no action today |

**⚙️ Internal flow** — `Config::load()` + `RetentionConfig { keep_runs_days, keep_runs_count, max_storage_bytes, policy }` + `apply(runs_dir, db_lookup, &cfg, dry_run)` → list candidates or apply.

### 15.8 `moagan telemetry verify`

**🧩 Flag matrix** — `--path <PATH>` required (exported directory with `SHA256SUMS`); exit 0 on OK / `Error::InvalidState` on mismatches (exit 80).

**⚙️ Internal flow** — `verify::verify(path)` → walk every entry, re-hash, mark `Ok` / `Mismatch { expected, actual }` → println per row + summary `OK: N verified, M failed`.

### 15.9 `moagan telemetry config`

**🧩 Flag matrix** — No operative flags.

**⚙️ Internal flow** — `Config::load()` → println in blocks (providers, parallelism, timeouts, privacy, stability, export, gate, server, retention, default_provider). **Never** prints API keys (`SecretString` values do not leave the registry).

**❌ Errors `telemetry`** — DB won't open → `Io` (8) / `Provider("sqlite: ...")` (40); `run_id` malformed → `InvalidArgs` (2); run not found → `InvalidState` (80); export verify mismatch → `InvalidState` (80) or `ExportVerificationFailed` (90).

---

## 16. `moagan pause`

**👁 What it is** — Serialises an active run's state to `<run_dir>/paused.json` with a `paused.lock` (TTL 5 min). Built for cross-process hibernation: shut down the machine, run `continue --from-pause` the next day, and the pipeline picks up where it left off. The lockfile prevents two pauses from competing on the same run.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `<run_id>` not present in filesystem | `InvalidArgs` ("run <id> not found") |
| concurrent pause within TTL (5 min) | `InvalidArgs` ("paused.lock held (age Xs, ttl 300s)") |
| pause on stale lockfile (> TTL) | overwrites it |
| run registered in DB | uses `last_completed_phase` + `list_completed_phases` |
| run NOT registered in DB | falls back to defaults (`paused_at_phase="synthesize"`, legacy list) |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Pause → pause_cmd::run_pause(home, PauseArgs { run_id, phase: None, completed: None })
  → run_dir.root().exists()? if not → InvalidArgs
  → acquire_lock(run_dir, TTL=300)
       → if lock.exists() && age < TTL → InvalidArgs
       → std::fs::write(paused.lock, "locked")
  → resolve_pause_state(home, &args):
       → db.try_open → if registered: derive_paused_at_phase + derive_completed_phases
       → else: defaults (resume::DEFAULT_PAUSED_AT_PHASE + DEFAULT_COMPLETED_PHASES)
  → PausePoint::new(run_id, paused_at_phase, completed_phases, json!({"resumable":true}), summary)
  → pp.save(run_dir.root())
  → println "paused run <id> at phase '<name>' (N completed phases)"
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| run not found | `InvalidArgs` | 2 |
| lock held (within TTL) | `InvalidArgs` | 2 |
| I/O writing paused.json / lock | `Io` | 8 |

---

## 17. `moagan list`

**👁 What it is** — Today it only understands `--paused`: enumerates every run id under `<home>/.runs/` that has a `paused.json`. For the full run listing (including non-paused runs) use `moagan inspect`.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| without `--paused` | `InvalidArgs` ("today only supports --paused; use inspect for full listing") |
| `--paused` with `.runs/` directory missing | tolerated, prints "(no paused runs)" |
| `--paused` with paused runs | one per line |

**⚙️ Internal flow** — `pause_cmd::run_list(home, ListArgs)` → walk `home.runs_dir()` → for each dir: if `paused.json` exists, print `file_name`.

**❌ Errors** — Without `--paused` → `InvalidArgs` (2); I/O reading the dir is tolerated (missing dir treated as empty).

---

## 18. `moagan rate`

**👁 What it is** — Manually rate one concrete proposal from one concrete run with a score in `[0.0, 1.0]`. Persists the rating into the user's preference cache (`$MOAGAN_USER` or `default`). No-op when `MOAGAN_LEARNING` is unset. Designed for the learning loop: "I liked this proposal" → future runs have the rank phase remember it.

**🧩 Flag matrix**

| Combination | Behaviour |
|---|---|
| `<run_id>` malformed | `InvalidArgs` |
| `<score>` outside `[0.0, 1.0]` | `InvalidArgs` ("must be in [0.0, 1.0]") |
| `<score>` non-numeric | `InvalidArgs` |
| `MOAGAN_LEARNING` unset | rating is **not** persisted (silent no-op) |

**⚙️ Internal flow**

```
cli::dispatch → Cmd::Rate → rate::run(RateArgs { run_id, proposal_id, score })
  → user = env::var("MOAGAN_USER").unwrap_or("default")
  → RunId::from_str(run_id) → bounds check
  → score: f64 = parse + check 0.0..=1.0
  → Rating { proposal_id, score, rated_unix, run_id }
  → preferences::integration::record_user_rating(&user, rating)
  → println "rated <proposal> = <score> for run <id>"
```

**❌ Errors / exit codes**

| Case | Error | Exit |
|---|---|---|
| `run_id` malformed | `InvalidArgs` | 2 |
| score outside range | `InvalidArgs` | 2 |
| score non-numeric | `InvalidArgs` | 2 |
| preferences I/O | `Io` | 8 |

---

# Appendix A — Master exit-code table

(T01-06 §12.3 + extended catalog)

| Code | Meaning | Source |
|---|---|---|
| `0` | OK | all |
| `1` | generic error (CLI dispatch wrapper) / `validate` FAIL / `doctor` FAIL | dispatcher, validate, doctor |
| `2` | `InvalidArgs` (including `PathTraversal`) | almost all |
| `3` | `InvalidApiKey` | provider missing |
| `4` | `PlanExhausted` (HTTP 429) | upstream rate-limit |
| `5` | `Timeout` | phase / total timeout |
| `6` | `Cancelled` (Ctrl-C) | shutdown signal |
| `7` | `SchemaViolation` | JSON schema mismatch |
| `8` | `Io` | filesystem |
| `9` | `BudgetExhausted` | token budget |
| `10` | `NeedsInput` (destructive plan without `--yes`) | `repair --cleanup-orphans` |
| `20` | `BudgetExceeded` | configured cap |
| `30` | `PlanPaused` | provider decision |
| `40` | `Provider` / `MockExhausted` | upstream 5xx, mock exhausted |
| `50` | `TimeoutExit` (extended) | n/a today |
| `60` | `InvalidArgsExit` (extended) | n/a today |
| `70` | `IoErrorExit` (extended) | n/a today |
| `80` | `ContextError` (`InvalidState`, `LockHeld`, `Cache`, `DiscoveryQualityTooLow`, `HostilePrompt`) | several |
| `90` | `ExportVerificationFailed` | `telemetry verify`, `audit verify` |
| `91–96` | `Storage`, `Llm`, `Sandbox`, `Research`, `Resume`, `Discovery` (extended) | n/a today |
| `130` | SIGINT (raw signal) | shell |

# Appendix B — Observable output per command

| Command | Primary stdout | Side effects |
|---|---|---|
| `run` | `run id: <uuid>` | `<home>/.runs/<uuid>/{manifest,brief,intake,proposals,evaluations,critiques,sketches,revisions,rankings,final}/` + `meta.sqlite` row |
| `continue` / `resume` | (silent or "resuming after phase X") | run updated |
| `rerun` | `moagan run <new> mode=... provider=... -> <dir>` | new run + sibling relation |
| `import` | `moagan import <id> -> <dest>` | run moved + DB row |
| `inspect` | table or warnings summary | (read-only) |
| `refine --proposal` | `refined proposal <id> for run <id>` | `final/refined_<id>.{md,json}` |
| `refine --action` | `refine action '<x>' applied to run <id>` (+ prohibited_decisions if applicable) | `manifest.json` rewritten |
| `rerank` | `reranked run <id>` | `rankings/ranking.json` rewritten |
| `validate` | `validate: PASS/FAIL` (+ issues to stderr) | (read-only) |
| `diff` | text / md / json | (read-only) |
| `repair` | `repair (dry-run|applied): cleanup=N reindex=N zombies=N` | filesystem + DB rows + outbox events |
| `doctor` | `[OK] / [WARN] / [FAIL]` lines + verdict | (read-only, writes probe then removes it) |
| `audit proxy` | `proxy listening on http://...` | `<run_dir>/telemetry/external_audit.jsonl.gz` |
| `audit verify` | TSV rows + `OK: N verified, M failed` | `<run_dir>/external_audit_verify.tsv` |
| `discover` | `discovery run id: <uuid>` | `<run_dir>/{tags,clusters,facets,extractions,final(cat_NN.md, summary.md)}/` |
| `telemetry list/summary/compare/provider/config` | tables / text | (read-only) |
| `telemetry view` | `dashboard listening on http://...` + endpoints list | HTTP dashboard running |
| `telemetry export` | `export: wrote N file(s)` | archive + SHA256SUMS |
| `telemetry cleanup` | `(no runs match...)` or candidate list | filesystem + DB |
| `telemetry verify` | `OK: N files verified, M failed` | (read-only) |
| `pause` | `paused run <id> at phase '<name>' (N completed phases)` | `<run_dir>/paused.{json,lock}` |
| `list` | one run id per line or `(no paused runs)` | (read-only) |
| `rate` | `rated <proposal> = <score> for run <id>` | preference cache |

---

# Appendix C — Subcommand cheat sheet (one-liner)

| Command | One-liner |
|---|---|
| `moagan run` | Start a new linear pipeline |
| `moagan continue` | Resume a paused / failed run (with switch flags) |
| `moagan resume` | Resume a paused / failed run (no switch flags) |
| `moagan rerun` | Clone a finished run and re-execute from intake |
| `moagan import` | Move a run from another `MOAGAN_HOME` |
| `moagan inspect` | List recent runs or drill into one |
| `moagan refine` | Re-deliver one proposal, or apply a refine action |
| `moagan rerank` | Re-compute the ranking of a finished run |
| `moagan validate` | Pre-flight gate (no LLM) for a brief JSON |
| `moagan diff` | Side-by-side comparison of two runs |
| `moagan repair` | Reconcile filesystem vs SQLite |
| `moagan doctor` | Verify the local environment |
| `moagan audit proxy` | Run the sidecar HTTP recorder |
| `moagan audit verify` | Cross-check sidecar vs internal calls |
| `moagan discover` | Run the discovery / knowledge-base pipeline |
| `moagan telemetry list` | List recent runs (alias of inspect, richer output) |
| `moagan telemetry summary` | Per-run aggregates (tokens, calls, by-phase, by-model) |
| `moagan telemetry compare` | Diff two runs (baseline 11 metrics) |
| `moagan telemetry provider` | Provider plans + recent per-provider usage |
| `moagan telemetry view` | Read-only HTTP dashboard on `127.0.0.1:<port>` |
| `moagan telemetry export` | Bundle a run as `tar.gz` / `tar` / `zip` / `tar.zst` + SHA256SUMS |
| `moagan telemetry cleanup` | Apply retention policy |
| `moagan telemetry verify` | Re-hash an exported bundle against `SHA256SUMS` |
| `moagan telemetry config` | Print effective configuration (no API keys) |
| `moagan pause` | Serialize current run state to `paused.json` |
| `moagan list` | Enumerate runs with `paused.json` |
| `moagan rate` | Manually rate a proposal (preference cache) |