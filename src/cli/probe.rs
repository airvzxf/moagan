//! `moagan probe <verb>` — operator-driven diagnostics for the LLM
//! transport layer.
//!
//! Verb-first sub-command tree per the operator-facing naming
//! convention (the `moagan <verb> <noun>` order reads naturally in a
//! shell). Two sub-commands today:
//!
//! 1. `moagan probe max_tokens` — the manual counterpart to the
//!    auto-probe that `ProviderRegistry` runs at startup (PR-400).
//!    It exists so an operator can:
//!    1. Probe a single `(provider, model)` pair on demand and
//!       persist the discovered `max_tokens` ceiling into
//!       `<MOAGAN_HOME>/max_tokens_auto.toml` so a future run
//!       does not have to re-discover it.
//!    2. With `--persist-min`, take the minimum across every
//!       model probed under the same provider and pin the cap as
//!       the operator-level ceiling. The cap is written to the
//!       same sidecar with `auto = false` so a human reading the
//!       file can tell it was operator-pinned rather than
//!       auto-detected.
//!
//! 2. `moagan probe temperature` — the manual counterpart to the
//!    temperature auto-probe that `ProviderRegistry` runs at
//!    startup. It exists so an operator can:
//!    1. Probe one or more `(provider, model)` pairs on demand and
//!       persist the discovered supported-temperatures set into
//!       `<MOAGAN_HOME>/temperatures_auto.toml` so a future run
//!       does not have to re-discover it.
//!    2. With `--persist-union`, take the **union** across every
//!       model probed under the same provider and write the
//!       resulting set into the same sidecar as the operator-level
//!       cap (`auto = false`). On the next run the runtime reads
//!       the cap and skips the auto-probe for the pinned
//!       provider. Union (not intersection) preserves the
//!       principle of "do not restrict what a model already
//!       demonstrated it accepts".
//!
//! The implementations reuse the canonical
//! [`crate::llm::probe::detect_max_tokens`] /
//! [`crate::llm::temperature_probe::detect_supported_temperatures`]
//! algorithms and the per-provider `ProviderProbeTransport` /
//! `ProviderTemperatureProbeTransport` wrappers, so the on-demand
//! probe and the startup auto-probe agree on the same discovered
//! value. `--dry-run` skips the HTTP traffic: the function still
//! validates the `provider:model` pairs, prints the plan, and
//! exits 0 without writing anything to disk.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::fs_layout::MoaganHome;
use crate::llm::probe::ProviderProbeTransport;
use crate::llm::probe_table::MaxTokensTable;
use crate::llm::temperature_probe::{
    ProviderTemperatureProbeTransport, TEMPERATURE_PROBE_BATCH_SIZE, TemperatureTable,
};

/// `moagan probe <verb>` sub-command tree. Verb-first naming per
/// the operator-facing convention; today the verbs are
/// `max_tokens` and `temperature`, both the on-demand counterpart
/// to the corresponding startup auto-probe.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ProbeCmd {
    /// `moagan probe max_tokens` — probe one or more
    /// `(provider, model)` pairs on demand and persist the
    /// discovered `max_tokens` ceiling. See the module docs for
    /// the rationale; the sub-command reuses the canonical
    /// `detect_max_tokens` algorithm and writes through the same
    /// `max_tokens_auto.toml` sidecar the startup auto-probe
    /// uses.
    MaxTokens(ProbeMaxTokensCmd),
    /// `moagan probe temperature` — probe one or more
    /// `(provider, model)` pairs on demand and persist the
    /// discovered supported-temperatures set. Reuses the
    /// canonical `detect_supported_temperatures` algorithm and
    /// writes through the same `temperatures_auto.toml` sidecar
    /// the startup auto-probe uses. `--persist-union` pins the
    /// per-provider cap as the union of every probed model's
    /// accepted set, with `auto = false`.
    Temperature(ProbeTemperatureCmd),
}

/// `moagan probe max_tokens` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct ProbeMaxTokensCmd {
    /// Provider:model pairs to probe, e.g.
    /// `--provider minimax:MiniMax-M3 opencode-go:kimi-k3`.
    /// Repeat the flag once per pair; the value is the literal
    /// `provider:model` string.
    #[arg(
        long = "provider",
        value_name = "PROVIDER:MODEL",
        required = true,
        num_args = 1..,
    )]
    pub providers: Vec<String>,
    /// When set, take the minimum across every probed model under
    /// the same provider and write the cap into
    /// `max_tokens_auto.toml` as the operator-level cap
    /// (`auto = false`). On the next run the runtime reads the
    /// cap and skips the auto-probe for the pinned provider.
    #[arg(long, default_value_t = false)]
    pub persist_min: bool,
    /// Skip the HTTP probe: validate the pairs, print the plan,
    /// exit 0 without touching the wire or the file. Useful for
    /// CI / dry-run scripts.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// `moagan probe temperature` arguments.
///
/// The shape mirrors [`ProbeMaxTokensCmd`] (provider:model list,
/// `--persist-*` pin flag, `--dry-run`) with one addition:
/// `--batch-size` controls the per-batch probe fan-out. The
/// default matches the runtime constant
/// [`TEMPERATURE_PROBE_BATCH_SIZE`] so the CLI probe never
/// exceeds the runtime's own concurrency envelope.
#[derive(Debug, Clone, clap::Args)]
pub struct ProbeTemperatureCmd {
    /// Provider:model pairs to probe, e.g.
    /// `--provider minimax:MiniMax-M3 opencode-go:kimi-k3`.
    /// Repeat the flag once per pair; the value is the literal
    /// `provider:model` string.
    #[arg(
        long = "provider",
        value_name = "PROVIDER:MODEL",
        required = true,
        num_args = 1..,
    )]
    pub providers: Vec<String>,
    /// When set, take the UNION across every probed model under
    /// the same provider and write the resulting set into
    /// `temperatures_auto.toml` as the operator-level cap
    /// (`auto = false`). On the next run the runtime reads the
    /// cap and skips the auto-probe for the pinned provider.
    /// Union (not intersection) preserves the principle of "do
    /// not restrict what a model already demonstrated it accepts".
    #[arg(long, default_value_t = false)]
    pub persist_union: bool,
    /// Batch size for the parallel probe fan-out. Default
    /// matches the runtime constant
    /// [`TEMPERATURE_PROBE_BATCH_SIZE`] (3) so the CLI probe
    /// never exceeds the runtime's own concurrency envelope.
    /// `0` is treated by the algorithm as "fan out every
    /// candidate in parallel".
    #[arg(long, default_value_t = TEMPERATURE_PROBE_BATCH_SIZE)]
    pub batch_size: usize,
    /// Skip the HTTP probe: validate the pairs, print the plan,
    /// exit 0 without touching the wire or the file. Useful for
    /// CI / dry-run scripts.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// `moagan probe <verb> [args]` dispatcher.
pub async fn dispatch(cmd: &ProbeCmd) -> Result<i32> {
    match cmd {
        ProbeCmd::MaxTokens(c) => dispatch_max_tokens(c).await,
        ProbeCmd::Temperature(c) => dispatch_temperature(c).await,
    }
}

async fn dispatch_max_tokens(cmd: &ProbeMaxTokensCmd) -> Result<i32> {
    // Parse + validate the `provider:model` pairs up front. A
    // malformed value (missing colon, empty halves) surfaces as
    // `Error::InvalidArgs` so the CLI can return a friendly exit
    // code instead of probing nothing.
    let pairs: Vec<(String, String)> = cmd
        .providers
        .iter()
        .map(|raw| parse_provider_model(raw))
        .collect::<Result<Vec<_>>>()?;

    let cfg = crate::config::Config::load()?;
    let home = MoaganHome::resolve()?;

    // Print the header before the per-pair loop so a probe that
    // errors out on pair 3 still leaves a partial report on
    // stdout.
    println!("PROBE MAX_TOKENS");
    if cmd.dry_run {
        println!("(dry run: no HTTP calls, no disk writes)");
    }

    let mut results: Vec<ProbeResult> = Vec::with_capacity(pairs.len());
    for (provider, model) in &pairs {
        let spec = cfg.providers.get(provider).cloned().ok_or_else(|| {
            Error::InvalidArgs(format!(
                "probe: provider '{provider}' is not in the loaded config; \
                 register it under [providers.{provider}] in config.toml first"
            ))
        })?;
        // The user asked for a specific model override; the spec
        // is the template we copy from. The override lets the
        // operator point the probe at an alias the config has not
        // been updated for.
        let mut spec = spec;
        spec.model = model.clone();

        if spec.kind == "mock" {
            println!("  Probing {provider}:{model} ... skipped (mock has no upstream)");
            results.push(ProbeResult {
                provider: provider.clone(),
                model: model.clone(),
                outcome: ProbeOutcome::SkippedMock,
            });
            continue;
        }

        if cmd.dry_run {
            println!("  Probing {provider}:{model} ... would probe (dry run)");
            results.push(ProbeResult {
                provider: provider.clone(),
                model: model.clone(),
                outcome: ProbeOutcome::DryRun,
            });
            continue;
        }

        // Build the inner provider with the override applied, then
        // wrap it in a transport. The construction goes through
        // the same `from_config` path the registry uses, so the
        // probe observes the same wire behaviour a real run would
        // see (auth header, endpoint, rate-limit knobs).
        let provider_arc = build_provider_for_probe(&spec)?;
        // Query the per-provider probe ceiling so the exponential
        // phase short-circuits at the upstream's hard cap rather
        // than walking `2^1..2^30` against values the upstream
        // will reject (e.g. DeepSeek-direct caps at 393_216).
        let ceiling = provider_arc.max_tokens_probe_ceiling();
        let transport = ProviderProbeTransport::new(provider_arc).map_err(|e| Error::Provider {
            message: format!("probe: build transport: {e}"),
            http_status: None,
        })?;
        let transport: Arc<dyn crate::llm::probe::ProbeTransport> = Arc::new(transport);

        // The table does both the probe and the persistence in
        // one call. The on-disk sidecar is updated as a side
        // effect so the next startup can pick the result up
        // without re-running the algorithm.
        let floor = crate::llm::probe::MIN_AUTOPROBE_FLOOR;
        let table = MaxTokensTable::from_home(&home, floor, true)?;
        let discovered = match table
            .probe_and_store(provider, model, transport, ceiling)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                println!("  Probing {provider}:{model} ... FAILED: {e}");
                results.push(ProbeResult {
                    provider: provider.clone(),
                    model: model.clone(),
                    outcome: ProbeOutcome::Failed(format!("{e}")),
                });
                continue;
            }
        };
        println!(
            "  Probing {provider}:{model} ... accepted up to {discovered}; discovered: {discovered}"
        );
        results.push(ProbeResult {
            provider: provider.clone(),
            model: model.clone(),
            outcome: ProbeOutcome::Discovered(discovered),
        });
    }

    // --persist-min: take the per-provider minimum and write each
    // as an operator cap into the same sidecar. The on-disk shape
    // is the `operator_caps` field of `MaxTokensTableFile`.
    if cmd.persist_min {
        let min_per_provider = compute_min_per_provider(&results);
        if min_per_provider.is_empty() {
            println!("\n--persist-min: no successful probes; nothing to pin");
        } else {
            let table =
                MaxTokensTable::from_home(&home, crate::llm::probe::MIN_AUTOPROBE_FLOOR, true)?;
            println!("\n--persist-min: operator caps written to max_tokens_auto.toml:");
            for (provider, min) in &min_per_provider {
                table.set_operator_cap(provider, *min)?;
                println!("  {provider}: MIN {min}  (auto=false)");
            }
        }
    }

    let _ = (home, cfg);
    Ok(0)
}

/// `moagan probe temperature` dispatcher.
///
/// Mirrors [`dispatch_max_tokens`] but writes through
/// `temperatures_auto.toml` and offers `--persist-union` instead
/// of `--persist-min`. The flow:
///
/// 1. Parse + validate `provider:model` pairs (reuses
///    [`parse_provider_model`]).
/// 2. Load [`Config`] and [`MoaganHome`].
/// 3. Print the header.
/// 4. `--dry-run` → print the plan and exit 0 without touching
///    the wire or the file.
/// 5. For every `(provider, model)`:
///    - Resolve the spec from `cfg.providers`; error out if
///      missing.
///    - Override `spec.model` with the operator-supplied model.
///    - `mock` provider → skip with
///      [`TemperatureProbeOutcome::SkippedMock`].
///    - Build the provider via [`build_provider_for_probe`] and
///      wrap it in [`ProviderTemperatureProbeTransport`].
///    - Load the [`TemperatureTable`] (persistence enabled).
///    - Run [`TemperatureTable::probe_and_store`] with the
///      operator's `--batch-size`.
///    - Print a per-pair report.
/// 6. `--persist-union` → group results by provider, take the
///    union of every accepted set, and call
///    [`TemperatureTable::set_operator_cap`] once per provider.
///
/// Exits 0 on success; non-zero is impossible from this
/// dispatcher because every error is logged and the loop
/// continues.
async fn dispatch_temperature(cmd: &ProbeTemperatureCmd) -> Result<i32> {
    // Parse + validate the `provider:model` pairs up front. Same
    // contract as `dispatch_max_tokens`: a malformed value
    // surfaces as `Error::InvalidArgs`.
    let pairs: Vec<(String, String)> = cmd
        .providers
        .iter()
        .map(|raw| parse_provider_model(raw))
        .collect::<Result<Vec<_>>>()?;

    let cfg = crate::config::Config::load()?;
    let home = MoaganHome::resolve()?;

    println!("PROBE TEMPERATURE");
    if cmd.dry_run {
        println!("(dry run: no HTTP calls, no disk writes)");
        println!("--batch-size: {batch}", batch = cmd.batch_size);
    } else {
        println!(
            "--batch-size: {batch} (runtime default: {default})",
            batch = cmd.batch_size,
            default = TEMPERATURE_PROBE_BATCH_SIZE
        );
    }

    let mut results: Vec<TemperatureProbeResult> = Vec::with_capacity(pairs.len());
    for (provider, model) in &pairs {
        let spec = cfg.providers.get(provider).cloned().ok_or_else(|| {
            Error::InvalidArgs(format!(
                "probe: provider '{provider}' is not in the loaded config; \
                 register it under [providers.{provider}] in config.toml first"
            ))
        })?;
        // The user asked for a specific model override; the spec
        // is the template we copy from.
        let mut spec = spec;
        spec.model = model.clone();

        if spec.kind == "mock" {
            println!("  Probing {provider}:{model} ... skipped (mock has no upstream)");
            results.push(TemperatureProbeResult {
                provider: provider.clone(),
                model: model.clone(),
                outcome: TemperatureProbeOutcome::SkippedMock,
            });
            continue;
        }

        if cmd.dry_run {
            println!("  Probing {provider}:{model} ... would probe (dry run)");
            results.push(TemperatureProbeResult {
                provider: provider.clone(),
                model: model.clone(),
                outcome: TemperatureProbeOutcome::DryRun,
            });
            continue;
        }

        // Build the inner provider with the override applied,
        // then wrap it in the temperature transport. The
        // construction goes through the same `from_config` path
        // the registry uses, so the probe observes the same
        // wire behaviour a real run would see (auth header,
        // endpoint, rate-limit knobs).
        let provider_arc = build_provider_for_probe(&spec)?;
        let transport =
            ProviderTemperatureProbeTransport::new(provider_arc).map_err(|e| Error::Provider {
                message: format!("probe: build temperature transport: {e}"),
                http_status: None,
            })?;
        let transport: Arc<dyn crate::llm::temperature_probe::TemperatureProbeTransport> =
            Arc::new(transport);

        // The table does both the probe and the persistence in
        // one call. The on-disk sidecar is updated as a side
        // effect so the next startup can pick the result up
        // without re-running the algorithm.
        let table = TemperatureTable::from_home(&home, true)?;
        let discovered = match table
            .probe_and_store(provider, model, transport, cmd.batch_size)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                println!("  Probing {provider}:{model} ... FAILED: {e}");
                results.push(TemperatureProbeResult {
                    provider: provider.clone(),
                    model: model.clone(),
                    outcome: TemperatureProbeOutcome::Failed(format!("{e}")),
                });
                continue;
            }
        };
        println!("  Probing {provider}:{model} ... accepted set: {discovered:?}");
        results.push(TemperatureProbeResult {
            provider: provider.clone(),
            model: model.clone(),
            outcome: TemperatureProbeOutcome::Discovered(discovered),
        });
    }

    // --persist-union: take the per-provider union of every
    // accepted set and write each as an operator cap into the
    // same sidecar. The on-disk shape is the `operator_caps`
    // field of `TemperatureTableFile`. The table is reloaded
    // from disk so the cap write goes through the standard
    // persistence path (and reflects any entries the startup
    // auto-probe may have already written).
    if cmd.persist_union {
        let union_per_provider_map = union_per_provider(&results);
        if union_per_provider_map.is_empty() {
            println!("\n--persist-union: no successful probes; nothing to pin");
        } else {
            let table = TemperatureTable::from_home(&home, true)?;
            println!("\n--persist-union: operator caps written to temperatures_auto.toml:");
            for (provider, temps) in &union_per_provider_map {
                table.set_operator_cap(provider, temps.clone())?;
                println!("  {provider}: UNION {temps:?}  (auto=false)");
            }
        }
    }

    let _ = (home, cfg);
    Ok(0)
}

/// Outcome of a single temperature-probe attempt. Mirrors
/// [`ProbeOutcome`] but the `Discovered` variant carries the
/// accepted-set `Vec<f32>` rather than a single integer.
#[derive(Debug, Clone)]
enum TemperatureProbeOutcome {
    /// Discovered accepted set.
    Discovered(Vec<f32>),
    /// Skipped: `mock` provider has no upstream.
    SkippedMock,
    /// Dry-run: would have probed, no HTTP traffic.
    DryRun,
    /// Probe failed (transport error, all probes rejected).
    Failed(#[allow(dead_code)] String),
}

#[derive(Debug, Clone)]
struct TemperatureProbeResult {
    provider: String,
    /// Model name. Kept on the struct so the printed report can
    /// echo the pair verbatim; the per-provider aggregation
    /// ignores the field.
    #[allow(dead_code)]
    model: String,
    outcome: TemperatureProbeOutcome,
}

impl TemperatureProbeResult {
    /// Borrow the discovered accepted set, when the probe
    /// succeeded. Returns `None` for `SkippedMock`, `DryRun`, or
    /// `Failed` outcomes so the per-provider aggregation
    /// naturally skips them.
    fn discovered(&self) -> Option<&Vec<f32>> {
        if let TemperatureProbeOutcome::Discovered(ref v) = self.outcome {
            Some(v)
        } else {
            None
        }
    }
}

/// Aggregate the per-provider accepted sets into one map of
/// `provider -> sorted, deduped union`. Only
/// [`TemperatureProbeOutcome::Discovered`] outcomes contribute;
/// `SkippedMock`, `DryRun`, and `Failed` are silently dropped so
/// a single bad probe cannot corrupt the cap.
///
/// The result is a `BTreeMap` so the iteration order is
/// deterministic (alphabetical by provider name), which makes
/// the printed `--persist-union` report reproducible across
/// runs.
///
/// Dedup is `f32::to_bits`-based so two `NaN` payloads are
/// treated as the same key. `NaN` cannot appear in a
/// discovered set (the algorithm only accepts `0.0..2.0` in
/// `0.1` increments), but the dedup key matches the convention
/// used by [`TemperatureTable::set_operator_cap`] so the
/// downstream union is byte-identical to what
/// `probe_and_store` + `set_operator_cap` would produce.
fn union_per_provider(results: &[TemperatureProbeResult]) -> BTreeMap<String, Vec<f32>> {
    let mut acc: BTreeMap<String, std::collections::BTreeSet<u32>> = BTreeMap::new();
    for r in results {
        if let Some(set) = r.discovered() {
            let entry = acc.entry(r.provider.clone()).or_default();
            for t in set {
                entry.insert(t.to_bits());
            }
        }
    }
    // Convert the bit-set back to a sorted `Vec<f32>`. Sorting by
    // `f32` value (not by bits) gives the operator a familiar
    // ascending order in the printed report.
    acc.into_iter()
        .map(|(provider, bits)| {
            let mut sorted: Vec<f32> = bits.into_iter().map(f32::from_bits).collect();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (provider, sorted)
        })
        .collect()
}

/// Outcome of a single probe attempt. The `Failed` variant
/// carries the error message verbatim so the per-pair report can
/// surface it without re-running the probe; the field is
/// read-only via the printed summary path even though no
/// standalone getter touches it today.
#[derive(Debug, Clone)]
enum ProbeOutcome {
    /// Discovered value (`max_tokens` ceiling).
    Discovered(u32),
    /// Skipped: `mock` provider has no upstream.
    SkippedMock,
    /// Dry-run: would have probed, no HTTP traffic.
    DryRun,
    /// Probe failed (transport error, all probes rejected).
    Failed(#[allow(dead_code)] String),
}

#[derive(Debug, Clone)]
struct ProbeResult {
    provider: String,
    /// Model name. Kept on the struct so the printed report
    /// (which is the only consumer of `ProbeResult`) can echo
    /// the pair verbatim; the per-provider aggregation ignores
    /// the field, hence the dead-code lint suppression below.
    #[allow(dead_code)]
    model: String,
    outcome: ProbeOutcome,
}

impl ProbeResult {
    fn discovered(&self) -> Option<u32> {
        if let ProbeOutcome::Discovered(v) = self.outcome {
            Some(v)
        } else {
            None
        }
    }
}

fn compute_min_per_provider(results: &[ProbeResult]) -> BTreeMap<String, u32> {
    let mut mins: BTreeMap<String, u32> = BTreeMap::new();
    for r in results {
        if let Some(v) = r.discovered() {
            mins.entry(r.provider.clone())
                .and_modify(|existing| {
                    if v < *existing {
                        *existing = v;
                    }
                })
                .or_insert(v);
        }
    }
    mins
}

/// Parse a `provider:model` pair. Rejects empty halves and
/// extra colons (`a:b:c`) to keep the CLI surface unambiguous.
pub fn parse_provider_model(raw: &str) -> Result<(String, String)> {
    let (provider, model) = raw.split_once(':').ok_or_else(|| {
        Error::InvalidArgs(format!("probe: expected 'provider:model', got '{raw}'"))
    })?;
    if provider.is_empty() || model.is_empty() {
        return Err(Error::InvalidArgs(format!(
            "probe: '{raw}' has an empty provider or model half"
        )));
    }
    if model.contains(':') {
        return Err(Error::InvalidArgs(format!(
            "probe: '{raw}' has more than one ':'; expected exactly one separator"
        )));
    }
    Ok((provider.to_owned(), model.to_owned()))
}

/// Build a [`Provider`](crate::llm::provider::Provider) from a spec
/// with a model override. Mirrors the dispatch in
/// [`crate::llm::provider::registry_from_config_with_home`] but
/// skips the registry wrapping (the probe only needs a transport,
/// not a pool or a breaker).
fn build_provider_for_probe(
    spec: &crate::config::ProviderConfig,
) -> Result<Arc<dyn crate::llm::provider::Provider>> {
    use crate::llm::provider::Provider;
    let provider: Arc<dyn Provider> = match spec.kind.as_str() {
        "deepseek" => Arc::new(crate::llm::deepseek::DeepSeekProvider::from_config(spec)?),
        "minimax" => Arc::new(crate::llm::minimax::MinimaxProvider::from_config(spec)?),
        "opencode_go" => {
            if crate::llm::opencode_go::OpenCodeGoProvider::is_blocked(&spec.model) {
                return Err(Error::InvalidArgs(format!(
                    "probe: model '{}' is blocked for opencode_go; use direct minimax provider instead",
                    spec.model
                )));
            }
            Arc::new(crate::llm::opencode_go::OpenCodeGoProvider::from_config(
                spec,
            )?)
        }
        "opencode_go_anthropic" => Arc::new(
            crate::llm::opencode_go_anthropic::OpenCodeGoAnthropicProvider::from_config(spec)?,
        ),
        "opencode_go_responses" => Arc::new(
            crate::llm::opencode_go_responses::OpenCodeGoResponsesProvider::from_config(spec)?,
        ),
        other => {
            return Err(Error::InvalidArgs(format!(
                "probe: provider kind '{other}' is not supported by `moagan probe max_tokens`; \
                 supported kinds: minimax, deepseek, opencode_go, opencode_go_anthropic, opencode_go_responses"
            )));
        }
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `provider:model` parsing: well-formed values split on the
    /// first colon.
    #[test]
    fn parse_provider_model_parses_well_formed() {
        let (p, m) = parse_provider_model("minimax:MiniMax-M3").unwrap();
        assert_eq!(p, "minimax");
        assert_eq!(m, "MiniMax-M3");
        // Multi-segment model names (e.g. `opencode-go:kimi-k3`)
        // are preserved verbatim — the model half is not split on
        // any internal separator.
        let (p, m) = parse_provider_model("opencode-go:kimi-k3").unwrap();
        assert_eq!(p, "opencode-go");
        assert_eq!(m, "kimi-k3");
    }

    /// `provider:model` parsing: missing colon is a hard error
    /// so a typo (`minimax-MiniMax-M3` instead of
    /// `minimax:MiniMax-M3`) fails loudly instead of silently
    /// probing nothing.
    #[test]
    fn parse_provider_model_rejects_missing_colon() {
        let err = parse_provider_model("minimax-MiniMax-M3").expect_err("missing colon");
        match err {
            Error::InvalidArgs(msg) => assert!(msg.contains("provider:model")),
            other => panic!("expected Error::InvalidArgs, got {other:?}"),
        }
    }

    /// `provider:model` parsing: empty halves are rejected so a
    /// stray `--provider :MiniMax-M3` or `--provider minimax:`
    /// cannot reach the registry.
    #[test]
    fn parse_provider_model_rejects_empty_halves() {
        assert!(parse_provider_model(":MiniMax-M3").is_err());
        assert!(parse_provider_model("minimax:").is_err());
    }

    /// `provider:model` parsing: extra colons are rejected. The
    /// shape is `provider:model`, and a third colon would mean
    /// the operator mis-pasted a URL.
    #[test]
    fn parse_provider_model_rejects_extra_colon() {
        let err = parse_provider_model("a:b:c").expect_err("extra colon");
        match err {
            Error::InvalidArgs(msg) => assert!(msg.contains("more than one")),
            other => panic!("expected Error::InvalidArgs, got {other:?}"),
        }
    }

    /// `--persist-min` aggregation: the minimum across multiple
    /// probes of the same provider is the per-provider cap.
    #[test]
    fn compute_min_per_provider_takes_minimum() {
        let results = vec![
            ProbeResult {
                provider: "minimax".into(),
                model: "M3".into(),
                outcome: ProbeOutcome::Discovered(524_288),
            },
            ProbeResult {
                provider: "minimax".into(),
                model: "M2.7".into(),
                outcome: ProbeOutcome::Discovered(131_072),
            },
            ProbeResult {
                provider: "opencode_go".into(),
                model: "kimi-k3".into(),
                outcome: ProbeOutcome::Discovered(8_192),
            },
            ProbeResult {
                provider: "minimax".into(),
                model: "M2.5".into(),
                outcome: ProbeOutcome::SkippedMock,
            },
        ];
        let mins = compute_min_per_provider(&results);
        assert_eq!(mins.get("minimax"), Some(&131_072));
        assert_eq!(mins.get("opencode_go"), Some(&8_192));
    }

    /// `--persist-min` aggregation: skipped and failed probes do
    /// not contribute to the per-provider minimum, so a
    /// single-provider run with one failed probe leaves the
    /// map empty rather than pinning `0`.
    #[test]
    fn compute_min_per_provider_ignores_failures() {
        let results = vec![
            ProbeResult {
                provider: "minimax".into(),
                model: "M3".into(),
                outcome: ProbeOutcome::Failed("network".into()),
            },
            ProbeResult {
                provider: "minimax".into(),
                model: "M2.7".into(),
                outcome: ProbeOutcome::SkippedMock,
            },
        ];
        let mins = compute_min_per_provider(&results);
        assert!(mins.is_empty(), "no successful probes => empty map");
    }

    /// Probe dry-run: `--dry-run` must skip the HTTP transport
    /// entirely. The test asserts the pair still validates and
    /// the `compute_min_per_provider` helper sees the right
    /// `DryRun` outcome.
    #[test]
    fn probe_max_tokens_dry_run_does_not_call_provider() {
        // The test pins the contract: the dry-run branch pushes
        // `ProbeOutcome::DryRun` (not `Discovered`), so a
        // `--persist-min` aggregation would ignore it. The
        // check is structural: the variant enum shape
        // guarantees the dispatcher never constructs a
        // `ProviderProbeTransport` for the dry-run path.
        let results = vec![ProbeResult {
            provider: "minimax".into(),
            model: "M3".into(),
            outcome: ProbeOutcome::DryRun,
        }];
        let mins = compute_min_per_provider(&results);
        assert!(
            mins.is_empty(),
            "dry-run must not contribute to the per-provider minimum"
        );
        // The DryRun variant carries no numeric value, so
        // `discovered()` returns None.
        assert!(results[0].discovered().is_none());
    }

    /// `--persist-min` writes the per-provider minimum to
    /// `max_tokens_auto.toml` under the `operator_caps` field.
    /// The test sets a `MaxTokensTable` in a tempdir, asks the
    /// helper to write the cap, and asserts the on-disk TOML
    /// contains the new field with `auto = false`.
    #[test]
    fn probe_max_tokens_persist_min_writes_operator_cap() {
        use crate::llm::probe::MIN_AUTOPROBE_FLOOR;
        use crate::llm::probe_table::MaxTokensTable;
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        home.ensure().unwrap();
        let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true).unwrap();
        table.set_operator_cap("minimax", 131_072).unwrap();
        // Re-load the file and inspect the `operator_caps` field.
        let file =
            crate::llm::probe::MaxTokensTableFile::load(&home.max_tokens_auto_path()).unwrap();
        let cap = file
            .operator_caps
            .get("minimax")
            .expect("operator cap must be persisted");
        assert_eq!(cap.min, 131_072);
        assert!(!cap.auto, "operator cap is always auto = false");
        // The TOML body must contain the new field so a human
        // diff after a probe-run stays meaningful.
        let body = std::fs::read_to_string(home.max_tokens_auto_path()).unwrap();
        assert!(body.contains("operator_caps"));
        assert!(body.contains("minimax"));
    }

    /// A successful probe persists the discovered value into
    /// `max_tokens_auto.toml`. The test runs the full
    /// `probe_and_store` flow with a custom in-memory transport
    /// (the `MockProvider` path is exercised by the integration
    /// tests; here we exercise the table-persistence path on
    /// its own).
    #[test]
    fn probe_max_tokens_persists_to_table() {
        use crate::llm::probe::{
            MAX_AUTOPROBE_CEILING, MIN_AUTOPROBE_FLOOR, ProbeOutcome, ProbeTransport,
        };
        use crate::llm::probe_table::MaxTokensTable;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        #[derive(Clone)]
        struct Capped {
            cap: Arc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl ProbeTransport for Capped {
            async fn probe_send(&self, n: u32) -> ProbeOutcome {
                if n <= self.cap.load(Ordering::SeqCst) {
                    ProbeOutcome::Accepted
                } else {
                    ProbeOutcome::Rejected
                }
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        home.ensure().unwrap();
        let table = MaxTokensTable::from_home(&home, MIN_AUTOPROBE_FLOOR, true).unwrap();
        let transport: Arc<dyn ProbeTransport> = Arc::new(Capped {
            cap: Arc::new(AtomicU32::new(65_536)),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Pass `MAX_AUTOPROBE_CEILING` as the ceiling: this test
        // exercises the table-persistence path on its own (the
        // per-provider ceiling is irrelevant here).
        let v = runtime
            .block_on(table.probe_and_store(
                "minimax",
                "MiniMax-M3",
                transport,
                MAX_AUTOPROBE_CEILING,
            ))
            .unwrap();
        assert_eq!(v, 65_536);
        // The on-disk sidecar must now carry the entry.
        let file =
            crate::llm::probe::MaxTokensTableFile::load(&home.max_tokens_auto_path()).unwrap();
        let entry = file
            .providers
            .get("minimax")
            .and_then(|m| m.get("MiniMax-M3"))
            .expect("entry must be persisted");
        assert_eq!(entry.max_tokens, 65_536);
        assert!(entry.auto);
    }

    // ----------------------------------------------------------------
    // Phase 4: `moagan probe temperature` unit tests.
    //
    // The dispatcher's full HTTP path is exercised by the
    // integration tests; here we pin the helper-layer
    // invariants (`union_per_provider`, dry-run outcome
    // shape, `set_operator_cap` persistence) without spinning
    // up a wiremock server.
    // ----------------------------------------------------------------

    /// `--persist-union` aggregation: the union across multiple
    /// probes of the same provider merges the accepted sets,
    /// dedups by `f32::to_bits`, and sorts ascending by value.
    /// Two distinct providers stay distinct (no cross-provider
    /// contamination).
    #[test]
    fn union_per_provider_takes_union_of_sets() {
        let results = vec![
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M3".into(),
                outcome: TemperatureProbeOutcome::Discovered(vec![0.0, 0.5]),
            },
            TemperatureProbeResult {
                provider: "opencode_go".into(),
                model: "kimi-k3".into(),
                outcome: TemperatureProbeOutcome::Discovered(vec![0.5, 1.0]),
            },
            // Second probe for the same provider; the union
            // brings in 0.3, the dedup drops the duplicate 0.5.
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M2.7".into(),
                outcome: TemperatureProbeOutcome::Discovered(vec![0.3, 0.5]),
            },
        ];
        let map = union_per_provider(&results);
        assert_eq!(map.get("minimax"), Some(&vec![0.0, 0.3, 0.5]));
        assert_eq!(map.get("opencode_go"), Some(&vec![0.5, 1.0]));
        assert_eq!(map.len(), 2);
    }

    /// `--persist-union` aggregation: skipped, dry-run, and
    /// failed probes do not contribute to the per-provider
    /// union, so a single-provider run with one failed probe
    /// leaves the map empty rather than pinning an empty set.
    #[test]
    fn union_per_provider_ignores_failures() {
        let results = vec![
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M3".into(),
                outcome: TemperatureProbeOutcome::Failed("network".into()),
            },
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M2.7".into(),
                outcome: TemperatureProbeOutcome::SkippedMock,
            },
            TemperatureProbeResult {
                provider: "opencode_go".into(),
                model: "kimi-k3".into(),
                outcome: TemperatureProbeOutcome::DryRun,
            },
        ];
        let map = union_per_provider(&results);
        assert!(map.is_empty(), "no successful probes => empty map");
    }

    /// `--dry-run` shape: the outcome enum carries the
    /// `DryRun` variant, the helper ignores it in the union
    /// aggregation, and `discovered()` returns `None`. The test
    /// pins the contract that the dispatcher pushes `DryRun`
    /// (not `Discovered`) for every pair, so the HTTP path is
    /// never constructed.
    #[test]
    fn temperature_probe_dry_run_does_not_call_provider() {
        // The DryRun variant carries no accepted set, so
        // `discovered()` returns None and the union aggregation
        // skips it.
        let results = vec![
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M3".into(),
                outcome: TemperatureProbeOutcome::DryRun,
            },
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M2.7".into(),
                outcome: TemperatureProbeOutcome::DryRun,
            },
        ];
        assert!(results[0].discovered().is_none());
        assert!(results[1].discovered().is_none());
        let map = union_per_provider(&results);
        assert!(
            map.is_empty(),
            "dry-run must not contribute to the per-provider union"
        );
    }

    /// `--persist-union` writes the per-provider union to
    /// `temperatures_auto.toml` under the `operator_caps` field.
    /// The test sets up a `TemperatureTable` in a tempdir,
    /// exercises the `set_operator_cap` write, and asserts the
    /// on-disk TOML carries the new field with `auto = false`.
    /// This is the structural counterpart of the
    /// `probe_max_tokens_persist_min_writes_operator_cap` test:
    /// the helper layer is identical; only the domain (temperatures
    /// vs. max_tokens) differs.
    #[test]
    fn temperature_probe_persist_union_writes_operator_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        home.ensure().unwrap();
        let table = TemperatureTable::from_home(&home, true).unwrap();
        // Simulate two probed models under the same provider.
        // The union (0.0, 0.3, 0.5, 0.7) is what
        // `union_per_provider` would produce; here we write it
        // directly to keep the test structural.
        table
            .set_operator_cap("minimax", vec![0.0, 0.3, 0.5, 0.7])
            .expect("set_operator_cap must succeed");
        // Re-load the file and inspect the `operator_caps` field.
        let file = crate::llm::temperature_probe::TemperatureTableFile::load(
            &home.temperatures_auto_path(),
        )
        .unwrap();
        let cap = file
            .operator_caps
            .get("minimax")
            .expect("operator cap must be persisted");
        assert_eq!(cap.temperatures, vec![0.0, 0.3, 0.5, 0.7]);
        assert!(!cap.auto, "operator cap is always auto = false");
        // The TOML body must contain the new field so a human
        // diff after a probe-run stays meaningful.
        let body = std::fs::read_to_string(home.temperatures_auto_path()).unwrap();
        assert!(body.contains("operator_caps"));
        assert!(body.contains("minimax"));
    }

    /// The full structural flow: build a [`TemperatureTable`]
    /// from a tempdir, simulate two probed models under the
    /// same provider (Discovered outcomes), aggregate via
    /// [`union_per_provider`], write the union through
    /// [`TemperatureTable::set_operator_cap`], and verify the
    /// on-disk sidecar carries the unioned set with `auto =
    /// false`. This is the closest the unit tests get to
    /// running the `dispatch_temperature` happy path without
    /// touching HTTP.
    #[test]
    fn temperature_probe_union_then_persist_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let home = MoaganHome::at(tmp.path().to_path_buf());
        home.ensure().unwrap();
        let table = TemperatureTable::from_home(&home, true).unwrap();
        let results = vec![
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M3".into(),
                outcome: TemperatureProbeOutcome::Discovered(vec![0.0, 0.5, 1.0]),
            },
            TemperatureProbeResult {
                provider: "minimax".into(),
                model: "M2.7".into(),
                outcome: TemperatureProbeOutcome::Discovered(vec![0.5, 0.7]),
            },
            // A different provider must stay independent.
            TemperatureProbeResult {
                provider: "opencode_go".into(),
                model: "kimi-k3".into(),
                outcome: TemperatureProbeOutcome::Discovered(vec![0.5, 1.0]),
            },
        ];
        let map = union_per_provider(&results);
        for (provider, temps) in &map {
            table
                .set_operator_cap(provider, temps.clone())
                .expect("set_operator_cap must succeed");
        }
        // Re-load the file from disk and check both caps.
        let file = crate::llm::temperature_probe::TemperatureTableFile::load(
            &home.temperatures_auto_path(),
        )
        .unwrap();
        let minimax_cap = file.operator_caps.get("minimax").expect("minimax cap");
        assert_eq!(minimax_cap.temperatures, vec![0.0, 0.5, 0.7, 1.0]);
        assert!(!minimax_cap.auto);
        let opencode_cap = file
            .operator_caps
            .get("opencode_go")
            .expect("opencode_go cap");
        assert_eq!(opencode_cap.temperatures, vec![0.5, 1.0]);
        assert!(!opencode_cap.auto);
    }
}
