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

use crate::llm::models_dev::ModelsDevCatalog;
use crate::llm::wire::Usage;

/// Per-call USD estimate. Reads the matching
/// `ModelsDevEntry.cost` block from `catalog` (when supplied) and
/// returns the sum of `input + output + cache_read + cache_write`
/// charges at the catalog rates.
///
/// The function deliberately does not validate `provider`/`model`
/// against a fixed enum: the catalog is the source of truth and the
/// caller is expected to pass the canonical strings the provider
/// emits on the wire. A misspelled model id returns `0.0` and the
/// caller can decide whether to log a `tracing::warn!`.
///
/// `Usage.cache_read` covers cached-input reads
/// (`cache_read_input_tokens` from Anthropic-style APIs) and
/// `Usage.cache_creation` covers the corresponding write side.
/// Both are priced against the catalog's `cache_read` /
/// `cache_write` rates — the same convention the upstream uses.
pub fn cost_estimate(
    catalog: Option<&ModelsDevCatalog>,
    provider: &str,
    model: &str,
    usage: &Usage,
) -> f64 {
    let Some(catalog) = catalog else {
        return 0.0;
    };
    let Some(entry) = catalog
        .providers
        .get(provider)
        .and_then(|p| p.models.get(model))
    else {
        return 0.0;
    };

    let cost = &entry.cost;
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
}
