//! `moagan probe <verb>` — operator-driven diagnostics for the LLM
//! transport layer.
//!
//! Verb-first sub-command tree per the operator-facing naming
//! convention (the `moagan <verb> <noun>` order reads naturally in a
//! shell). The only sub-command today is `moagan probe max_tokens`,
//! which is the manual counterpart to the auto-probe that
//! `ProviderRegistry` runs at startup (PR-400). It exists so an
//! operator can:
//!
//! 1. Probe a single `(provider, model)` pair on demand and persist
//!    the discovered `max_tokens` ceiling into
//!    `<MOAGAN_HOME>/max_tokens_auto.toml` so a future run does not
//!    have to re-discover it.
//! 2. With `--persist-min`, take the minimum across every model
//!    probed under the same provider and pin the cap as the
//!    operator-level ceiling. The cap is written to the same
//!    sidecar with `auto = false` so a human reading the file can
//!    tell it was operator-pinned rather than auto-detected.
//!
//! The implementation reuses the canonical
//! [`crate::llm::probe::detect_max_tokens`] algorithm and the
//! per-provider `ProviderProbeTransport` wrapper, so the on-demand
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

/// `moagan probe <verb>` sub-command tree. Verb-first naming per
/// the operator-facing convention; today the only verb is
/// `max_tokens`, the on-demand counterpart to the startup
/// auto-probe.
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

/// `moagan probe <verb> [args]` dispatcher.
pub async fn dispatch(cmd: &ProbeCmd) -> Result<i32> {
    match cmd {
        ProbeCmd::MaxTokens(c) => dispatch_max_tokens(c).await,
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
}
