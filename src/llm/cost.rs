//! Per-call USD cost estimation from the models.dev catalog.
//!
//! PR-6: takes the catalog `cost: {input, output, cache_read,
//! cache_write}` block (USD per million tokens) and a
//! [`Usage`] record (token counts) and returns the dollar total
//! for one LLM call. The function is pure so it lives next to the
//! catalog and can be unit-tested without an LLM round-trip.
//!
//! Pricing convention: every `Cost` field is USD per **million**
//! tokens. `cost_per_token(per_million, tokens)` is the divide-by-1e6
//! helper that converts one rate to a per-call bill. Cache reads
//! and writes use the same convention.
//!
//! When no catalog is supplied, or the `(provider, model)` pair is
//! unknown, the function returns `0.0` — NOT an error. The catalog
//! may not yet have the model (a freshly-shipped upstream that the
//! 1h TTL has not refreshed yet), and a failed lookup must never
//! abort a call site that only wants a non-zero estimate for the
//! dashboard. Aggregations filter on `cost_usd > 0` so a zero row
//! is silently treated as "no data, do not assume zero".
//!
//! Operator-side cost overrides (`CostOverrides`):
//!
//! When the upstream `https://models.dev/api.json` does not track a
//! provider (e.g. `minimax` as of v0.18.1), every call to a model
//! under that provider silently bills as `$0.00`. The
//! [`CostOverrides`] struct lets the operator hand-author a pricing
//! table at `<MOAGAN_HOME>/cost_overrides.toml` and have the cost
//! estimator honour it. [`cost_estimate_with_overrides`] consults
//! the override table BEFORE the catalog so an operator can fix a
//! catalog-miss without waiting for upstream — and a stale
//! override wins over a present-but-wrong catalog row, so the
//! operator is always in control. See `cost_overrides.toml` for the
//! schema (closes #965).

use crate::llm::client::Usage;
use crate::llm::models_dev::{Cost, ModelsDevCatalog};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Operator-authored cost overrides, loaded from
/// `<MOAGAN_HOME>/cost_overrides.toml`. Two-level map keyed by
/// `(provider, model)` whose leaves are the same
/// [`Cost`] shape the upstream `models.dev` catalog uses (USD per
/// million tokens for `input` / `output` / `cache_read` /
/// `cache_write`).
///
/// The file is **read-only** at runtime: the cost estimator never
/// rewrites it, and there is no auto-probe path that fills it in.
/// This is intentional — pricing is an operator decision, not
/// something the runtime can infer from a wire response. Empty
/// (no override present) is the safe default and matches the
/// pre-existing behaviour where every catalog miss silently bills
/// as `$0.00`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CostOverrides {
    /// `provider -> { model -> Cost }`.
    #[serde(default)]
    pub providers: BTreeMap<String, BTreeMap<String, Cost>>,
}

/// On-disk schema for `<MOAGAN_HOME>/cost_overrides.toml`. The
/// public TOML form uses the dotted-table syntax
/// `[cost_overrides.<provider>."<model>"]` so operators can hand-
/// author the file with the same `(provider, model)` shape the
/// catalog uses; the loader maps it to
/// [`CostOverrides::providers`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct CostOverridesFile {
    /// `schema_version = 1`. Bumped only on a breaking change to
    /// the TOML shape so a future migration can refuse to load
    /// stale files with a clear error.
    #[serde(default = "default_cost_overrides_schema_version")]
    pub schema_version: u32,
    /// Dotted-table: `[cost_overrides.<provider>."<model>"]`.
    #[serde(default)]
    pub cost_overrides: BTreeMap<String, BTreeMap<String, Cost>>,
}

fn default_cost_overrides_schema_version() -> u32 {
    1
}

/// Current schema version. Loader refuses any file with a
/// different value so a future migration cannot silently corrupt
/// the cost ledger.
pub const COST_OVERRIDES_SCHEMA_VERSION: u32 = 1;

impl CostOverrides {
    /// Build an empty override table. Used by tests and as the
    /// fallback when the on-disk file is missing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load from an explicit TOML path. A missing file is NOT an
    /// error — it returns `Self::empty()` and the caller logs a
    /// single `tracing::info!` so the operator knows no overrides
    /// are in effect. A malformed file IS an error so the operator
    /// gets immediate feedback on a typo.
    pub fn from_path(path: &Path) -> crate::error::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let parsed: CostOverridesFile = toml::from_str(&raw).map_err(|e| {
                    crate::error::Error::SchemaViolation(format!(
                        "cost_overrides.toml parse error at {}: {e}",
                        path.display()
                    ))
                })?;
                if parsed.schema_version != COST_OVERRIDES_SCHEMA_VERSION {
                    return Err(crate::error::Error::SchemaViolation(format!(
                        "cost_overrides.toml at {} has schema_version={}, expected {}; refusing to load a stale file",
                        path.display(),
                        parsed.schema_version,
                        COST_OVERRIDES_SCHEMA_VERSION
                    )));
                }
                tracing::info!(
                    path = %path.display(),
                    providers = parsed.cost_overrides.len(),
                    models = parsed
                        .cost_overrides
                        .values()
                        .map(|m| m.len())
                        .sum::<usize>(),
                    "cost_overrides: loaded"
                );
                Ok(Self {
                    providers: parsed.cost_overrides,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %path.display(),
                    "cost_overrides: file not present; using empty overrides"
                );
                Ok(Self::empty())
            }
            Err(e) => Err(crate::error::Error::Io(crate::error::IoError::Raw(e))),
        }
    }

    /// Look up the override for one `(provider, model)` pair.
    /// Returns `None` if no operator-supplied row exists.
    pub fn get(&self, provider: &str, model: &str) -> Option<&Cost> {
        self.providers
            .get(provider)
            .and_then(|models| models.get(model))
    }

    /// Total number of `(provider, model)` entries across every
    /// provider. Useful for `tracing` summaries and tests.
    pub fn len(&self) -> usize {
        self.providers.values().map(|m| m.len()).sum()
    }

    /// `true` when no overrides are configured.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Per-call USD estimate with operator override support. Reads the
/// matching `ModelsDevEntry.cost` block from `catalog` (when
/// supplied), falling back to the matching row in `overrides` when
/// the catalog has nothing (or nothing yet — the upstream 1h TTL
/// has not refreshed). The override is consulted BEFORE the
/// catalog so an operator who disagrees with the upstream row
/// (catalog drift, a fresh regional rate card, etc.) can always
/// win by re-writing `cost_overrides.toml`.
///
/// Precedence (first match wins):
///   1. `overrides.get(provider, model)` — operator file
///   2. `catalog.providers[provider].models[model].cost` — upstream
///   3. return `0.0`
///
/// `Usage.cache_read` covers cached-input reads
/// (`cache_read_input_tokens` from Anthropic-style APIs) and
/// `Usage.cache_creation` covers the corresponding write side.
/// Both are priced against the matched block's `cache_read` /
/// `cache_write` rates — the same convention the upstream uses.
pub fn cost_estimate_with_overrides(
    catalog: Option<&ModelsDevCatalog>,
    overrides: Option<&CostOverrides>,
    provider: &str,
    model: &str,
    usage: &Usage,
) -> f64 {
    // 1. Operator override — wins over the catalog so the operator
    //    can fix a stale or missing catalog row without waiting
    //    for upstream to ship the change.
    if let Some(ov) = overrides.and_then(|o| o.get(provider, model)) {
        let total = compute(ov, usage);
        tracing::trace!(
            provider,
            model,
            source = "override",
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read = usage.cache_read,
            cache_creation = usage.cache_creation,
            total_usd = total,
            "cost_estimate: computed"
        );
        return total;
    }

    // 2. Catalog lookup — the original path; `None` is the silent
    //    miss that today's `coverage show` consumers learn to
    //    filter on. `Some(None)` (catalog present but row absent)
    //    is the v0.18.1 status quo for `minimax`: keep the
    //    warning so dashboards that sum `cost_usd > 0` still work.
    let Some(catalog) = catalog else {
        tracing::debug!(
            provider,
            model,
            "cost_estimate: no catalog attached; returning $0.00"
        );
        return 0.0;
    };
    let Some(entry) = catalog
        .providers
        .get(provider)
        .and_then(|p| p.models.get(model))
    else {
        tracing::warn!(
            provider,
            model,
            "cost_estimate: catalog miss; returning $0.00 (set [cost_overrides.<provider>.\"<model>\"] in cost_overrides.toml to silence this)"
        );
        return 0.0;
    };

    let total = compute(&entry.cost, usage);
    tracing::trace!(
        provider,
        model,
        source = "catalog",
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        cache_read = usage.cache_read,
        cache_creation = usage.cache_creation,
        total_usd = total,
        "cost_estimate: computed"
    );
    total
}

/// Per-call USD estimate. Reads the matching
/// `ModelsDevEntry.cost` block from `catalog` (when supplied) and
/// returns the sum of `input + output + cache_read + cache_write`
/// charges at the catalog rates.
///
/// Kept as the no-overrides entry point for the
/// `cost_estimate(Some(catalog), p, m, usage)` call sites that
/// pre-date #965. Internally delegates to
/// [`cost_estimate_with_overrides`] with `overrides = None`, so
/// the behaviour is byte-identical to the pre-fix path for
/// callers that never wire an override table.
///
/// The function deliberately does not validate `provider`/`model`
/// against a fixed enum: the catalog is the source of truth and the
/// caller is expected to pass the canonical strings the provider
/// emits on the wire. A misspelled model id returns `0.0` and the
/// caller can decide whether to log a `tracing::warn!`.
pub fn cost_estimate(
    catalog: Option<&ModelsDevCatalog>,
    provider: &str,
    model: &str,
    usage: &Usage,
) -> f64 {
    cost_estimate_with_overrides(catalog, None, provider, model, usage)
}

/// Sum the four legs of a [`Cost`] block at the per-call
/// token count. Pulled out of the lookup helpers so both the
/// override and catalog branches share the exact same arithmetic.
fn compute(cost: &Cost, usage: &Usage) -> f64 {
    cost_per_token(cost.input, usage.input_tokens)
        + cost_per_token(cost.output, usage.output_tokens)
        + cost_per_token(cost.cache_read, usage.cache_read)
        + cost_per_token(cost.cache_write, usage.cache_creation)
}

/// Convert a per-million-token USD rate and a token count into a
/// per-call dollar amount. Kept as a `fn` (not a `const fn`) so the
/// `f64` arithmetic follows IEEE 754 and the test can pin the exact
/// answer.
fn cost_per_token(per_million: f64, tokens: u64) -> f64 {
    (per_million / 1_000_000.0) * (tokens as f64)
}

/// Convenience: build a `Cost` from `(input, output, cache_read,
/// cache_write)` USD-per-million rates. Useful for unit tests that
/// want to avoid a full catalog fixture.
#[cfg(test)]
fn rates(
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
) -> crate::llm::models_dev::Cost {
    crate::llm::models_dev::Cost {
        input,
        output,
        cache_read,
        cache_write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::models_dev::{
        CATALOG_SCHEMA_VERSION, Cost, Limits, ModelsDevEntry, ModelsDevProvider,
    };
    use std::collections::BTreeMap;

    fn usage(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read,
            cache_creation,
        }
    }

    fn catalog_with(cost: Cost) -> ModelsDevCatalog {
        let mut models = BTreeMap::new();
        models.insert(
            "m".to_string(),
            ModelsDevEntry {
                id: "m".to_string(),
                name: "m".to_string(),
                family: None,
                attachment: false,
                reasoning: false,
                reasoning_options: Vec::new(),
                tool_call: false,
                temperature: false,
                interleaved: None,
                modalities: crate::llm::models_dev::Modalities::default(),
                limit: Limits::default(),
                cost,
                open_weights: false,
                release_date: None,
                last_updated: None,
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            "p".to_string(),
            ModelsDevProvider {
                id: "p".to_string(),
                name: "p".to_string(),
                api: None,
                doc: None,
                models,
            },
        );
        ModelsDevCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            fetched_at_unix: 0,
            providers,
        }
    }

    #[test]
    fn cost_estimate_zero_tokens_is_zero() {
        let catalog = catalog_with(rates(1.0, 2.0, 0.1, 0.2));
        let u = usage(0, 0, 0, 0);
        assert_eq!(cost_estimate(Some(&catalog), "p", "m", &u), 0.0);
    }

    #[test]
    fn cost_estimate_input_only() {
        // $1.00/M input * 1_000_000 tokens = $1.00.
        let catalog = catalog_with(rates(1.0, 0.0, 0.0, 0.0));
        let u = usage(1_000_000, 0, 0, 0);
        assert!((cost_estimate(Some(&catalog), "p", "m", &u) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cost_estimate_output_only() {
        // $2.50/M output * 200_000 tokens = $0.50.
        let catalog = catalog_with(rates(0.0, 2.5, 0.0, 0.0));
        let u = usage(0, 200_000, 0, 0);
        assert!((cost_estimate(Some(&catalog), "p", "m", &u) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cost_estimate_input_and_output() {
        // ($0.30/M * 1_000_000) + ($1.20/M * 500_000) = $0.30 + $0.60 = $0.90.
        let catalog = catalog_with(rates(0.30, 1.20, 0.0, 0.0));
        let u = usage(1_000_000, 500_000, 0, 0);
        assert!((cost_estimate(Some(&catalog), "p", "m", &u) - 0.90).abs() < 1e-9);
    }

    #[test]
    fn cost_estimate_with_cache_read() {
        // ($0.30/M * 1_000_000) + ($0.03/M * 500_000) = $0.30 + $0.015 = $0.315.
        let catalog = catalog_with(rates(0.30, 0.0, 0.03, 0.0));
        let u = usage(1_000_000, 0, 500_000, 0);
        assert!((cost_estimate(Some(&catalog), "p", "m", &u) - 0.315).abs() < 1e-9);
    }

    #[test]
    fn cost_estimate_with_cache_write() {
        // ($0.30/M * 1_000_000) + ($0.375/M * 400_000) = $0.30 + $0.15 = $0.45.
        let catalog = catalog_with(rates(0.30, 0.0, 0.0, 0.375));
        let u = usage(1_000_000, 0, 0, 400_000);
        assert!((cost_estimate(Some(&catalog), "p", "m", &u) - 0.45).abs() < 1e-9);
    }

    #[test]
    fn cost_estimate_unknown_model_returns_zero() {
        let catalog = catalog_with(rates(1.0, 1.0, 1.0, 1.0));
        let u = usage(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert_eq!(cost_estimate(Some(&catalog), "p", "missing", &u), 0.0);
        assert_eq!(cost_estimate(Some(&catalog), "missing", "m", &u), 0.0);
    }

    #[test]
    fn cost_estimate_no_catalog_returns_zero() {
        let u = usage(1_000_000, 1_000_000, 0, 0);
        assert_eq!(cost_estimate(None, "p", "m", &u), 0.0);
    }

    #[test]
    fn cost_per_token_math_correctness() {
        // 1M tokens at $1/M = $1 exactly (the rate-to-tokens
        // identity is the load-bearing math for every other test).
        assert!((cost_per_token(1.0, 1_000_000) - 1.0).abs() < 1e-9);
        // 500k at $2/M = $1.
        assert!((cost_per_token(2.0, 500_000) - 1.0).abs() < 1e-9);
        // 250k at $0/M = $0.
        assert_eq!(cost_per_token(0.0, 250_000), 0.0);
        // 0 tokens at any rate = $0.
        assert_eq!(cost_per_token(1.0, 0), 0.0);
    }

    // -----------------------------------------------------------------
    // Issue #965 — CostOverrides + cost_estimate_with_overrides tests.
    // -----------------------------------------------------------------

    /// Build a `CostOverrides` from a single `(provider, model) ->
    /// rates` tuple. Keeps the per-test noise low.
    fn overrides_with(provider: &str, model: &str, cost: Cost) -> CostOverrides {
        let mut models = BTreeMap::new();
        models.insert(model.to_string(), cost);
        let mut providers = BTreeMap::new();
        providers.insert(provider.to_string(), models);
        CostOverrides { providers }
    }

    #[test]
    fn cost_overrides_empty_is_empty() {
        let ov = CostOverrides::empty();
        assert!(ov.is_empty());
        assert_eq!(ov.len(), 0);
        assert!(ov.get("any", "model").is_none());
    }

    #[test]
    fn cost_overrides_get_returns_inserted_row() {
        let ov = overrides_with("minimax", "MiniMax-M3", rates(0.30, 1.20, 0.06, 0.375));
        let got = ov.get("minimax", "MiniMax-M3").expect("row must exist");
        assert_eq!(got.input, 0.30);
        assert_eq!(got.output, 1.20);
        assert_eq!(got.cache_read, 0.06);
        assert_eq!(got.cache_write, 0.375);
        assert_eq!(ov.len(), 1);
        assert!(!ov.is_empty());
    }

    #[test]
    fn cost_overrides_get_misses_on_unknown_pair() {
        let ov = overrides_with("minimax", "MiniMax-M3", rates(0.30, 1.20, 0.0, 0.0));
        // Right provider, wrong model.
        assert!(ov.get("minimax", "MiniMax-M2.7").is_none());
        // Wrong provider, right model.
        assert!(ov.get("deepseek", "MiniMax-M3").is_none());
    }

    #[test]
    fn cost_estimate_with_overrides_uses_override_when_present() {
        // Catalog has rates that would otherwise compute to
        // $0.90, but the override sits at $1.50 — the override
        // must win (closes #965 precedence rule).
        let catalog = catalog_with(rates(0.30, 1.20, 0.0, 0.0));
        let ov = overrides_with("p", "m", rates(1.50, 0.0, 0.0, 0.0));
        let u = usage(1_000_000, 0, 0, 0);
        assert!(
            (cost_estimate_with_overrides(Some(&catalog), Some(&ov), "p", "m", &u) - 1.50).abs()
                < 1e-9,
            "override must win over catalog"
        );
    }

    #[test]
    fn cost_estimate_with_overrides_falls_back_to_catalog_when_no_override() {
        // The override table is present but empty for `(p, m)` —
        // the catalog row must still be consulted.
        let catalog = catalog_with(rates(0.30, 1.20, 0.0, 0.0));
        let ov = overrides_with("other-provider", "other-model", rates(99.0, 99.0, 0.0, 0.0));
        let u = usage(1_000_000, 0, 0, 0);
        assert!(
            (cost_estimate_with_overrides(Some(&catalog), Some(&ov), "p", "m", &u) - 0.30).abs()
                < 1e-9,
            "catalog row must be consulted when no override matches"
        );
    }

    #[test]
    fn cost_estimate_with_overrides_falls_back_to_zero_when_neither_has_the_pair() {
        // Mimic the v0.18.1 status quo for `minimax`: catalog
        // exists (Anthropic rows in there) but has no `minimax`
        // provider; no override either. The estimator must
        // return $0.00, NOT crash, NOT borrow another provider's
        // rates by mistake.
        let catalog = catalog_with(rates(1.0, 1.0, 1.0, 1.0));
        let ov = CostOverrides::empty();
        let u = usage(1_000_000, 1_000_000, 0, 0);
        assert_eq!(
            cost_estimate_with_overrides(Some(&catalog), Some(&ov), "minimax", "MiniMax-M3", &u),
            0.0
        );
    }

    #[test]
    fn cost_estimate_with_overrides_none_overrides_is_byte_identical_to_cost_estimate() {
        // The pre-fix call site
        //     cost_estimate(Some(catalog), provider, model, usage)
        // and the post-fix call site
        //     cost_estimate_with_overrides(Some(catalog), None, provider, model, usage)
        // MUST compute the same dollar amount — otherwise the
        // refactor introduced a regression for every existing
        // consumer that never wired an override table.
        let catalog = catalog_with(rates(0.30, 1.20, 0.03, 0.375));
        let u = usage(1_000_000, 500_000, 250_000, 100_000);
        let a = cost_estimate(Some(&catalog), "p", "m", &u);
        let b = cost_estimate_with_overrides(Some(&catalog), None, "p", "m", &u);
        assert!(
            (a - b).abs() < 1e-9,
            "passing None for overrides must reproduce cost_estimate byte-for-byte (got {a} vs {b})"
        );
    }

    #[test]
    fn cost_overrides_from_path_missing_file_returns_empty() {
        // The fixture path is under /tmp; the test will fail on
        // any box where this file already exists, so we pick a
        // unique name and clean it up if present.
        let path = std::env::temp_dir().join(format!(
            "moagan-cost-overrides-missing-{}-{}.toml",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let ov = CostOverrides::from_path(&path).expect("missing file is not an error");
        assert!(ov.is_empty());
    }

    #[test]
    fn cost_overrides_from_path_parses_round_trip() {
        // Authoring the file by hand and reading it back must
        // reproduce the original struct byte-for-byte. Catches
        // a typo in the TOML schema (e.g. forgetting
        // `#[serde(default)]` on `cost_overrides`).
        let path = std::env::temp_dir().join(format!(
            "moagan-cost-overrides-roundtrip-{}-{}.toml",
            std::process::id(),
            line!()
        ));
        let raw = r#"
schema_version = 1

[cost_overrides.minimax."MiniMax-M3"]
input = 0.30
output = 1.20
cache_read = 0.06
cache_write = 0.375

[cost_overrides.minimax."MiniMax-M2.7-highspeed"]
input = 0.60
output = 2.40
cache_read = 0.03
"#;
        std::fs::write(&path, raw).expect("write fixture");
        let ov = CostOverrides::from_path(&path).expect("parse");
        assert_eq!(ov.len(), 2);
        let m3 = ov.get("minimax", "MiniMax-M3").expect("MiniMax-M3 row");
        assert_eq!(m3.input, 0.30);
        assert_eq!(m3.output, 1.20);
        assert_eq!(m3.cache_read, 0.06);
        assert_eq!(m3.cache_write, 0.375);
        let m27 = ov
            .get("minimax", "MiniMax-M2.7-highspeed")
            .expect("MiniMax-M2.7-highspeed row");
        assert_eq!(m27.input, 0.60);
        assert_eq!(m27.output, 2.40);
        assert_eq!(m27.cache_read, 0.03);
        // No cache_write → defaults to 0.0 (serde default).
        assert_eq!(m27.cache_write, 0.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cost_overrides_from_path_rejects_unknown_schema_version() {
        // A future migration bumps `schema_version`; the loader
        // must refuse a stale file rather than silently corrupt
        // the cost ledger with a wrong shape.
        let path = std::env::temp_dir().join(format!(
            "moagan-cost-overrides-schema-{}-{}.toml",
            std::process::id(),
            line!()
        ));
        std::fs::write(
            &path,
            r#"
schema_version = 999

[cost_overrides.p.m]
input = 0.30
output = 1.20
"#,
        )
        .expect("write fixture");
        let err = CostOverrides::from_path(&path).expect_err("stale schema must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("schema_version=999"),
            "error must call out the bad schema_version, got: {msg}"
        );
        assert!(
            msg.contains("refusing to load a stale file"),
            "error must explain the refusal, got: {msg}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cost_overrides_from_path_rejects_malformed_toml() {
        let path = std::env::temp_dir().join(format!(
            "moagan-cost-overrides-malformed-{}-{}.toml",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, "this is not = = valid toml at all = =").expect("write fixture");
        let err = CostOverrides::from_path(&path).expect_err("malformed TOML must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("cost_overrides.toml parse error"),
            "error must mention the file + parse stage, got: {msg}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cost_estimate_with_overrides_silences_catalog_miss_warning_for_overridden_pair() {
        // The override row means the operator has already
        // declared their intent; the noisy
        // `catalog miss; returning $0.00` warning must NOT fire
        // for that pair — only for the genuine miss path.
        //
        // We can't directly assert `tracing` events from a unit
        // test (the subscriber is global), but we CAN exercise
        // the branch and assert the math is non-zero so the
        // override path was actually taken.
        let catalog = catalog_with(rates(1.0, 1.0, 1.0, 1.0));
        let ov = overrides_with("minimax", "MiniMax-M3", rates(0.30, 1.20, 0.06, 0.375));
        let u = usage(1_000_000, 500_000, 0, 0);
        let total =
            cost_estimate_with_overrides(Some(&catalog), Some(&ov), "minimax", "MiniMax-M3", &u);
        // ($0.30/M * 1M) + ($1.20/M * 500k) = $0.30 + $0.60 = $0.90
        assert!(
            (total - 0.90).abs() < 1e-9,
            "override branch must compute via override rates, not catalog"
        );
    }
}
