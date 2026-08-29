//! Dual-mode deserializer for the v0.13.0 `[[providers.<name>]]` config
//! schema.
//!
//! ## Why this exists
//!
//! v0.13.0 (`B-1` of the v0.13 config redesign) replaces the legacy
//! `[providers.<name>]` single-table + `Vec<ModelConfig>` shape with
//! `[[providers.<name>]]` array-of-tables + flat model-id strings.
//! Operators with v0.12.x `config.toml` files must keep loading without
//! action during the v0.13.x deprecation window — the change is
//! unavoidable on the binary side, but the file format stays
//! backwards-compatible.
//!
//! This module owns the deserializer bridge:
//!
//! 1. `deserialize_providers_map` accepts a `providers` table whose
//!    values are EITHER a single TOML table (legacy) OR a TOML array of
//!    tables (new). It dispatches per section, emits a `tracing::warn!`
//!    once per legacy section, and groups the legacy models by their
//!    effective endpoint so the bridge `Config::compute_legacy_providers`
//!    sees the same `Vec<ProviderEntry>` shape on both sides.
//! 2. `deserialize_model_list` accepts a `models` array whose elements
//!    are EITHER strings (new, e.g. `["kimi-k3", "glm-5.1"]`) OR inline
//!    tables with `id` (legacy, e.g. `[{ id = "kimi-k3" }]`). Legacy
//!    entries emit a single deprecation warning per call (the per-section
//!    warning is emitted by the map deserializer; this is the
//!    per-array fallback for callers that wire `deserialize_model_list`
//!    on a non-`providers` field).
//!
//! ## Mock corner
//!
//! The `mock` section is the only one whose `ProviderConfig::endpoint`
//! is consulted by `is_mock` (see `src/llm/provider.rs:1351-1356`).
//! After the bridge converts `Vec<ProviderEntry>` to
//! `BTreeMap<String, ProviderConfig>`, every non-mock section has
//! `endpoint = None` (the entry now carries the URL). The
//! `compute_legacy_providers` helper detects the `mock` name and
//! propagates the first entry's endpoint to the legacy field so the
//! runtime check keeps working unchanged.

use std::collections::BTreeMap;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

use super::{ProviderEntry, SectionKnobs};

/// Deserialize the `models` field of a `[[providers.<name>]]` entry as
/// `Vec<String>` (the v0.13 canonical form) while still accepting the
/// v0.12 legacy `Vec<ModelConfig>` shape (a list of inline tables with
/// at least an `id` field).
///
/// The function is generic over the underlying deserializer: it asks
/// the visitor for a sequence of `toml::Value` and then collapses each
/// element to a `String`. Plain strings pass through; tables have
/// their `id` extracted and trigger a single `tracing::warn!` for the
/// rest of the call (per-section deprecation is emitted by
/// [`deserialize_providers_map`], which is the canonical call-site).
pub(crate) fn deserialize_model_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModelListVisitor;

    impl<'de> Visitor<'de> for ModelListVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                r#"a list of model IDs (["kimi-k3", "glm-5.1"]) or legacy model objects ([{id = "kimi-k3"}])"#,
            )
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Vec<String>, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            // `toml::Value` deserializes from any deserializer that
            // produces compatible shapes (map/array/scalar). Pull the
            // sequence into `Vec<toml::Value>` and then collapse.
            let mut values: Vec<toml::Value> = Vec::new();
            while let Some(item) = seq.next_element::<toml::Value>()? {
                values.push(item);
            }
            let mut ids: Vec<String> = Vec::with_capacity(values.len());
            let mut legacy_warned = false;
            for item in values {
                match item {
                    toml::Value::String(s) => ids.push(s),
                    toml::Value::Table(t) => {
                        if !legacy_warned {
                            tracing::warn!(
                                "config: legacy `models = [{{id = \"...\", ...}}]` is deprecated in \
                                 v0.13; use `models = [\"<id>\", ...]` instead. The \
                                 `max_tokens` / per-model `endpoint` fields are now resolved \
                                 centrally (see `resolve_max_tokens` in PR #3 and the v0.13 ADR)."
                            );
                            legacy_warned = true;
                        }
                        let id = match t.get("id") {
                            Some(toml::Value::String(s)) => s.clone(),
                            // The legacy shape carries `id` as a string;
                            // anything else is a typo we surface instead
                            // of silently dropping.
                            Some(other) => {
                                return Err(de::Error::custom(format!(
                                    "config: legacy `models[]` entry `id` must be a string, got {}",
                                    other.type_str()
                                )));
                            }
                            None => {
                                return Err(de::Error::custom(
                                    "config: legacy `models[]` entry missing required `id` field",
                                ));
                            }
                        };
                        ids.push(id);
                    }
                    other => {
                        return Err(de::Error::custom(format!(
                            "config: `models[]` entries must be strings or tables, got {}",
                            other.type_str()
                        )));
                    }
                }
            }
            Ok(ids)
        }
    }

    deserializer.deserialize_seq(ModelListVisitor)
}

/// Deserialize the `providers` field of `Config` as
/// `BTreeMap<String, Vec<ProviderEntry>>` while transparently
/// accepting either the v0.13 array-of-tables form or the v0.12
/// legacy single-table form (one warning per legacy section).
///
/// Each entry value is dispatched independently: an array becomes a
/// straight `Vec<ProviderEntry>`, a table becomes a single-entry
/// `Vec<ProviderEntry>` after grouping the legacy models by their
/// effective endpoint.
pub(crate) fn deserialize_providers_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, Vec<ProviderEntry>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ProvidersVisitor;

    impl<'de> Visitor<'de> for ProvidersVisitor {
        type Value = BTreeMap<String, Vec<ProviderEntry>>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                "a map of provider name -> {endpoint, models=[...] | [providers.<name>] legacy table}",
            )
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut out: BTreeMap<String, Vec<ProviderEntry>> = BTreeMap::new();
            // Pull every (name, raw_value) pair as `toml::Value` so we
            // can dispatch on the shape (array-of-tables vs single
            // table). Using `toml::Value` here keeps the helper
            // agnostic of the underlying file format (TOML in
            // production; future YAML/JSON just needs the dual-mode
            // visitor to map to the same `toml::Value` shape).
            while let Some((name, raw)) = map.next_entry::<String, toml::Value>()? {
                let entries = match raw {
                    toml::Value::Array(arr) => {
                        parse_new_array(&name, arr).map_err(de::Error::custom)?
                    }
                    toml::Value::Table(tbl) => {
                        tracing::warn!(
                            section = %name,
                            "config: `[providers.{name}]` (single-table form) is \
                             deprecated in v0.13; use `[[providers.{name}]]` \
                             (array-of-tables) with `models = [\"<id>\", ...]` instead. \
                             The conversion preserves the legacy shape in-memory; \
                             support for the single-table form will be removed in v0.15."
                        );
                        parse_legacy_table(&name, tbl).map_err(de::Error::custom)?
                    }
                    other => {
                        return Err(de::Error::custom(format!(
                            "providers.{name}: expected an array-of-tables or a single table, got {}",
                            other.type_str()
                        )));
                    }
                };
                out.insert(name, entries);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_map(ProvidersVisitor)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse the v0.13 `[[providers.<name>]]` array form. Each element
/// must be a table; `endpoint` and `models` are required per entry.
/// `models` may be either the new `Vec<String>` form or the legacy
/// `Vec<ModelConfig>` form (warning is per-section, already emitted
/// by the caller).
fn parse_new_array(
    section: &str,
    arr: Vec<toml::Value>,
) -> Result<Vec<ProviderEntry>, de::value::Error> {
    let mut entries: Vec<ProviderEntry> = Vec::with_capacity(arr.len());
    for (i, elem) in arr.into_iter().enumerate() {
        let toml::Value::Table(tbl) = elem else {
            return Err(de::Error::custom(format!(
                "providers.{section}[{i}]: array elements must be tables, got {}",
                elem.type_str()
            )));
        };
        let entry: ProviderEntry = ProviderEntry::deserialize(toml::Value::Table(tbl))
            .map_err(|e| de::Error::custom(format!("providers.{section}[{i}]: {e}")))?;
        // Empty endpoint is a hard error per the v0.13 spec: the
        // array-of-tables form does not inherit a section-level
        // default, so each entry must carry its own URL. Whitespace
        // is rejected too — `ProviderConfig::endpoint` is later
        // matched by URL prefix to pick the wire format, so a URL
        // of `"   "` would silently route to nothing.
        if entry.endpoint.trim().is_empty() {
            return Err(de::Error::custom(format!(
                "providers.{section}[{i}]: `endpoint` is required in v0.13"
            )));
        }
        // Warn-and-drop for empty `models = []` (legacy form would
        // produce one entry with no models — kept for operator-side
        // "section exists but disabled" semantics). The entry stays
        // in the `Vec` so the section header survives a partial
        // removal in the operator's TOML; the runtime sees an empty
        // `models[]` and falls back to the section default.
        if entry.models.is_empty() {
            tracing::warn!(
                section = %section,
                index = i,
                "config: `[[providers.{section}]]` entry has empty `models = []`; \
                 the section will be treated as inactive. Add at least one model id \
                 (e.g. `models = [\"<id>\"]`) to register the entry."
            );
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Parse the v0.12 legacy `[providers.<name>]` single-table form
/// into `Vec<ProviderEntry>` (typically one entry per unique
/// endpoint; a section without per-model endpoint overrides collapses
/// into a single entry).
///
/// The grouping rule: each legacy `ModelConfig` carries its own
/// `endpoint` (optional). When absent, it falls back to the
/// section-level `endpoint`. Models that resolve to the same effective
/// endpoint land in the same `ProviderEntry` so the bridge sees a
/// stable `(endpoint, models)` pair.
fn parse_legacy_table(
    section: &str,
    tbl: toml::value::Table,
) -> Result<Vec<ProviderEntry>, de::value::Error> {
    // Pull the per-section knobs and per-model list via separate
    // helpers so the field naming can drift between the two without
    // touching this function.
    let section_endpoint = match tbl.get("endpoint") {
        Some(toml::Value::String(s)) => {
            // Whitespace-only endpoints are silently coerced to
            // `None` later (the bridge treats them as "no endpoint
            // set"). Surface them here as a hard error so the
            // operator learns about the typo at load time, not as
            // a `provider has no endpoint` failure at first LLM
            // call. Aligns with `parse_new_array`, which rejects
            // empty / whitespace endpoints as well (H-5 fix).
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(de::Error::custom(format!(
                    "providers.{section}.endpoint must be a non-empty URL, \
                     got whitespace-only or empty string"
                )));
            }
            Some(s.clone())
        }
        None => None,
        Some(other) => {
            return Err(de::Error::custom(format!(
                "providers.{section}.endpoint must be a string, got {}",
                other.type_str()
            )));
        }
    };
    let knobs: SectionKnobs = SectionKnobs::deserialize(toml::Value::Table(tbl.clone()))
        .map_err(|e| de::Error::custom(format!("providers.{section} knobs: {e}")))?;
    let models_value = tbl
        .get("models")
        .cloned()
        .unwrap_or(toml::Value::Array(Vec::new()));
    let legacy_models: Vec<LegacyModel> = match models_value {
        toml::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, item) in arr.into_iter().enumerate() {
                match item {
                    toml::Value::Table(t) => {
                        let m = LegacyModel::deserialize(toml::Value::Table(t)).map_err(|e| {
                            de::Error::custom(format!("providers.{section}.models[{i}]: {e}"))
                        })?;
                        out.push(m);
                    }
                    // The legacy form expected inline tables; a string
                    // here is a mixed-array shape that we reject with
                    // an actionable message (see plan §7.B.28).
                    toml::Value::String(s) => {
                        return Err(de::Error::custom(format!(
                            "providers.{section}.models[{i}]: legacy single-table form \
                             mixes objects and strings; either convert all entries to \
                             `[[providers.{section}]]` arrays, or use `{{id = \"{s}\"}}` \
                             for legacy compatibility"
                        )));
                    }
                    other => {
                        return Err(de::Error::custom(format!(
                            "providers.{section}.models[{i}]: expected a table, got {}",
                            other.type_str()
                        )));
                    }
                }
            }
            out
        }
        other => {
            return Err(de::Error::custom(format!(
                "providers.{section}.models: expected an array, got {}",
                other.type_str()
            )));
        }
    };

    // Empty legacy sections are allowed (the v0.12 schema treated
    // them as "section exists but disabled" — the operator uses
    // this to comment out a relay without removing the section
    // header). Emit a single `tracing::warn!` and return one entry
    // with the section-level endpoint and no models so the bridge
    // produces a `ProviderConfig { models: [], endpoint: None }`
    // view identical to the v0.12 shape.
    if legacy_models.is_empty() {
        tracing::warn!(
            section = %section,
            "config: legacy `[providers.{section}]` has no `models[]` entries; \
             the section is treated as inactive. Use `[[providers.{section}]] \
             models = [\"id\"]` in v0.13 to register models explicitly."
        );
        return Ok(vec![ProviderEntry {
            endpoint: section_endpoint.clone().unwrap_or_default(),
            models: Vec::new(),
            legacy_model_max_tokens: BTreeMap::new(),
            knobs: knobs.clone(),
        }]);
    }

    // Group by effective endpoint AND capture each model's
    // `max_tokens` so the bridge can preserve the operator-set cap
    // (v0.12 had per-model `max_tokens`; v0.13 drops the knob from
    // the new schema but the bridge must keep it for backwards
    // compat until `resolve_max_tokens` lands in PR #4).
    let mut groups: BTreeMap<String, (Vec<String>, bool)> = BTreeMap::new();
    let mut per_model_max_tokens: BTreeMap<String, u32> = BTreeMap::new();
    for m in legacy_models {
        if let Some(cap) = m.max_tokens {
            per_model_max_tokens.insert(m.id.clone(), cap);
        }
        // Pull the per-model endpoint (if any) into a local so we
        // can use it both for resolution and for the
        // `from_section` bookkeeping below.
        let model_endpoint = m.endpoint.clone();
        let effective = m
            .endpoint
            .or_else(|| section_endpoint.clone())
            .ok_or_else(|| {
                de::Error::custom(format!(
                    "providers.{section}: legacy model `{}` has no endpoint \
                     (neither section-level nor per-model `endpoint` is set)",
                    m.id
                ))
            })?;
        // Track whether the section-level endpoint was the source so
        // we can attach the section knobs only to that single entry
        // (otherwise we'd duplicate them across grouped entries).
        let from_section = model_endpoint.is_none();
        groups
            .entry(effective)
            .or_insert_with(|| (Vec::new(), from_section))
            .0
            .push(m.id);
    }

    // Each group becomes one `ProviderEntry`. The first group whose
    // section-level endpoint won the resolution gets the section
    // knobs; the rest get empty knobs (the bridge will not double-
    // apply them). Per-model `max_tokens` is captured in the
    // `legacy_model_max_tokens` side-channel so the bridge can
    // populate `ModelConfig::max_tokens` and the operator-side cap
    // chain keeps working until PR #4's `resolve_max_tokens` lands.
    let mut entries: Vec<ProviderEntry> = Vec::with_capacity(groups.len());
    for (endpoint, (model_ids, from_section)) in groups {
        let entry_knobs = if from_section {
            knobs.clone()
        } else {
            SectionKnobs::default()
        };
        let mut legacy_model_max_tokens: BTreeMap<String, u32> = BTreeMap::new();
        for id in &model_ids {
            if let Some(&cap) = per_model_max_tokens.get(id) {
                legacy_model_max_tokens.insert(id.clone(), cap);
            }
        }
        entries.push(ProviderEntry {
            endpoint,
            models: model_ids,
            legacy_model_max_tokens,
            knobs: entry_knobs,
        });
    }
    Ok(entries)
}
/// only the fields we need to extract (`id` plus the optional
/// per-model `endpoint` and `max_tokens` overrides).
#[derive(Debug, Clone, Deserialize)]
struct LegacyModel {
    id: String,
    #[serde(default)]
    endpoint: Option<String>,
    /// Per-model `max_tokens` ceiling. Preserved for backwards
    /// compat with v0.12 operators who set this per model; the
    /// v0.13 schema drops the knob from the array-of-tables form
    /// and resolves it centrally via
    /// `crate::llm::max_tokens::resolve_max_tokens` (PR #4).
    #[serde(default)]
    max_tokens: Option<u32>,
    // All other fields (`kind`, `hard_incompatibilities`, ...) are
    // silently dropped: they were v0.9 deprecated knobs and the
    // schema is gone in v0.13.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: deserialize a TOML fragment as `BTreeMap<String, Vec<ProviderEntry>>`.
    fn parse_providers(toml_str: &str) -> BTreeMap<String, Vec<ProviderEntry>> {
        let raw: toml::Value = toml::from_str(toml_str).expect("TOML parse");
        let table = raw.as_table().expect("root table");
        let providers_value = table.get("providers").expect("providers key").clone();
        // Wrap into a single-key map so we can reuse the visitor
        // directly via `Vec<ProviderEntry>` parsing per section.
        let mut by_section = BTreeMap::new();
        if let toml::Value::Table(provs) = providers_value {
            for (name, raw) in provs {
                let entries = match raw {
                    toml::Value::Array(arr) => {
                        parse_new_array(&name, arr).expect("new array parse")
                    }
                    toml::Value::Table(tbl) => {
                        parse_legacy_table(&name, tbl).expect("legacy table parse")
                    }
                    other => panic!("unexpected shape: {other:?}"),
                };
                by_section.insert(name, entries);
            }
        }
        by_section
    }

    #[test]
    fn new_array_of_tables_deserialises_clean() {
        let toml_str = r#"
[[providers.minimax]]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
models = ["MiniMax-M3", "MiniMax-M2.5"]
temperature = 0.6
"#;
        let parsed = parse_providers(toml_str);
        let mm = parsed.get("minimax").expect("minimax entry");
        assert_eq!(mm.len(), 1);
        assert_eq!(
            mm[0].endpoint,
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(mm[0].models, vec!["MiniMax-M3", "MiniMax-M2.5"]);
        assert_eq!(mm[0].knobs.temperature, Some(0.6));
        assert!(!mm[0].knobs.omit_max_tokens);
        // max_token_auto_save defaults to `true` per the helper.
        assert!(mm[0].knobs.max_token_auto_save);
    }

    #[test]
    fn new_array_of_tables_multiple_entries_per_section() {
        let toml_str = r#"
[[providers.opencode]]
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"
models = ["kimi-k3", "glm-5.1"]

[[providers.opencode]]
endpoint = "https://opencode.ai/zen/go/v1/messages"
models = ["minimax-m3"]
temperature = 1.0
"#;
        let parsed = parse_providers(toml_str);
        let oc = parsed.get("opencode").expect("opencode entry");
        assert_eq!(oc.len(), 2);
        assert_eq!(
            oc[0].endpoint,
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
        assert_eq!(oc[0].models, vec!["kimi-k3", "glm-5.1"]);
        assert_eq!(oc[1].endpoint, "https://opencode.ai/zen/go/v1/messages");
        assert_eq!(oc[1].models, vec!["minimax-m3"]);
        assert_eq!(oc[1].knobs.temperature, Some(1.0));
    }

    #[test]
    fn legacy_single_table_groups_by_endpoint() {
        let toml_str = r#"
[providers.minimax]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
temperature = 0.6

[[providers.minimax.models]]
id = "MiniMax-M3"

[[providers.minimax.models]]
id = "MiniMax-M2.5"
"#;
        let parsed = parse_providers(toml_str);
        let mm = parsed.get("minimax").expect("minimax entry");
        assert_eq!(
            mm.len(),
            1,
            "single section-level endpoint collapses to one entry"
        );
        assert_eq!(
            mm[0].endpoint,
            "https://api.minimax.io/anthropic/v1/messages"
        );
        assert_eq!(mm[0].models, vec!["MiniMax-M3", "MiniMax-M2.5"]);
        assert_eq!(mm[0].knobs.temperature, Some(0.6));
    }

    #[test]
    fn legacy_distinct_endpoints_produce_separate_entries() {
        let toml_str = r#"
[providers.opencode]
temperature = 1.0

[[providers.opencode.models]]
id = "kimi-k3"
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"

[[providers.opencode.models]]
id = "minimax-m3"
endpoint = "https://opencode.ai/zen/go/v1/messages"
"#;
        let parsed = parse_providers(toml_str);
        let oc = parsed.get("opencode").expect("opencode entry");
        assert_eq!(oc.len(), 2, "per-model endpoints become separate entries");
        // The first entry holds the section knobs (since it was the
        // first to fall back to the section-level for endpoint
        // resolution — but here both have per-model endpoints, so
        // both fall through to `from_section = false` and the knobs
        // attach to neither).
        for entry in oc {
            assert_eq!(entry.knobs.temperature, None);
        }
        let chat = oc
            .iter()
            .find(|e| e.endpoint.contains("chat/completions"))
            .expect("chat entry");
        assert_eq!(chat.models, vec!["kimi-k3"]);
        let messages = oc
            .iter()
            .find(|e| e.endpoint.contains("/messages"))
            .expect("messages entry");
        assert_eq!(messages.models, vec!["minimax-m3"]);
    }

    #[test]
    fn legacy_models_with_section_endpoint_attach_knobs_to_first_group() {
        let toml_str = r#"
[providers.opencode]
endpoint = "https://opencode.ai/zen/go/v1/chat/completions"
temperature = 1.0

[[providers.opencode.models]]
id = "kimi-k3"

[[providers.opencode.models]]
id = "minimax-m3"
endpoint = "https://opencode.ai/zen/go/v1/messages"
"#;
        let parsed = parse_providers(toml_str);
        let oc = parsed.get("opencode").expect("opencode entry");
        assert_eq!(oc.len(), 2);
        // The chat endpoint group inherits the section knobs.
        let chat = oc
            .iter()
            .find(|e| e.endpoint.contains("chat/completions"))
            .expect("chat entry");
        assert_eq!(chat.knobs.temperature, Some(1.0));
        // The messages endpoint group has per-model endpoints, so it
        // does NOT inherit the section knobs (avoids double-apply).
        let messages = oc
            .iter()
            .find(|e| e.endpoint.contains("/messages"))
            .expect("messages entry");
        assert_eq!(messages.knobs.temperature, None);
    }

    #[test]
    fn new_array_with_empty_endpoint_is_rejected() {
        let toml_str = r#"
[[providers.broken]]
models = ["a", "b"]
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let tbl = raw.as_table().unwrap();
        let provs = tbl.get("providers").and_then(|v| v.as_table()).unwrap();
        let arr = provs
            .get("broken")
            .and_then(|v| v.as_array())
            .expect("broken array");
        let result = parse_new_array("broken", arr.clone());
        assert!(result.is_err(), "empty endpoint must error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("`endpoint` is required"),
            "error must mention the missing endpoint; got: {msg}"
        );
    }

    /// H-5 regression: `parse_legacy_table` previously returned
    /// `ProviderEntry { endpoint: "", ... }` when both the section
    /// and per-model endpoints were missing. `parse_new_array`
    /// already rejected the empty case (see
    /// `new_array_with_empty_endpoint_is_rejected` above); the
    /// legacy form must do the same so the operator sees the typo
    /// at load time instead of a `provider has no endpoint`
    /// failure at the first LLM call.
    #[test]
    fn legacy_with_empty_endpoint_is_rejected() {
        let toml_str = r#"
[providers.broken]
endpoint = ""

[[providers.broken.models]]
id = "orphan"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let tbl = raw.as_table().unwrap();
        let provs = tbl.get("providers").and_then(|v| v.as_table()).unwrap();
        let broken_tbl = provs
            .get("broken")
            .and_then(|v| v.as_table())
            .expect("table");
        let result = parse_legacy_table("broken", broken_tbl.clone());
        assert!(result.is_err(), "empty endpoint must error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("non-empty URL"),
            "error must mention the empty endpoint; got: {msg}"
        );
    }

    /// Whitespace-only endpoint must be rejected too — the URL is
    /// matched by prefix to pick the wire format, so `"   "` would
    /// silently route to nothing at first LLM call.
    #[test]
    fn legacy_with_whitespace_endpoint_is_rejected() {
        let toml_str = r#"
[providers.broken]
endpoint = "   "

[[providers.broken.models]]
id = "orphan"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let tbl = raw.as_table().unwrap();
        let provs = tbl.get("providers").and_then(|v| v.as_table()).unwrap();
        let broken_tbl = provs
            .get("broken")
            .and_then(|v| v.as_table())
            .expect("table");
        let result = parse_legacy_table("broken", broken_tbl.clone());
        assert!(result.is_err(), "whitespace endpoint must error");
    }

    #[test]
    fn legacy_without_models_array_is_accepted_as_inactive() {
        let toml_str = r#"
[providers.broken]
endpoint = "https://example.com/v1/messages"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let tbl = raw.as_table().unwrap();
        let provs = tbl.get("providers").and_then(|v| v.as_table()).unwrap();
        let broken_tbl = provs
            .get("broken")
            .and_then(|v| v.as_table())
            .expect("table");
        let result = parse_legacy_table("broken", broken_tbl.clone());
        // Empty legacy sections are accepted (v0.12 used this as a
        // "section exists but disabled" signal). The bridge turns
        // them into a single entry with no models so the runtime
        // sees an empty `ProviderConfig.models` list — identical to
        // the v0.12 behaviour.
        let entries = result.expect("empty legacy section must load");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].models.len(), 0);
        assert_eq!(entries[0].endpoint, "https://example.com/v1/messages");
    }

    #[test]
    fn legacy_model_without_any_endpoint_is_rejected() {
        let toml_str = r#"
[providers.broken]
[[providers.broken.models]]
id = "orphan"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let tbl = raw.as_table().unwrap();
        let provs = tbl.get("providers").and_then(|v| v.as_table()).unwrap();
        let broken_tbl = provs
            .get("broken")
            .and_then(|v| v.as_table())
            .expect("table");
        let result = parse_legacy_table("broken", broken_tbl.clone());
        assert!(result.is_err(), "model with no endpoint must error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no endpoint"),
            "error must mention the missing endpoint; got: {msg}"
        );
    }

    #[test]
    fn deserialize_model_list_accepts_flat_strings() {
        let raw: Vec<String> = serde_json::from_str(r#"["a", "b"]"#).unwrap();
        assert_eq!(raw, vec!["a", "b"]);
    }

    #[test]
    fn new_section_knobs_merge_first_wins_via_struct_default() {
        // First entry carries knobs; subsequent entries might also
        // (the bridge picks first non-default wins via
        // `SectionKnobs::merge_first_wins`). Confirm the per-entry
        // shape is preserved verbatim by the deserializer.
        let toml_str = r#"
[[providers.test]]
endpoint = "https://example.com/v1/a"
models = ["m1"]
temperature = 0.7
max_token_auto = 2048

[[providers.test]]
endpoint = "https://example.com/v1/b"
models = ["m2"]
"#;
        let parsed = parse_providers(toml_str);
        let entries = parsed.get("test").expect("test section");
        assert_eq!(entries.len(), 2);
        // Only the first entry carries the knobs (the bridge's
        // first-non-default wins rule then merges anything the
        // second entry set, but this fixture leaves the second
        // entry knob-less).
        assert_eq!(entries[0].knobs.temperature, Some(0.7));
        assert_eq!(entries[0].knobs.max_token_auto, Some(2048));
        assert_eq!(entries[1].knobs.temperature, None);
        assert_eq!(entries[1].knobs.max_token_auto, None);
    }

    #[test]
    fn legacy_per_model_max_tokens_propagates_via_side_channel() {
        // Backwards-compat: a v0.12 legacy TOML with per-model
        // `max_tokens = N` keeps the value through the bridge so
        // the operator-side cap chain (`MinimaxProvider::send`
        // clamps to `provider_max_tokens`) keeps working until PR
        // #4 lands the central `resolve_max_tokens` helper.
        let toml_str = r#"
[providers.minimax]
endpoint = "https://api.minimax.io/anthropic/v1/messages"

[[providers.minimax.models]]
id = "MiniMax-M2.7"
max_tokens = 131072
"#;
        let parsed = parse_providers(toml_str);
        let entries = parsed.get("minimax").expect("minimax section");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].models, vec!["MiniMax-M2.7"]);
        assert_eq!(
            entries[0].legacy_model_max_tokens.get("MiniMax-M2.7"),
            Some(&131_072_u32),
            "legacy per-model max_tokens must propagate through the bridge side-channel"
        );
    }

    #[test]
    fn new_schema_does_not_populate_legacy_side_channel() {
        // The v0.13 array-of-tables form has no per-model
        // `max_tokens` knob. The bridge leaves the side-channel
        // empty so the bridge populates `ModelConfig::max_tokens =
        // None` for new-schema entries (the runtime resolver
        // fills it in once PR #4 lands).
        let toml_str = r#"
[[providers.test]]
endpoint = "https://example.com/v1/messages"
models = ["m1", "m2"]
"#;
        let parsed = parse_providers(toml_str);
        let entries = parsed.get("test").expect("test section");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].legacy_model_max_tokens.is_empty());
    }

    #[test]
    fn mixed_legacy_and_new_load_side_by_side() {
        // Mixed TOML: one section in legacy form, another in the
        // new array-of-tables form. Both load; only the legacy
        // section emits the deprecation warning.
        let toml_str = r#"
[providers.legacy_section]
endpoint = "https://legacy.example.com/v1/messages"

[[providers.legacy_section.models]]
id = "legacy-model"
max_tokens = 4096

[[providers.new_section]]
endpoint = "https://new.example.com/v1/messages"
models = ["new-model-1", "new-model-2"]
"#;
        let parsed = parse_providers(toml_str);
        let legacy = parsed.get("legacy_section").expect("legacy section");
        let new_sec = parsed.get("new_section").expect("new section");
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].models, vec!["legacy-model"]);
        assert_eq!(
            legacy[0].legacy_model_max_tokens.get("legacy-model"),
            Some(&4096)
        );
        assert_eq!(new_sec.len(), 1);
        assert_eq!(new_sec[0].models, vec!["new-model-1", "new-model-2"]);
        assert!(new_sec[0].legacy_model_max_tokens.is_empty());
    }
}
