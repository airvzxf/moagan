//! `moagan discover --explain` — print the cardinality calculation
//! without starting the pipeline.
//!
//! F3 (Track G.2): the `--explain` flag is a no-op fast-path that
//! resolves the operator's knobs the same way the real run would
//! (`CLI > env > TOML > default`), prints a table + formula, and
//! exits. The `Role::DimensionDeriver` is **never** invoked on this
//! path — the cells count for the LLM-derive case is reported as a
//! placeholder (4 dims × 2 facets = 8 cells) so the operator sees
//! the worst-case budget up-front.
//!
//! The output is intentionally minimal:
//!
//! 1. A "Values" table showing each knob (cells, sketches_per_cell,
//!    temperatures, replicas), its size, and the source that
//!    resolved it (`Default`, `Flag`, `Env`, `Toml`, `Spec`, `Llm`).
//! 2. A "Calculation" block showing the formula and the resolved
//!    product.
//! 3. A "Results" block with `Requests LLM = N` and `Surviving
//!    sketches = ≤ N` so the operator can sanity-check the budget.
//!
//! All three sections are pure functions of [`ExplainInput`] so a
//! snapshot test pins the wire format.

use std::fmt;

use tracing::{debug, trace};

use crate::cli::discover::{DEFAULT_SKETCHES_PER_CELL, DiscoverOptions, MIN_SKETCHES_PER_CELL};
use crate::config::Config;
use crate::discovery::matrix_spec::MatrixSpec;
use crate::error::{Error, Result};

/// Where a knob's resolved value came from. The variants match the
/// precedence chain the rest of the codebase documents
/// (`CLI > env > TOML > default`); the explain table uses them to
/// render the "Type" column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    /// Built-in default (no flag, no env, no TOML).
    Default,
    /// CLI flag.
    Flag,
    /// Environment variable.
    Env,
    /// `~/.config/moagan/config.toml`.
    Toml,
    /// `--matrix-spec` derivation (cells = sum(spec facets)).
    Spec,
    /// LLM-derive placeholder (the dimension-deriver runs at
    /// runtime; we don't have a count yet).
    Llm,
}

impl fmt::Display for ValueSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("Default"),
            Self::Flag => f.write_str("Flag"),
            Self::Env => f.write_str("Env"),
            Self::Toml => f.write_str("Toml"),
            Self::Spec => f.write_str("Spec"),
            Self::Llm => f.write_str("Llm"),
        }
    }
}

/// Resolved knobs for the explain table. `cells` is the sum of
/// facet counts across all dimensions (NOT a Cartesian product of
/// `dims × facets`) — see F1's `ExplorationMatrix::cells()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainInput {
    /// Sum of facet counts across dimensions. `0` means
    /// "matrix has no cells" and the explain output omits the
    /// "Results" block.
    pub cells: usize,
    /// Per-cell fan-out (sketches generated for each cell).
    pub sketches_per_cell: usize,
    /// Number of sampling temperatures in the resolved profile.
    pub temperatures: usize,
    /// Replicas per `(cell, temperature)` pair in the resolved
    /// profile.
    pub replicas: usize,
    /// Source for `cells`.
    pub source_cells: ValueSource,
    /// Source for `sketches_per_cell`.
    pub source_sketches_per_cell: ValueSource,
    /// Source for `temperatures`.
    pub source_temperatures: ValueSource,
    /// Source for `replicas`.
    pub source_replicas: ValueSource,
}

impl ExplainInput {
    /// Total LLM requests the matrix will fire. `cells *
    /// sketches_per_cell * temperatures * replicas`. Always
    /// `>= 0` (overflow is impossible for realistic inputs).
    pub fn requests_llm(&self) -> usize {
        let n = self.cells * self.sketches_per_cell * self.temperatures * self.replicas;
        trace!(
            cells = self.cells,
            sketches_per_cell = self.sketches_per_cell,
            temperatures = self.temperatures,
            replicas = self.replicas,
            requests = n,
            "ExplainInput::requests_llm"
        );
        n
    }
}

/// Default cells when nothing resolves — 4 dimensions × 2 facets =
/// 8 cells, the historical "honest fallback" the plan documents.
/// Mirrors the v0.5 default so the operator sees the same budget
/// they would have seen before F1 made facet counts asymmetric.
const DEFAULT_FALLBACK_CELLS: usize = 8;

/// Default facets per dimension used to derive `cells` from
/// `--dimensions N` alone (no `--facets-per-dimension`). The F3
/// resolution says: when only `--dimensions N` is set, we advertise
/// `N * DEFAULT_FACETS_PER_DIM_HINT` cells as a placeholder, with
/// `Source::Default` — the LLM is free to redistribute at runtime.
const DEFAULT_FACETS_PER_DIM_HINT: usize = 2;

/// Resolve the explain input from the operator's CLI options +
/// loaded config. The precedence is `CLI > env > TOML > default`,
/// matching the rest of the discovery code path. The matrix-spec
/// parser is reused (F1's `MatrixSpec::parse_all`) so the cells
/// computation matches the real run verbatim.
pub fn build_explain_input(opts: &DiscoverOptions, cfg: &Config) -> Result<ExplainInput> {
    debug!("discover_explain::build_explain_input: enter");
    let (cells, source_cells) = resolve_cells(opts)?;
    let (sketches_per_cell, source_sketches_per_cell) = resolve_sketches_per_cell(opts, cfg);
    let (temperatures, replicas, source_profile) = resolve_temperature_profile(opts, cfg);
    trace!(
        cells,
        sketches_per_cell, temperatures, replicas, "discover_explain: built input"
    );
    Ok(ExplainInput {
        cells,
        sketches_per_cell,
        temperatures,
        replicas,
        source_cells,
        source_sketches_per_cell,
        source_temperatures: source_profile,
        source_replicas: source_profile,
    })
}

/// Resolve the `cells` (sum of facet counts) plus its source.
///
/// Precedence:
/// 1. `--matrix-spec` (re-uses the F1 parser) → `Source::Spec`.
/// 2. `--dimensions N` + `--facets-per-dimension M` →
///    `cells = N * M`, `Source::Flag`.
/// 3. `--dimensions N` alone → `cells = N *
///    DEFAULT_FACETS_PER_DIM_HINT`, `Source::Default` (the LLM
///    picks asymmetric counts at runtime; this is a worst-case
///    placeholder so the operator sees a budget).
/// 4. `--llm-derive` alone → `Source::Llm` (the LLM picks
///    everything; cells reported as 0 so the "Results" block is
///    omitted — the operator should not see a made-up number).
/// 5. Nothing → `cells = DEFAULT_FALLBACK_CELLS` (= 8), `Source::Default`.
fn resolve_cells(opts: &DiscoverOptions) -> Result<(usize, ValueSource)> {
    let non_empty_specs: Vec<&String> = opts
        .matrix_spec
        .iter()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !non_empty_specs.is_empty() {
        let parsed = MatrixSpec::parse_all(non_empty_specs.into_iter().cloned())?;
        return Ok((parsed.cells(), ValueSource::Spec));
    }
    if let (Some(n), Some(m)) = (opts.dimensions, opts.facets_per_dimension) {
        return Ok((n * m, ValueSource::Flag));
    }
    if let Some(n) = opts.dimensions {
        // `--dimensions N` alone: the LLM picks facets
        // asymmetrically per dimension, so we cannot predict the
        // exact cells count. Surface a worst-case placeholder
        // (`N * 2`) so the operator sees an upper bound, with
        // `Source::Default` so they know it's not a real number.
        return Ok((n * DEFAULT_FACETS_PER_DIM_HINT, ValueSource::Default));
    }
    if opts.llm_derive {
        return Ok((0, ValueSource::Llm));
    }
    Ok((DEFAULT_FALLBACK_CELLS, ValueSource::Default))
}

/// Resolve `sketches_per_cell` + its source.
///
/// Precedence (CLI wins on conflict):
/// 1. CLI flag (`opts.sketches_per_cell`, default 10) → `Flag`.
/// 2. TOML `[discovery_matrix].sketches_per_cell` → `Toml`.
/// 3. Built-in default 10 → `Default`.
///
/// The operator-facing floor is 1 (lowered from 10 in v0.13.2);
/// only the default remains 10. The floor is enforced at the
/// CLI layer and re-checked below in `build_and_format`.
///
/// The `MOAGAN_DISCOVERY_SKETCHES_PER_CELL` env var is applied
/// inside `Config::apply_env_overrides` (before this helper sees
/// the config) so a `Toml` source here actually means "the env
/// var was unset and TOML has the value"; we deliberately do NOT
/// distinguish Env from Toml in this helper — both fall under
/// `Toml` per the plan's "Default / Flag / Env / Toml" taxonomy
/// (the operator's TOML block is what they edited, the env var
/// is a transient override).
fn resolve_sketches_per_cell(opts: &DiscoverOptions, cfg: &Config) -> (usize, ValueSource) {
    // The CLI flag is the highest-precedence source. We detect
    // "user passed --sketches-per-cell" by comparing the parsed
    // value against the default; clap's `default_value_t = 10`
    // means we cannot distinguish "user typed 10" from "user
    // typed nothing". The plan's `Source::Flag` covers both cases
    // when the CLI was used, so we treat the parsed value as the
    // CLI source. If a future subagent changes the default,
    // `resolve_sketches_per_cell` will need a `matches!` against
    // the clap default to disambiguate.
    if opts.sketches_per_cell != DEFAULT_SKETCHES_PER_CELL {
        return (opts.sketches_per_cell, ValueSource::Flag);
    }
    let toml_value = cfg.discovery_matrix.sketches_per_cell;
    if toml_value != DEFAULT_SKETCHES_PER_CELL {
        return (toml_value, ValueSource::Toml);
    }
    (DEFAULT_SKETCHES_PER_CELL, ValueSource::Default)
}

/// Resolve the temperature profile's `(temperatures, replicas)` +
/// source.
///
/// Resolution:
/// 1. CLI `--temperature-profile` (last-wins per provider, but
///    for the explain we just take the last one) → `Flag`.
/// 2. TOML `[discovery_matrix].default_profile` →
///    `(profile.temperatures.len(), profile.replicas_per_temperature)`,
///    `Toml`.
/// 3. TOML `[discovery_matrix].temperature_profiles.<model>`
///    (first entry wins for the explain) → `Toml`.
/// 4. Built-in default `[1.0] × 1` → `Default`.
///
/// The `replicas_per_temperature` is reported alongside the
/// temperatures count; both share the same source so the
/// operator sees a single "Type" annotation.
fn resolve_temperature_profile(
    opts: &DiscoverOptions,
    cfg: &Config,
) -> (usize, usize, ValueSource) {
    if let Some(spec) = opts.temperature_profiles.last() {
        return (
            spec.temperatures.len(),
            spec.replicas_per_temperature,
            ValueSource::Flag,
        );
    }
    if let Some(p) = cfg.discovery_matrix.default_profile.as_ref() {
        return (
            p.temperatures.len(),
            p.replicas_per_temperature,
            ValueSource::Toml,
        );
    }
    if let Some((_, p)) = cfg.discovery_matrix.temperature_profiles.iter().next() {
        return (
            p.temperatures.len(),
            p.replicas_per_temperature,
            ValueSource::Toml,
        );
    }
    // Default profile = `[1.0] × 1` → 1 temperature, 1 replica.
    (1, 1, ValueSource::Default)
}

/// Format the explain output as a single string. Pure function of
/// the [`ExplainInput`] so snapshot tests can pin the wire format
/// without spinning up a config / disk.
///
/// Format (the plan mandates this verbatim — the column widths,
/// underlines, blank lines, and ordering are part of the contract):
///
/// ```text
/// Values
/// ------
///
/// | Value             | Size | Type    |
/// | ----------------- | ---- | ------- |
/// | facets            |   24 | Default |
/// | sketches_per_cell |   10 | Flag    |
/// | temperatures      |    4 | Env     |
/// | replicas          |    2 | Env     |
///
/// Calculation
/// -----------
///
/// facets × sketches_per_cell x temperatures × replicas
/// 24 × 10 × 4 × 2 = 1920
///
/// Results
/// -------
///
/// Requests LLM = 1920
/// Surviving sketches = ≤ 1920
/// ```
///
/// When `cells == 0` (LLM-derive without dimension hints) the
/// "Results" block is omitted so the operator does not see a
/// fabricated `0 × 10 × 1 × 1 = 0` line.
pub fn format_explain(input: &ExplainInput) -> String {
    let mut out = String::new();
    out.push_str("Values\n------\n\n");
    out.push_str(&format_values_table(input));
    out.push_str("\n\n");
    out.push_str("Calculation\n-----------\n\n");
    out.push_str(&format_calculation(input));
    if input.cells > 0 {
        out.push('\n');
        out.push_str("Results\n-------\n\n");
        out.push_str(&format_results(input));
    }
    out
}

/// Width of the `Value` column. Picked to fit `sketches_per_cell`
/// (17 chars) without truncation.
const VALUE_COL_WIDTH: usize = 17;
/// Width of the `Size` column. 4 chars matches "10000" plus a
/// leading space; right-aligned numeric formatting goes inside
/// `| {n:>4} |`.
const SIZE_COL_WIDTH: usize = 4;
/// Width of the `Type` column. 7 chars fits `Default` plus a
/// trailing space.
const TYPE_COL_WIDTH: usize = 7;

/// Render the "Values" table. The `Size` column is right-aligned so
/// `24`, `10`, `4`, `2` line up; the `Type` column is left-aligned.
fn format_values_table(input: &ExplainInput) -> String {
    let rows: [(&str, usize, ValueSource); 4] = [
        // `facets` is documented as "suma de facets por dimensión
        // (= cells)". The numeric value is `cells` so a 4-dim ×
        // 2-facet matrix reports `8`, not `4×2=8` — the formula
        // already collapses the sum.
        ("facets", input.cells, input.source_cells),
        (
            "sketches_per_cell",
            input.sketches_per_cell,
            input.source_sketches_per_cell,
        ),
        (
            "temperatures",
            input.temperatures,
            input.source_temperatures,
        ),
        ("replicas", input.replicas, input.source_replicas),
    ];
    let mut buf = String::new();
    buf.push_str(&format!(
        "| {:<VW$} | {:>SW$} | {:<TW$} |\n",
        "Value",
        "Size",
        "Type",
        VW = VALUE_COL_WIDTH,
        SW = SIZE_COL_WIDTH,
        TW = TYPE_COL_WIDTH,
    ));
    buf.push_str(&format!(
        "| {:-<VW$} | {:-<SW$} | {:-<TW$} |\n",
        "",
        "",
        "",
        VW = VALUE_COL_WIDTH,
        SW = SIZE_COL_WIDTH,
        TW = TYPE_COL_WIDTH,
    ));
    for (name, size, source) in rows {
        buf.push_str(&format!(
            "| {:<VW$} | {:>SW$} | {:<TW$} |\n",
            name,
            size,
            source.to_string(),
            VW = VALUE_COL_WIDTH,
            SW = SIZE_COL_WIDTH,
            TW = TYPE_COL_WIDTH,
        ));
    }
    // Strip the trailing newline so callers can compose the
    // output cleanly with surrounding `\n` separators.
    if buf.ends_with('\n') {
        buf.pop();
    }
    buf
}

/// Render the "Calculation" block. The formula header uses a
/// lowercase `x` between `sketches_per_cell` and `temperatures`
/// (per the plan's exact format) — every other separator is the
/// multiplication sign `×`. The returned string ends with `\n` so
/// the caller can prepend a blank-line separator (`\n`) without
/// manual newline bookkeeping.
fn format_calculation(input: &ExplainInput) -> String {
    let product = input.requests_llm();
    format!(
        "facets × sketches_per_cell x temperatures × replicas\n\
         {} × {} × {} × {} = {}\n\
         \n\
         Calculation note: facets is the sum of facet counts\n\
         across all dimensions (= cells); temperatures and\n\
         replicas multiply that cell count to size the LLM fan-out.\n",
        input.cells, input.sketches_per_cell, input.temperatures, input.replicas, product,
    )
}

/// Render the "Results" block. The two lines follow the plan's
/// exact format: `Requests LLM = N` then `Surviving sketches = ≤ N`.
fn format_results(input: &ExplainInput) -> String {
    let product = input.requests_llm();
    format!(
        "Requests LLM = {}\n\
         Surviving sketches = ≤ {}",
        product, product,
    )
}

/// Convenience wrapper invoked by the CLI dispatcher. Loads the
/// config (the dispatcher already does this; this helper exists so
/// unit tests can drive the full surface in isolation), builds the
/// explain input, formats it, prints it to stdout, and returns the
/// exit code.
///
/// The dispatcher prints the formatted string itself (so it can be
/// captured by tests) — this helper only formats.
pub fn build_and_format(opts: &DiscoverOptions, _cfg: &Config) -> Result<String> {
    debug!("discover_explain::build_and_format: enter");
    // Validation: the operator-facing floor is enforced at the CLI layer
    // (the dispatcher rejects `sketches_per_cell <
    // MIN_SKETCHES_PER_CELL` — 1 as of v0.13.2), but we re-check
    // here so the explain path matches the real run's contract.
    // This way, `moagan discover --sketches-per-cell 0
    // --explain` errors out exactly the same way a non-explain
    // invocation would.
    if opts.sketches_per_cell < MIN_SKETCHES_PER_CELL {
        return Err(Error::InvalidArgs(format!(
            "sketches-per-cell {value} below the minimum of {MIN_SKETCHES_PER_CELL}",
            value = opts.sketches_per_cell,
        )));
    }
    let input = build_explain_input(opts, _cfg)?;
    Ok(format_explain(&input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::discover::TemperatureProfileSpec;

    fn opts() -> DiscoverOptions {
        DiscoverOptions {
            provider: "mock".to_string(),
            prompt: "x".to_string(),
            home: None,
            mock_dir: None,
            sketches_per_cell: DEFAULT_SKETCHES_PER_CELL,
            max_parallelism: None,
            dimensions: None,
            facets_per_dimension: None,
            matrix_spec: Vec::new(),
            llm_derive: false,
            cluster_threshold: 0.7,
            out_dir: None,
            non_interactive: false,
            cache_facets: false,
            temperature_profiles: Vec::new(),
            explain: false,
        }
    }

    #[test]
    fn value_source_display_is_stable() {
        // The Type column shows these exact strings — operators
        // may parse the output, so any rename surfaces here.
        assert_eq!(ValueSource::Default.to_string(), "Default");
        assert_eq!(ValueSource::Flag.to_string(), "Flag");
        assert_eq!(ValueSource::Env.to_string(), "Env");
        assert_eq!(ValueSource::Toml.to_string(), "Toml");
        assert_eq!(ValueSource::Spec.to_string(), "Spec");
        assert_eq!(ValueSource::Llm.to_string(), "Llm");
    }

    #[test]
    fn default_fallback_cells_is_eight() {
        // Pin the historical 4×2 default so a future refactor
        // cannot silently change the worst-case budget the
        // operator sees on a bare `moagan discover --explain`.
        assert_eq!(DEFAULT_FALLBACK_CELLS, 8);
    }

    #[test]
    fn build_input_no_flags_uses_eight_cell_default() {
        let cfg = Config::default();
        let input = build_explain_input(&opts(), &cfg).unwrap();
        assert_eq!(input.cells, 8);
        assert_eq!(input.source_cells, ValueSource::Default);
        assert_eq!(input.sketches_per_cell, 10);
        assert_eq!(input.source_sketches_per_cell, ValueSource::Default);
        assert_eq!(input.temperatures, 1);
        assert_eq!(input.replicas, 1);
        assert_eq!(input.source_temperatures, ValueSource::Default);
    }

    #[test]
    fn build_input_matrix_spec_resolves_as_spec() {
        let mut o = opts();
        o.matrix_spec = vec!["auth=oauth,api-key".to_string()];
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.cells, 2);
        assert_eq!(input.source_cells, ValueSource::Spec);
    }

    #[test]
    fn build_input_matrix_spec_sum_matches_plan_example() {
        // 2 + 3 = 5 cells. The plan uses this exact arithmetic
        // as the canonical "spec" example.
        let mut o = opts();
        o.matrix_spec = vec!["auth=oauth,api-key;scaling=vertical,horizontal,auto".to_string()];
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.cells, 5);
        assert_eq!(input.source_cells, ValueSource::Spec);
    }

    #[test]
    fn build_input_dimensions_plus_facets_resolves_as_flag() {
        let mut o = opts();
        o.dimensions = Some(4);
        o.facets_per_dimension = Some(2);
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.cells, 8);
        assert_eq!(input.source_cells, ValueSource::Flag);
    }

    #[test]
    fn build_input_dimensions_alone_uses_hint() {
        let mut o = opts();
        o.dimensions = Some(4);
        // No --facets-per-dimension: hint is N * 2 = 8.
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.cells, 8);
        assert_eq!(input.source_cells, ValueSource::Default);
    }

    #[test]
    fn build_input_llm_derive_reports_zero_cells() {
        let mut o = opts();
        o.llm_derive = true;
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.cells, 0);
        assert_eq!(input.source_cells, ValueSource::Llm);
    }

    #[test]
    fn build_input_sketches_per_cell_via_cli_is_flag() {
        let mut o = opts();
        o.sketches_per_cell = 25;
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.sketches_per_cell, 25);
        assert_eq!(input.source_sketches_per_cell, ValueSource::Flag);
    }

    #[test]
    fn build_input_sketches_per_cell_via_toml_is_toml() {
        let mut o = opts();
        o.sketches_per_cell = DEFAULT_SKETCHES_PER_CELL;
        let mut cfg = Config::default();
        cfg.discovery_matrix.sketches_per_cell = 40;
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.sketches_per_cell, 40);
        assert_eq!(input.source_sketches_per_cell, ValueSource::Toml);
    }

    #[test]
    fn build_input_temperature_profile_cli_is_flag() {
        let mut o = opts();
        o.temperature_profiles = vec![TemperatureProfileSpec {
            provider: "minimax-m3".to_string(),
            temperatures: vec![0.5, 0.7, 1.0, 1.3],
            replicas_per_temperature: 2,
        }];
        let cfg = Config::default();
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.temperatures, 4);
        assert_eq!(input.replicas, 2);
        assert_eq!(input.source_temperatures, ValueSource::Flag);
        assert_eq!(input.source_replicas, ValueSource::Flag);
    }

    #[test]
    fn build_input_default_profile_toml_is_toml() {
        let o = opts();
        let mut cfg = Config::default();
        cfg.discovery_matrix.default_profile = Some(crate::discovery::matrix::TemperatureProfile {
            temperatures: vec![0.0, 0.3, 0.7, 1.0],
            replicas_per_temperature: 4,
        });
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.temperatures, 4);
        assert_eq!(input.replicas, 4);
        assert_eq!(input.source_temperatures, ValueSource::Toml);
    }

    #[test]
    fn build_input_named_profile_toml_is_toml() {
        let o = opts();
        let mut cfg = Config::default();
        cfg.discovery_matrix.temperature_profiles.insert(
            "minimax-m3".to_string(),
            crate::discovery::matrix::TemperatureProfile {
                temperatures: vec![0.5],
                replicas_per_temperature: 3,
            },
        );
        let input = build_explain_input(&o, &cfg).unwrap();
        assert_eq!(input.temperatures, 1);
        assert_eq!(input.replicas, 3);
        assert_eq!(input.source_temperatures, ValueSource::Toml);
    }

    #[test]
    fn requests_llm_multiplies_all_four_knobs() {
        let input = ExplainInput {
            cells: 5,
            sketches_per_cell: 10,
            temperatures: 4,
            replicas: 2,
            source_cells: ValueSource::Spec,
            source_sketches_per_cell: ValueSource::Default,
            source_temperatures: ValueSource::Toml,
            source_replicas: ValueSource::Toml,
        };
        assert_eq!(input.requests_llm(), 5 * 10 * 4 * 2);
        // Same arithmetic as the plan's "Calculation" example
        // (`24 × 10 × 4 × 2 = 1920`).
        let big = ExplainInput { cells: 24, ..input };
        assert_eq!(big.requests_llm(), 1920);
    }

    /// Pin the wire format the plan documents. Any change to the
    /// output layout (column widths, headers, separators, blank
    /// lines) shows up here. Update both this test and the plan in
    /// lock-step.
    #[test]
    fn format_explain_matches_plan_example() {
        let input = ExplainInput {
            cells: 24,
            sketches_per_cell: 10,
            temperatures: 4,
            replicas: 2,
            source_cells: ValueSource::Default,
            source_sketches_per_cell: ValueSource::Flag,
            source_temperatures: ValueSource::Env,
            source_replicas: ValueSource::Env,
        };
        let out = format_explain(&input);
        let expected = "\
Values
------

| Value             | Size | Type    |
| ----------------- | ---- | ------- |
| facets            |   24 | Default |
| sketches_per_cell |   10 | Flag    |
| temperatures      |    4 | Env     |
| replicas          |    2 | Env     |

Calculation
-----------

facets × sketches_per_cell x temperatures × replicas
24 × 10 × 4 × 2 = 1920

Calculation note: facets is the sum of facet counts
across all dimensions (= cells); temperatures and
replicas multiply that cell count to size the LLM fan-out.

Results
-------

Requests LLM = 1920
Surviving sketches = ≤ 1920";
        assert_eq!(out, expected);
    }

    #[test]
    fn format_explain_omits_results_when_cells_zero() {
        let input = ExplainInput {
            cells: 0,
            sketches_per_cell: 10,
            temperatures: 1,
            replicas: 1,
            source_cells: ValueSource::Llm,
            source_sketches_per_cell: ValueSource::Default,
            source_temperatures: ValueSource::Default,
            source_replicas: ValueSource::Default,
        };
        let out = format_explain(&input);
        assert!(
            !out.contains("Results\n-------"),
            "Results block must be omitted when cells == 0; got:\n{out}"
        );
        assert!(
            !out.contains("Requests LLM"),
            "Requests LLM line must be omitted when cells == 0; got:\n{out}"
        );
    }

    #[test]
    fn format_explain_renders_table_column_widths() {
        // The plan pins the column widths:
        //   Value: 17 chars (" Value" + 1 trailing space)
        //   Size:  4 chars (right-aligned numbers)
        //   Type:  7 chars (left-aligned source name)
        // Verify by rendering a row and asserting the leading
        // '|' is followed by a single space and 16 more chars
        // before the next '|'.
        let input = ExplainInput {
            cells: 8,
            sketches_per_cell: 10,
            temperatures: 1,
            replicas: 1,
            source_cells: ValueSource::Default,
            source_sketches_per_cell: ValueSource::Default,
            source_temperatures: ValueSource::Default,
            source_replicas: ValueSource::Default,
        };
        let out = format_explain(&input);
        // Header + separator rows.
        assert!(out.contains("| Value             | Size |"), "got:\n{out}");
        assert!(out.contains("| ----------------- | ---- |"), "got:\n{out}");
        // Body row widths.
        assert!(out.contains("| facets            |    8 |"), "got:\n{out}");
        assert!(out.contains("| sketches_per_cell |   10 |"), "got:\n{out}");
        assert!(out.contains("| temperatures      |    1 |"), "got:\n{out}");
        assert!(out.contains("| replicas          |    1 |"), "got:\n{out}");
    }

    #[test]
    fn build_and_format_rejects_sketches_below_floor() {
        let mut o = opts();
        o.sketches_per_cell = 0;
        let cfg = Config::default();
        let err = build_and_format(&o, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("below the minimum of 1"),
            "error must mention the floor; got {err}"
        );
    }

    #[test]
    fn build_and_format_round_trip_default() {
        let cfg = Config::default();
        let out = build_and_format(&opts(), &cfg).unwrap();
        // Spot-check the structural pieces without pinning the
        // exact line lengths (those live in
        // `format_explain_matches_plan_example`).
        assert!(out.starts_with("Values\n------\n\n"));
        assert!(out.contains("\n\nCalculation\n-----------\n\n"));
        assert!(out.contains("facets × sketches_per_cell x temperatures × replicas"));
        assert!(out.contains("8 × 10 × 1 × 1 = 80"));
        assert!(out.contains("Requests LLM = 80"));
        assert!(out.contains("Surviving sketches = ≤ 80"));
    }

    #[test]
    fn matrix_spec_invalid_propagates_error() {
        let mut o = opts();
        o.matrix_spec = vec!["not-a-spec".to_string()];
        let cfg = Config::default();
        let err = build_and_format(&o, &cfg).unwrap_err();
        // The F1 parser's error message includes "missing `=`";
        // we don't pin the exact text here, just that the explain
        // path surfaces it rather than silently defaulting.
        assert!(
            err.to_string().contains("missing `=`"),
            "invalid spec must surface a parse error; got {err}"
        );
    }
}
