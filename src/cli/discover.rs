//! `moagan discover` — discovery mode.
//!
//! One pipeline: intake → `discover_dimensions` (only when the model
//! derives the matrix) → `discover_sketches` → `discover_render`. A
//! fresh run first records the operator's choices in
//! `discover_run.json`; `moagan continue --kind discovery` runs the
//! same pipeline on the same run dir, and every phase skips the work
//! whose artefact already exists.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, trace, warn};

use crate::cli::flags_batch;
use crate::config::Config;
use crate::discovery::matrix::TemperatureProfile;
use crate::discovery::matrix_spec::MatrixSpec;
use crate::discovery::run_spec::DiscoverRunSpec;
use crate::domain::Intake;
use crate::error::{Error, Result};
use crate::execution::Parallelism;
use crate::fs_layout::MoaganHome;
use crate::ids::RunId;
use crate::llm::Role;
use crate::phases::Pipeline;
use crate::phases::RunContext;
use crate::phases::discover_sketches::EXPLORATION_MATRIX_FILENAME;
use crate::phases::{
    DiscoverDimensionsPhase, DiscoverIntakePhase, DiscoverRenderPhase, DiscoverSketchesPhase,
};
use crate::redact::RedactPolicy;
use crate::storage::sqlite::Db;
use crate::telemetry::Telemetry;

/// F2 (Track G.2) default `sketches_per_cell`. The matrix's
/// per-cell fan-out is `cells() * sketches_per_cell`. F2 lowers
/// the v0.5 floor from `cardinality = 80` to
/// `sketches_per_cell = 10` so the per-cell fan-out is the
/// explicit knob (an operator who wants 80 sketches on a 4×2
/// matrix sets `sketches_per_cell = 20`).
pub const DEFAULT_SKETCHES_PER_CELL: usize = 10;

/// F2 minimum allowed `sketches_per_cell`. Used by the CLI
/// dispatcher's `--sketches-per-cell` validator AND the
/// `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env-var parser so both
/// surfaces reject the same floor. F2.x (v0.13.2) lowered the
/// operator-facing floor to 1 to support debug / integration
/// runs; default is unchanged at 10 to preserve the v0.5
/// cardinality contract for nominal discovery runs.
pub const MIN_SKETCHES_PER_CELL: usize = 1;

/// Parse and validate the operator's `--matrix-spec` inputs.
/// Returns `Ok(None)` when every entry is empty (the caller falls
/// back to LLM-derive or the legacy count pair). Returns
/// `Err(Error::InvalidArgs)` on the first malformed entry so the
/// dispatcher surfaces a clear CLI message.
fn parse_matrix_spec_inputs(entries: &[String]) -> Result<Option<MatrixSpec>> {
    let non_empty: Vec<&String> = entries.iter().filter(|s| !s.trim().is_empty()).collect();
    if non_empty.is_empty() {
        trace!(
            entries = entries.len(),
            "parse_matrix_spec_inputs: all empty"
        );
        return Ok(None);
    }
    let parsed = MatrixSpec::parse_all(non_empty.into_iter().cloned())?;
    parsed.validate()?;
    Ok(Some(parsed))
}

/// Merge the CLI options over the config's `[discovery_matrix]` block
/// into the run spec. CLI wins: `--provider` (else the config default),
/// `--matrix-spec` when given, each count when given, `--llm-derive`
/// (or the config flag), `--sketches-per-cell`, `--mock-dir`,
/// `--max-parallelism`; temperature profiles are the config map with
/// each CLI profile inserted under its `section::model` key (a profile
/// without a section takes the `--provider` section; last wins); the
/// default profile is the config's. Fails with `Error::InvalidArgs` on
/// a malformed `--matrix-spec` or a provider that is not
/// `SECTION:MODEL`.
pub fn resolve_spec(opts: &DiscoverOptions, cfg: &Config) -> Result<DiscoverRunSpec> {
    let provider = if opts.provider.is_empty() {
        cfg.default_provider.clone()
    } else {
        opts.provider.clone()
    };
    let (section, _) = provider_pair(&provider)?;
    parse_matrix_spec_inputs(&opts.matrix_spec)?;
    let block = &cfg.discovery_matrix;
    let mut temperature_profiles: BTreeMap<String, TemperatureProfile> = block
        .temperature_profiles
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for profile in &opts.temperature_profiles {
        let (sec, model) = profile.into_pair(&section);
        temperature_profiles.insert(
            crate::llm::ProviderRegistry::registry_key(&sec, &model),
            profile.clone().into_matrix_profile(),
        );
    }
    Ok(DiscoverRunSpec {
        provider,
        mock_dir: opts.mock_dir.clone(),
        max_parallelism: opts.max_parallelism,
        sketches_per_cell: opts.sketches_per_cell,
        matrix_spec: if opts.matrix_spec.is_empty() {
            block.matrix_spec.clone()
        } else {
            opts.matrix_spec.clone()
        },
        dimensions: opts.dimensions.or(block.dimensions),
        facets_per_dimension: opts.facets_per_dimension.or(block.facets_per_dimension),
        llm_derive: opts.llm_derive || block.llm_derive_first,
        temperature_profiles,
        default_profile: block.default_profile.clone(),
    })
}

/// The discover pipeline: [`DiscoverIntakePhase`], then
/// [`DiscoverDimensionsPhase`] only when `spec.derives_dimensions()`
/// and `exploration_matrix.json` is not persisted yet, then
/// [`DiscoverSketchesPhase`] and [`DiscoverRenderPhase`].
pub fn discover_pipeline(spec: &DiscoverRunSpec, matrix_persisted: bool) -> Pipeline {
    let mut pipeline = Pipeline::new().push(DiscoverIntakePhase);
    if spec.derives_dimensions() && !matrix_persisted {
        pipeline = pipeline.push(DiscoverDimensionsPhase);
    }
    pipeline
        .push(DiscoverSketchesPhase)
        .push(DiscoverRenderPhase)
}

/// `(section, model)` of a `SECTION:MODEL` provider; a bare section is
/// rejected with `Error::InvalidArgs` (there is no implicit model).
fn provider_pair(provider: &str) -> Result<(String, String)> {
    if !provider.contains(':') {
        warn!(provider = %provider, "discover: bare SECTION without model");
        return Err(Error::InvalidArgs(format!(
            "--provider '{provider}' is a bare section name; \
             pass the explicit SECTION:MODEL form (e.g. \
             --provider {provider}:MODEL_ID). No implicit \
             'first model' fallback in v0.10+."
        )));
    }
    crate::cli::probe::parse_provider_model(provider)
}

/// Options for `moagan discover`.
#[derive(Debug, Clone, Default)]
pub struct DiscoverOptions {
    /// Provider name (must be in config).
    pub provider: String,
    /// User prompt.
    pub prompt: String,
    /// Optional override of the home directory.
    pub home: Option<PathBuf>,
    /// Optional directory of canned mock responses.
    pub mock_dir: Option<PathBuf>,
    /// F2 (Track G.2): sketches per matrix cell. The matrix
    /// fan-out is `cells() × sketches_per_cell ×
    /// profile_total`. Default 10; floor 1 (replaces the
    /// v0.5 `cardinality = 80` contract; floor lowered from
    /// 10 in v0.13.2). The CLI's `--sketches-per-cell` flag
    /// is the canonical operator-facing knob; the
    /// `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env var and the
    /// `[discovery_matrix].sketches_per_cell` TOML key are
    /// merge-order fall-backs.
    pub sketches_per_cell: usize,
    /// Optional override of the global parallel cap.
    pub max_parallelism: Option<usize>,
    /// F1: target dimension count (no default — `None` means the
    /// LLM picks freely).
    pub dimensions: Option<usize>,
    /// F1: target facets per dimension (no default — `None` means
    /// the LLM picks asymmetrically per dimension).
    pub facets_per_dimension: Option<usize>,
    /// F1: operator-supplied matrix spec (repetible and
    /// consolidated). When non-empty, the LLM-derive path is
    /// skipped.
    pub matrix_spec: Vec<String>,
    /// F1: force the LLM-derive path even when the operator did
    /// not pass a spec.
    pub llm_derive: bool,
    /// Output directory for the run. Defaults to MOAGAN_HOME resolution.
    pub out_dir: Option<PathBuf>,
    /// Non-interactive: every checkpoint is a `<skipped:non_interactive>`
    /// marker instead of blocking on stdin. Required for CI / smoke
    /// runs where stdin is not a TTY.
    pub non_interactive: bool,
    /// PR-D1: per-provider sampling-temperature profiles sourced
    /// from the `--temperature-profile` CLI flag (last-wins per
    /// provider model) merged with the persisted `[discovery]`
    /// block from `~/.config/moagan/config.toml`. The CLI specs
    /// win on conflict — the operator's explicit invocation
    /// beats the persisted default. When this list is empty AND
    /// the persisted `[discovery]` block is empty, the matrix
    /// uses the default `[1.0] × 1` profile (the v0.5 single-shot
    /// contract).
    pub temperature_profiles: Vec<TemperatureProfileSpec>,
    /// F3 (Track G.2): `--explain` flag from the CLI. The
    /// dispatcher reads this to short-circuit before the
    /// pipeline starts; the field is kept on the options struct
    /// so `discover_explain::build_and_format` can be called
    /// from a single helper with the full picture. Default
    /// `false` so existing call sites stay unchanged.
    pub explain: bool,
}

/// Parsed CLI form of a per-provider temperature profile (PR-D1,
/// Tanda 04e D-1).
///
/// The clap `Vec<String>` for `--temperature-profile` is parsed
/// into this typed form once at the dispatcher boundary so the
/// downstream matrix / coordinator code consumes validated,
/// type-safe values. The spec grammar is
/// `provider=<ref>;temperatures=<csv>;replicas=<n>` where `<ref>`
/// accepts two forms:
///
/// * `provider=<model>` — legacy form (PR-D1). The section is
///   implicit and defaults to the `--provider` section at merge
///   time. The lookup key on the matrix becomes
///   `<section>::<model>`.
/// * `provider=<section>:<model>` — Tanda 04e D-1 form. The
///   section is explicit; the parser splits on the first `:`
///   via `parse_provider_model`. The lookup key on the matrix
///   becomes `<section>::<model>` directly.
///
/// Other segments:
///
/// * `temperatures=<csv>` — REQUIRED. Comma-separated floats in
///   `0.0..=2.0`. At least one value required.
/// * `replicas=<n>` — REQUIRED. Integer `>= 1`.
///
/// Multiple `--temperature-profile` flags for the same `(section,
/// model)` pair are allowed; the LAST spec wins (documented
/// behaviour so the audit can pin the merge order).
#[derive(Debug, Clone, PartialEq)]
pub struct TemperatureProfileSpec {
    /// Provider MODEL name (the matrix's lookup key when no
    /// explicit section is supplied). For the legacy form this
    /// is the bare model string (e.g. `MiniMax-M3`,
    /// `mimo-v2.5`); for the new `<section>:<model>` form this
    /// is the model half (e.g. `MiniMax-M3` with section
    /// `minimax`). Case-sensitive.
    pub provider: String,
    /// Explicit provider SECTION name when the operator used the
    /// `provider=<section>:<model>` form. `None` means the
    /// legacy form was used and the section is implicit (the
    /// `cli::discover::run` merge step substitutes the active
    /// `--provider` section before persisting the profile).
    pub section: Option<String>,
    /// Sampling temperatures the loop iterates per `(cell,
    /// replica)` pair. Always non-empty (the parser enforces it).
    pub temperatures: Vec<f32>,
    /// Replicas per `(cell, temperature)` pair. Always `>= 1`.
    pub replicas_per_temperature: usize,
}

impl TemperatureProfileSpec {
    /// Parse a single CLI spec into the typed form. Returns a
    /// [`crate::error::Error::InvalidArgs`] error on every malformed
    /// input (missing `provider=`, out-of-range temperature, etc.)
    /// so the dispatcher surfaces the message through the same
    /// channel as the other CLI validators (D.15.5 pattern).
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        debug!(spec = s, "TemperatureProfileSpec::parse: enter");
        let mut provider: Option<String> = None;
        let mut section: Option<String> = None;
        let mut temperatures: Option<Vec<f32>> = None;
        let mut replicas: Option<usize> = None;
        for kv in s.split(';') {
            let kv = kv.trim();
            if kv.is_empty() {
                warn!(spec = s, "empty segment in temperature-profile spec");
                return Err(crate::error::Error::InvalidArgs(format!(
                    "empty `key=value` segment in temperature-profile spec {s:?}"
                )));
            }
            let (k, v) = kv.split_once('=').ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "expected `key=value` in temperature-profile spec segment {kv:?} \
                     (full spec: {s:?}); grammar is \
                     `provider=<name|section:model>;temperatures=<csv>;replicas=<n>`"
                ))
            })?;
            let key = k.trim();
            let value = v.trim();
            match key {
                "provider" => {
                    if value.is_empty() {
                        return Err(crate::error::Error::InvalidArgs(format!(
                            "provider name is empty in temperature-profile spec {s:?}"
                        )));
                    }
                    // Tanda 04e D-1: the value can be either a bare
                    // model (`MiniMax-M3`, `mimo-v2.5`) — the legacy
                    // form — or `<section>:<model>`. The split is
                    // delegated to `parse_provider_model` so the
                    // canonical "section name + single colon + no
                    // extra colons" contract is honoured (matches
                    // `moagan probe <section>:<model>`). On success
                    // we record the explicit section; on failure the
                    // value is treated as a bare model name (the
                    // legacy form).
                    match crate::cli::probe::parse_provider_model(value) {
                        Ok((sec, mdl)) => {
                            section = Some(sec);
                            provider = Some(mdl);
                        }
                        Err(_) => {
                            section = None;
                            provider = Some(value.to_owned());
                        }
                    }
                }
                "temperatures" => {
                    let parsed = value
                        .split(',')
                        .map(|t| t.trim())
                        .filter(|t| !t.is_empty())
                        .map(|t| {
                            t.parse::<f32>().map_err(|e| {
                                crate::error::Error::InvalidArgs(format!(
                                    "invalid temperature {t:?} in temperature-profile \
                                     spec {s:?}: {e}"
                                ))
                            })
                        })
                        .collect::<crate::error::Result<Vec<f32>>>()?;
                    if parsed.is_empty() {
                        return Err(crate::error::Error::InvalidArgs(format!(
                            "temperatures list is empty in temperature-profile spec {s:?}"
                        )));
                    }
                    for t in &parsed {
                        if !(*t >= 0.0 && *t <= 2.0) {
                            return Err(crate::error::Error::InvalidArgs(format!(
                                "temperature {t} out of range 0.0..=2.0 in \
                                 temperature-profile spec {s:?}"
                            )));
                        }
                    }
                    temperatures = Some(parsed);
                }
                "replicas" => {
                    let parsed = value.parse::<usize>().map_err(|e| {
                        crate::error::Error::InvalidArgs(format!(
                            "invalid replicas {value:?} in temperature-profile spec {s:?}: {e}"
                        ))
                    })?;
                    if parsed == 0 {
                        return Err(crate::error::Error::InvalidArgs(format!(
                            "replicas must be >= 1 in temperature-profile spec {s:?}; got 0"
                        )));
                    }
                    replicas = Some(parsed);
                }
                other => {
                    return Err(crate::error::Error::InvalidArgs(format!(
                        "unknown key {other:?} in temperature-profile spec {s:?}; \
                         expected `provider`, `temperatures`, or `replicas`"
                    )));
                }
            }
        }
        let out = Self {
            provider: provider.ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "missing `provider=<name>` in temperature-profile spec {s:?}"
                ))
            })?,
            section,
            temperatures: temperatures.ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "missing `temperatures=<csv>` in temperature-profile spec {s:?}"
                ))
            })?,
            replicas_per_temperature: replicas.ok_or_else(|| {
                crate::error::Error::InvalidArgs(format!(
                    "missing `replicas=<n>` in temperature-profile spec {s:?}"
                ))
            })?,
        };
        trace!(
            provider = %out.provider,
            section = ?out.section,
            temperatures = out.temperatures.len(),
            replicas = out.replicas_per_temperature,
            "TemperatureProfileSpec::parse: ok"
        );
        Ok(out)
    }

    /// Resolve the `(section, model)` pair the profile should be
    /// keyed under on the matrix. When the spec carries an explicit
    /// `section` (the `provider=<section>:<model>` form), the value
    /// is returned verbatim. Otherwise the supplied
    /// `default_section` is substituted (the legacy form's section
    /// is implicit — it matches the active `--provider` section).
    pub fn into_pair(&self, default_section: &str) -> (String, String) {
        (
            self.section
                .clone()
                .unwrap_or_else(|| default_section.to_owned()),
            self.provider.clone(),
        )
    }

    /// Convert into the matrix's `TemperatureProfile` (the form
    /// stored on `ExplorationMatrix`). Drops the `provider`
    /// string because the matrix indexes the profile map by
    /// provider model name, and the matrix owns that key.
    pub fn into_matrix_profile(self) -> crate::discovery::matrix::TemperatureProfile {
        crate::discovery::matrix::TemperatureProfile {
            temperatures: self.temperatures,
            replicas_per_temperature: self.replicas_per_temperature,
        }
    }
}

/// F2 (B2): resolve every `(section, model)` pair the discovery
/// run may dispatch against, so the provider registry hosts all
/// of them before the coordinator's fan-out starts.
///
/// The list is the default `--provider SECTION:MODEL` pair plus
/// one entry per key in the MERGED
/// `[discovery_matrix].temperature_profiles` map — merged meaning
/// "persisted TOML block with the CLI `--temperature-profile`
/// specs already applied on top". Deriving the list from the
/// merged map (instead of from the CLI specs alone) is what
/// makes a TOML-only pair reachable: the pre-fix code hosted
/// only the CLI pairs, so the coordinator panicked in
/// `RunContext::provider_for` the first time it dispatched
/// against a TOML-configured provider.
///
/// The key → pair mapping mirrors
/// [`crate::discovery::matrix::ExplorationMatrix::active_provider_profiles`]
/// exactly: a joined `section::model` key splits on `"::"`, and a
/// legacy bare-model key is attributed to `default_section`. Keys
/// are visited in sorted order so the resulting list (and thus
/// the probe fan-out order) is deterministic despite the
/// `HashMap` backing the profile map. The default pair always
/// comes first and duplicates are dropped.
fn active_pairs_for(
    default_section: &str,
    default_model: &str,
    temperature_profiles: &std::collections::HashMap<
        String,
        crate::discovery::matrix::TemperatureProfile,
    >,
) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> =
        vec![(default_section.to_owned(), default_model.to_owned())];
    let mut keys: Vec<&String> = temperature_profiles.keys().collect();
    keys.sort();
    for key in keys {
        let (section, model) = match key.split_once("::") {
            Some((sec, mdl)) => (sec.to_owned(), mdl.to_owned()),
            None => (default_section.to_owned(), key.clone()),
        };
        if !pairs.iter().any(|(s, m)| s == &section && m == &model) {
            trace!(
                section = %section,
                model = %model,
                joined_key = %key,
                "discover: active pair from temperature profile"
            );
            pairs.push((section, model));
        }
    }
    pairs
}

/// Start a fresh discover run `run_id`: resolve the run spec from
/// `opts` and `cfg`, write it to `discover_run.json` and the normalised
/// prompt to `prompt.md`, then run the pipeline. Returns `run_id`.
pub async fn run(opts: DiscoverOptions, cfg: &Config, run_id: RunId) -> Result<RunId> {
    debug!(
        provider = %opts.provider,
        sketches_per_cell = opts.sketches_per_cell,
        non_interactive = opts.non_interactive,
        run_id = %run_id,
        "discover::run: enter"
    );
    let home = Arc::new(match opts.home.clone() {
        Some(path) => MoaganHome::at(path),
        None => MoaganHome::resolve()?,
    });
    home.ensure()?;
    let spec = resolve_spec(&opts, cfg)?;
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;
    spec.save(run_dir.root())?;
    crate::atomic::writer::AtomicWriter::new().write(
        &run_dir.prompt(),
        crate::phases::intake::normalize_raw_prompt(&opts.prompt).as_bytes(),
    )?;
    info!(run_id = %run_id, "discover: allocated run directory");
    execute(home, &spec, &opts.prompt, cfg, run_id, opts.non_interactive).await?;
    Ok(run_id)
}

/// Continue the discover run `run_id` under `home` with the choices it
/// started with ([`DiscoverRunSpec::load`]) and its stored prompt
/// (`prompt.md`, else `brief.json#raw_prompt`). Finished work is
/// skipped, so a complete run makes no model call and only re-renders
/// `final/`. Fails with `Error::InvalidState` when the run dir is
/// missing or holds no spec.
pub async fn resume(home: &MoaganHome, run_id: RunId, non_interactive: bool) -> Result<()> {
    let run_dir = home.run_dir(run_id);
    if !run_dir.root().is_dir() {
        return Err(Error::InvalidState(format!(
            "no discover run {run_id} under {}",
            home.runs_dir().display()
        )));
    }
    let spec = DiscoverRunSpec::load(run_dir.root())?;
    let prompt = stored_prompt(run_dir.root());
    let cfg = Config::load()?;
    info!(run_id = %run_id, provider = %spec.provider, "discover: resuming");
    execute(
        Arc::new(home.clone()),
        &spec,
        &prompt,
        &cfg,
        run_id,
        non_interactive,
    )
    .await
}

/// The prompt a run was started with: `prompt.md`, else
/// `brief.json#raw_prompt`, else empty.
fn stored_prompt(run_dir: &Path) -> String {
    let prompt_md = std::fs::read_to_string(run_dir.join("prompt.md")).unwrap_or_default();
    if !prompt_md.trim().is_empty() {
        return prompt_md;
    }
    crate::phases::util::read_json::<Intake>(&run_dir.join("brief.json"))
        .map(|brief| brief.raw_prompt)
        .unwrap_or_default()
}

/// Run [`discover_pipeline`] for `spec` on run `run_id`: build the
/// provider registry for every active `(section, model)` pair, wait for
/// the probe tables, register (or re-mark) the run as running, run the
/// pipeline (Ctrl-C cancels it) and mark the run completed. Fresh runs
/// and resumed runs both end here.
async fn execute(
    home: Arc<MoaganHome>,
    spec: &DiscoverRunSpec,
    prompt: &str,
    cfg: &Config,
    run_id: RunId,
    non_interactive: bool,
) -> Result<()> {
    let (default_provider_section, default_model) = provider_pair(&spec.provider)?;
    let default_provider = spec.provider.clone();
    let run_dir = home.run_dir(run_id);
    run_dir.ensure()?;

    // The spec is the single source of the matrix knobs; every phase
    // reads them from `ctx.config.discovery_matrix`.
    let mut effective_cfg = cfg.clone();
    spec.apply_to(&mut effective_cfg.discovery_matrix);

    let active_pairs = active_pairs_for(
        &default_provider_section,
        &default_model,
        &effective_cfg.discovery_matrix.temperature_profiles,
    );
    debug!(
        active_pairs = active_pairs.len(),
        "discover: active (section, model) pairs resolved"
    );
    // Throttle governors and circuit breakers are keyed by SECTION.
    let throttle_sections: Vec<String> = {
        let mut sections: Vec<String> = Vec::new();
        for (section, _) in active_pairs.iter() {
            if !sections.iter().any(|s| s == section) {
                sections.push(section.clone());
            }
        }
        sections
    };
    let providers = Arc::new(super::run::build_registry_for_with_active(
        cfg,
        &default_provider,
        spec.mock_dir.as_deref(),
        None,
        Some(&active_pairs),
        Some(&home),
    )?);
    debug!(
        providers = providers.len(),
        "discover: provider registry built"
    );
    let max_tokens_table = providers.max_tokens_table().cloned();
    let temperature_table = providers.temperature_table().cloned();
    let param_rejections = providers.param_rejections().cloned();

    let policy = RedactPolicy::default();
    let db = Db::open(&home.meta_db_path())?;
    if db.has_run(run_id)? {
        db.update_run_status(run_id, "running")?;
    } else {
        db.register_run(
            run_id,
            "discover",
            "running",
            env!("CARGO_PKG_VERSION"),
            None,
            None,
            None,
        )?;
    }
    let telemetry = Telemetry::open(run_id, &run_dir, policy, Some(db.clone()))?;
    if let Some(n) = spec.max_parallelism {
        flags_batch::validate_max_parallelism(n).map_err(Error::InvalidArgs)?;
    }
    let resolved_parallelism = spec.max_parallelism.unwrap_or(cfg.max_parallelism);
    debug!(resolved_parallelism, "discover: parallelism resolved");
    let parallelism = Parallelism::new(resolved_parallelism);

    // Wire-the-cost-overrides plan (closes #970): load the
    // operator-authored `<MOAGAN_HOME>/cost_overrides.toml` so the
    // cost-estimator call sites in `phases/phase.rs` consult it
    // BEFORE the catalog. Same logic as `cli/run.rs`: missing file
    // is the safe default (v0.18.1 status quo preserved); malformed
    // file degrades to no-overrides with a `tracing::warn!` so the
    // operator gets a one-line breadcrumb without aborting the run.
    //
    // IMPORTANT: the sidecar is a global operator preference, NOT a
    // per-run artifact. We deliberately resolve `MoaganHome::resolve()`
    // (the GLOBAL home from `$MOAGAN_HOME` or `$HOME/.local/share/moagan/`)
    // rather than reusing the local `home` variable above, because
    // `--runs-dir` re-roots `home` to the per-run directory and the
    // operator's hand-authored sidecar lives in the global home, not
    // next to every run they produce. The other auto-discovered
    // tables (`max_tokens_auto.toml`, `temperatures_auto.toml`, ...)
    // follow the same pattern via `providers.max_tokens_table()` which
    // already uses the global home.
    let global_cost_overrides_path = MoaganHome::resolve().ok().map(|h| h.cost_overrides_path());
    let cost_overrides = match global_cost_overrides_path.as_ref() {
        Some(path) => match crate::llm::cost::CostOverrides::from_path(path) {
            Ok(table) => {
                if table.is_empty() {
                    tracing::debug!(
                        path = %path.display(),
                        "cost_overrides: no rows configured; cost_estimate falls through to catalog (v0.18.1 status quo)"
                    );
                    None
                } else {
                    Some(Arc::new(table))
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %path.display(),
                    stage = "cost_overrides.load.failed",
                    "cost_overrides.toml failed to load; proceeding without overrides (cost_estimate falls through to catalog)"
                );
                None
            }
        },
        None => {
            tracing::warn!(
                stage = "cost_overrides.resolve.failed",
                "could not resolve global MoaganHome; proceeding without overrides"
            );
            None
        }
    };

    let ctx = RunContext::new_with_config(
        run_id,
        Arc::clone(&home),
        Arc::clone(&providers),
        default_provider_section.clone(),
        default_model,
        parallelism,
        telemetry.clone(),
        prompt.to_owned(),
        "discover".to_owned(),
        Arc::new(effective_cfg.clone()),
    )
    .with_timeouts(
        effective_cfg.phase_timeout_secs,
        effective_cfg.total_timeout_secs,
    )
    .with_max_tokens_table_opt(max_tokens_table)
    .with_temperature_table_opt(temperature_table)
    .with_param_rejections_opt(param_rejections)
    .with_cost_overrides_opt(cost_overrides.clone())
    .with_interactive(!non_interactive)
    // Per-role rate-limit (catalog        ): wire each
    // `[rate_limit_per_role]` entry into a `RateLimiter` keyed by
    // the parsed `Role`. Unknown role names are silently skipped
    // so a stale config never aborts the run; the per-role bucket
    // then throttles the chatty roles (e.g. `tagger` in the
    // post-matrix fan-out) without affecting the per-provider
    // bucket the rest of the pipeline uses.
    .with_role_rate_limits({
        let mut rate_limit_per_role: std::collections::HashMap<_, _> =
            std::collections::HashMap::new();
        for (role_name, cfg) in &effective_cfg.rate_limit_per_role {
            if let Ok(role) = role_name.parse::<Role>() {
                rate_limit_per_role.insert(
                    role,
                    std::sync::Arc::new(crate::llm::rate_limiter::RateLimiter::new(cfg.clone())),
                );
            }
        }
        rate_limit_per_role
    })
    // v0.9.6: per-`role` adaptive throttle governors. Each
    // `[throttle_per_role]` entry is a `ThrottleConfig` keyed by
    // `Role`. The default-constructed `GovernorRegistry` returns a
    // default-config governor the first time an unknown role is
    // called, so omitting the entry matches the v0.9.5 default
    // (no adaptive backpressure).
    //
    // F2 (B3): the pre-creation walks every active `(section,
    // model)` pair instead of the default provider alone. The
    // multi-provider dispatch path looks the governor up with
    // `governor_for_at(section, role)`, so a pair that was never
    // pre-created silently fell back to a default-config
    // (lenient) governor and ignored `[throttle_per_role]`. The
    // registry is keyed by SECTION (not by the joined
    // `section::model`), which is also why the pre-fix
    // `default_provider` key — the raw `--provider
    // SECTION:MODEL` string — never matched: `RunContext`
    // stores `default_provider = SECTION`.
    .with_throttle_governors({
        let mut throttle = crate::llm::governor::GovernorRegistry::new();
        for section in throttle_sections.iter() {
            for (role_name, cfg) in &effective_cfg.throttle_per_role {
                if let Ok(role) = role_name.parse::<Role>() {
                    throttle.with_config_for(
                        section,
                        role,
                        crate::llm::governor::ThrottleConfig::from(cfg.clone()),
                    );
                }
            }
        }
        throttle
    })
    // v0.9.6: per-`(provider, role)` circuit breakers. Each
    // `[circuit_breaker_per_role]` entry is a `BreakerConfig` keyed
    // by `Role`. The provider key is the SECTION so the lookup
    // matches what the `ThrottleGovernor` and the
    // per-`(provider, role)` breaker share.
    //
    // F2 (B3): same fan-out as the governors above — every
    // active section gets its configured breaker, so a
    // non-default provider no longer falls through to the
    // lenient default config.
    .with_breakers_per_role({
        let mut breakers = crate::llm::circuit_breaker::BreakerRegistry::new();
        for section in throttle_sections.iter() {
            for (role_name, cfg) in &effective_cfg.circuit_breaker_per_role {
                if let Ok(role) = role_name.parse::<Role>() {
                    breakers.pre_create(
                        section,
                        role,
                        crate::llm::circuit_breaker::BreakerConfig::from(*cfg),
                    );
                }
            }
        }
        breakers
    });

    // Gate the first LLM call behind the auto-probe tables so the
    // intake call does not race the max_tokens probe, and the
    // `<MOAGAN_HOME>/*_auto.toml` sidecars land before the run exits.
    if let Some(table) = ctx.max_tokens_table.as_ref() {
        table.await_ready().await;
    }
    if let Some(table) = ctx.temperature_table.as_ref() {
        table.await_ready().await;
    }
    if let Some(table) = ctx.top_p_table.as_ref() {
        table.await_ready().await;
    }
    if let Some(table) = ctx.top_k_table.as_ref() {
        table.await_ready().await;
    }
    let matrix_persisted = run_dir.root().join(EXPLORATION_MATRIX_FILENAME).is_file();
    let pipeline = discover_pipeline(spec, matrix_persisted);
    info!(run_id = %run_id, phases = ?pipeline.names(), "discover: pipeline started");
    let pipeline_future = pipeline.run(&ctx);
    tokio::pin!(pipeline_future);
    tokio::select! {
        result = &mut pipeline_future => { result?; }
        _ = tokio::signal::ctrl_c() => {
            warn!(run_id = %run_id, "discover: shutdown signal received");
            ctx.cancel().cancel(crate::cancel::CancelReason::UserInterrupt);
            return Err(ctx.cancel().into_error());
        }
    }

    telemetry.flush()?;
    debug!(run_id = %run_id, "discover: telemetry flushed");
    if let Err(e) = db.update_run_status(run_id, "completed") {
        warn!(run_id = %run_id, error = %e, "discover: failed to update run status");
    }
    // The human-readable banner only goes to a terminal so a piped
    // stdout stays pure NDJSON.
    if std::io::stdout().is_terminal() {
        let mut stdout = std::io::stdout().lock();
        let _ = write_discover_banner(
            &mut stdout,
            &run_id.short(),
            &default_provider,
            &run_dir.root().display(),
        );
    } else {
        info!(
            run_id = %run_id,
            provider = %default_provider,
            run_dir = %run_dir.root().display(),
            "discover: completed (banner suppressed because stdout is non-TTY)"
        );
    }
    info!(run_id = %run_id, "discover: completed");
    Ok(())
}

/// Build the human-readable discover banner (the line that prints
/// `moagan discover <id> provider=<name> -> <path>`). Public-ish so
/// unit tests can capture it through `Vec<u8>` without touching the
/// real stdout. The shape is exactly the format string the
/// production print statement at L886 emits; if you change one,
/// change both (and update `write_discover_banner_emits_expected_shape`).
fn write_discover_banner<W: std::io::Write>(
    out: &mut W,
    run_id_short: &str,
    provider: &str,
    run_dir: &dyn std::fmt::Display,
) -> std::io::Result<()> {
    writeln!(
        out,
        "moagan discover {} provider={} -> {}",
        run_id_short, provider, run_dir
    )
}

#[cfg(test)]
mod pipeline_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// PR-B1 (B1.4) lifted to u32::MAX: `discover` validates
    /// `--max-parallelism` against the same helper as `run`, which
    /// now caps at `u32::MAX` (`4_294_967_295`). One above that
    /// bound is rejected with the documented message; the
    /// hard-cap-of-64 history is preserved in the helper's
    /// `flags_batch::validate_max_parallelism` test (which is
    /// the source of truth for the cap and its message).
    #[test]
    fn max_parallelism_cap_holds_for_discover() {
        // Exactly the cap: accepted.
        assert!(flags_batch::validate_max_parallelism(4_294_967_295).is_ok());
        // One above the cap: rejected with the documented message.
        let err = flags_batch::validate_max_parallelism(4_294_967_296).expect_err("must error");
        assert!(
            err.contains("exceeds maximum 4_294_967_295"),
            "error must mention the cap; got {err:?}"
        );
    }

    // ---- PR-D1: TemperatureProfileSpec parser tests ----

    /// PR-D1: the minimal spec (`provider=...;temperatures=<one>;
    /// replicas=<n>`) parses to the typed form. The operator's
    /// `mimo-v2.5` / `[0.5]` / `2` example from the spec.
    #[test]
    fn parse_temperature_profile_spec_minimal() {
        let spec = TemperatureProfileSpec::parse("provider=foo;temperatures=0.5;replicas=2")
            .expect("minimal spec must parse");
        assert_eq!(spec.provider, "foo");
        assert_eq!(spec.temperatures, vec![0.5]);
        assert_eq!(spec.replicas_per_temperature, 2);
    }

    /// PR-D1: a CSV temperature list parses into the typed
    /// `Vec<f32>`. The audit's canonical `[0.0, 0.3, 0.7, 1.0] ×
    /// 4` example yields 4 temperatures + 4 replicas.
    #[test]
    fn parse_temperature_profile_spec_csv() {
        let spec =
            TemperatureProfileSpec::parse("provider=foo;temperatures=0.0,0.3,0.7,1.0;replicas=4")
                .expect("CSV spec must parse");
        assert_eq!(spec.provider, "foo");
        assert_eq!(spec.temperatures, vec![0.0, 0.3, 0.7, 1.0]);
        assert_eq!(spec.replicas_per_temperature, 4);
    }

    /// PR-D1: a spec missing `provider=` fails cleanly with a
    /// message that names the missing key (so an operator
    /// debugging a typo sees exactly what's wrong).
    #[test]
    fn parse_temperature_profile_spec_rejects_missing_provider() {
        let err = TemperatureProfileSpec::parse("temperatures=0.5;replicas=2")
            .expect_err("missing provider must fail");
        assert!(
            err.to_string().contains("missing `provider=<name>`"),
            "error must name the missing key; got {err:?}"
        );
    }

    /// PR-D1: a temperature outside `0.0..=2.0` fails cleanly.
    /// Pin the band here so a future spec change doesn't
    /// accidentally accept a 5.0 by mistake.
    #[test]
    fn parse_temperature_profile_spec_rejects_out_of_range_temp() {
        let err = TemperatureProfileSpec::parse("provider=foo;temperatures=2.5;replicas=1")
            .expect_err("out-of-range temperature must fail");
        assert!(
            err.to_string().contains("out of range 0.0..=2.0"),
            "error must mention the range; got {err:?}"
        );
    }

    /// PR-D1: `replicas=0` fails cleanly. The audit's contract is
    /// `replicas >= 1`; a zero would silently produce an empty
    /// matrix, which is a footgun.
    #[test]
    fn parse_temperature_profile_spec_rejects_zero_replicas() {
        let err = TemperatureProfileSpec::parse("provider=foo;temperatures=0.5;replicas=0")
            .expect_err("replicas=0 must fail");
        assert!(
            err.to_string().contains("replicas must be >= 1"),
            "error must explain the floor; got {err:?}"
        );
    }

    /// PR-D1: `into_matrix_profile` drops the `provider` key (the
    /// matrix stores the profile under the provider's model name
    /// as the map key, not as a field on the profile itself).
    /// Pin the conversion shape so a future field added to
    /// `TemperatureProfile` doesn't silently leak the provider
    /// string into the matrix.
    #[test]
    fn parse_temperature_profile_spec_into_matrix_profile() {
        let spec =
            TemperatureProfileSpec::parse("provider=minimax-m3;temperatures=0.0,0.7;replicas=3")
                .expect("spec must parse");
        let matrix_profile = spec.into_matrix_profile();
        assert_eq!(matrix_profile.temperatures, vec![0.0, 0.7]);
        assert_eq!(matrix_profile.replicas_per_temperature, 3);
    }

    /// Tanda 04e D-1: the legacy `provider=<model>` form parses
    /// with `section = None`. `into_pair` substitutes the supplied
    /// default section so the CLI merge step produces the right
    /// `(section, model)` pair for the matrix.
    #[test]
    fn parse_temperature_profile_spec_legacy_form_section_is_none() {
        let spec = TemperatureProfileSpec::parse("provider=MiniMax-M3;temperatures=0.5;replicas=2")
            .expect("legacy form must parse");
        assert_eq!(spec.provider, "MiniMax-M3");
        assert_eq!(spec.section, None);
        let (section, model) = spec.into_pair("minimax");
        assert_eq!(section, "minimax");
        assert_eq!(model, "MiniMax-M3");
    }

    /// Tanda 04e D-1: the new `provider=<section>:<model>` form
    /// parses with `section = Some("<section>")`. `into_pair`
    /// returns the explicit pair verbatim, regardless of the
    /// default section supplied.
    #[test]
    fn parse_temperature_profile_spec_explicit_section_form() {
        let spec = TemperatureProfileSpec::parse(
            "provider=opencode:mimo-v2.5;temperatures=0.0,0.7;replicas=3",
        )
        .expect("new form must parse");
        assert_eq!(spec.provider, "mimo-v2.5");
        assert_eq!(spec.section.as_deref(), Some("opencode"));
        let (section, model) = spec.into_pair("minimax");
        assert_eq!(section, "opencode");
        assert_eq!(model, "mimo-v2.5");
    }

    /// Tanda 04e D-1: a value containing more than one `:` (e.g.
    /// an IPv6-looking section name) does NOT silently slip through
    /// `parse_provider_model`. The parser falls through to the
    /// legacy form and stores the whole string as the model
    /// name, which surfaces as a downstream error rather than
    /// silently splitting on the wrong colon.
    #[test]
    fn parse_temperature_profile_spec_multi_colon_falls_back_to_legacy_form() {
        // parse_provider_model rejects extra colons, so the parser
        // falls through to the legacy `provider=<value>` form
        // with `section = None` and the whole string as `provider`.
        let spec = TemperatureProfileSpec::parse(
            "provider=section:weird:MiniMax-M3;temperatures=0.5;replicas=1",
        )
        .expect("multi-colon must still parse as legacy form");
        assert_eq!(spec.provider, "section:weird:MiniMax-M3");
        assert_eq!(spec.section, None);
        let (section, model) = spec.into_pair("minimax");
        assert_eq!(section, "minimax");
        assert_eq!(model, "section:weird:MiniMax-M3");
    }

    // ------------------------------------------------------------
    // PR-04b-1 (A-2): unit tests for the banner helpers. The
    // original `discover_banner_suppressed_when_stdout_is_not_a_tty`
    // integration test was useless (it ran `moagan discover --help`
    // which exits via clap parse before any banner code runs; the
    // assertion was vacuously true). These unit tests capture the
    // banner through `Vec<u8>` and pin the gate via `include_str!`
    // so the contract is locked without touching a real stdout.
    // ------------------------------------------------------------

    /// `write_discover_banner` must produce the exact line the
    /// production print statement emits. If the format string
    /// drifts, this test breaks immediately.
    #[test]
    fn write_discover_banner_emits_expected_shape() {
        let mut buf: Vec<u8> = Vec::new();
        write_discover_banner(
            &mut buf,
            "abc123",
            "minimax",
            &std::path::Path::new("/tmp/run").display(),
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "moagan discover abc123 provider=minimax -> /tmp/run\n");
    }

    /// Pin the gate: the banner print is wrapped in
    /// `if std::io::stdout().is_terminal()`. `is_terminal()` depends on
    /// the OS, so the test reads the source: the gate exists once and
    /// precedes the only call of the helper.
    #[test]
    fn discover_banner_is_gated_by_is_terminal() {
        let src = include_str!("discover.rs");
        let gate = "if std::io::stdout().is_terminal()";
        let gate_idx = src.find(gate).expect("the is_terminal gate must exist");
        let helper_call = src
            .find("let _ = write_discover_banner(")
            .expect("the helper must be called");
        assert!(
            gate_idx < helper_call,
            "the gate must precede the helper call"
        );
    }
}
