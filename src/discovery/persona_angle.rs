//! PersonaPicker + AnglePicker helpers for `DiscoveryCoordinator`.
//!
//! The two D.7.1 catalog roles [`Role::PersonaPicker`] and
//! [`Role::AnglePicker`] are wired through the prompt + schema layer
//! (PR #79 batch-2) but nothing in the runtime invokes them. This
//! module exposes two helpers that downstream phases (or the
//! Discovery sketch loop, in a follow-up PR) can call to ask the
//! model for the next persona / angle.
//!
//! The helpers are opt-in. Each gate is a field on
//! [`DiscoveryWiringConfig`]:
//!
//! * `persona_enabled: false` (default) — `pick_persona` returns
//!   `Ok(None)` immediately, no LLM call, no telemetry.
//! * `angle_enabled: false` (default) — `pick_angle` returns
//!   `Ok(None)` immediately, no LLM call, no telemetry, no legacy
//!   mutation.
//!
//! When enabled, the helpers go through the canonical
//! `RunContext::call_with_retry_parse` so the retry / parse /
//! telemetry pipeline matches the rest of the catalog invocations.
//!
//! The chosen angle is appended to
//! [`EpistemicLegacy::preferred_strategies`] so subsequent runs build
//! on prior choices. The legacy is passed explicitly (instead of
//! fetched from `RunContext`) so the helpers remain unit-testable
//! without spinning up a discovery coordinator.
//!
//! [`Role::PersonaPicker`]: crate::llm::Role::PersonaPicker
//! [`Role::AnglePicker`]: crate::llm::Role::AnglePicker
//! [`DiscoveryWiringConfig`]: crate::config::DiscoveryWiringConfig
//! [`EpistemicLegacy::preferred_strategies`]: super::epistemic_legacy::EpistemicLegacy::preferred_strategies

use crate::Result;
use crate::domain::{AnglePickerReport, PersonaPickerReport};
use crate::llm::Role;
use crate::llm::prompts::system_prompt;
use crate::phases::phase::RunContext;

use super::epistemic_legacy::EpistemicLegacy;

/// Pick a persona from the supplied candidate pool.
///
/// Short-circuits to `Ok(None)` when the wiring is disabled
/// (`ctx.config.discovery.persona_enabled` is `false`) or when the
/// candidate list is empty. Otherwise calls
/// `Role::PersonaPicker` via the canonical
/// `RunContext::call_with_retry_parse` and returns the selected
/// persona id.
pub async fn pick_persona(ctx: &RunContext, candidates: Vec<String>) -> Result<Option<String>> {
    tracing::debug!(
        candidates = candidates.len(),
        persona_enabled = ctx.config.discovery.persona_enabled,
        "pick_persona: enter (async)"
    );
    if !ctx.config.discovery.persona_enabled {
        tracing::trace!("pick_persona: short-circuit (persona_enabled=false)");
        return Ok(None);
    }
    if candidates.is_empty() {
        tracing::trace!("pick_persona: short-circuit (empty candidates)");
        return Ok(None);
    }
    let user = serde_json::json!({ "candidates": candidates });
    let user_str = serde_json::to_string(&user).unwrap_or_default();
    let response: serde_json::Value = ctx
        .call_with_retry_parse(
            Role::PersonaPicker,
            system_prompt(Role::PersonaPicker).to_owned(),
            user_str,
            "PersonaPicker: {selected, rationale}",
            2,
        )
        .await?;
    let report: PersonaPickerReport = serde_json::from_value(response)?;
    tracing::info!(
        selected = %report.selected,
        rationale = %report.rationale,
        "pick_persona: selected"
    );
    Ok(Some(report.selected))
}

/// Pick the next exploration angle distinct from the supplied
/// cluster list.
///
/// Short-circuits to `Ok(None)` when the wiring is disabled
/// (`ctx.config.discovery.angle_enabled` is `false`) or when the
/// cluster list is below `angle_clusters_min`. Otherwise calls
/// `Role::AnglePicker` and, on success, appends the chosen angle to
/// [`EpistemicLegacy::preferred_strategies`] so subsequent runs
/// build on prior choices. The `legacy` parameter is the caller's
/// mutable view of the in-memory legacy; persistence is the caller's
/// responsibility (the helper is pure with respect to disk).
///
/// [`EpistemicLegacy::preferred_strategies`]: super::epistemic_legacy::EpistemicLegacy::preferred_strategies
pub async fn pick_angle(
    ctx: &RunContext,
    legacy: &mut EpistemicLegacy,
    clusters: Vec<String>,
) -> Result<Option<String>> {
    tracing::debug!(
        clusters = clusters.len(),
        angle_enabled = ctx.config.discovery.angle_enabled,
        "pick_angle: enter (async)"
    );
    if !ctx.config.discovery.angle_enabled {
        tracing::trace!("pick_angle: short-circuit (angle_enabled=false)");
        return Ok(None);
    }
    let min_clusters = ctx.config.discovery.angle_clusters_min;
    if clusters.len() < min_clusters {
        tracing::trace!(
            clusters = clusters.len(),
            min_clusters,
            "pick_angle: short-circuit (below min_clusters)"
        );
        return Ok(None);
    }
    let user = serde_json::json!({ "clusters": clusters });
    let user_str = serde_json::to_string(&user).unwrap_or_default();
    let response: serde_json::Value = ctx
        .call_with_retry_parse(
            Role::AnglePicker,
            system_prompt(Role::AnglePicker).to_owned(),
            user_str,
            "AnglePicker: {selected, rationale}",
            2,
        )
        .await?;
    let report: AnglePickerReport = serde_json::from_value(response)?;
    legacy
        .preferred_strategies
        .push(format!("angle:{}", report.selected));
    tracing::info!(
        selected = %report.selected,
        rationale = %report.rationale,
        legacy_preferred = legacy.preferred_strategies.len(),
        "pick_angle: appended to legacy"
    );
    Ok(Some(report.selected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DiscoveryWiringConfig};
    use crate::execution::Parallelism;
    use crate::fs_layout::MoaganHome;
    use crate::ids::RunId;
    use crate::llm::ProviderRegistry;
    use crate::llm::client::{LlmClient, ScriptedLlmClient, ScriptedLlmResponse};
    use crate::telemetry::Telemetry;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// #928: build a [`ScriptedLlmClient`] (SDK stub) that pops a
    /// scripted response sequence and exposes a `call_count()`
    /// accessor the tests assert against. Mirrors the legacy
    /// `MockProvider` behaviour byte-for-byte (200 OK, `end_turn`,
    /// default usage).
    fn scripted_client(responses: Vec<String>) -> Arc<ScriptedLlmClient> {
        let mut stub = ScriptedLlmClient::empty();
        stub.set_name("mock-persona-angle");
        stub.set_endpoint("mock://persona-angle");
        for body in responses {
            stub.push_ok(ScriptedLlmResponse::accepted(body));
        }
        Arc::new(stub)
    }

    /// #928: wrap a `ScriptedLlmClient` in the
    /// [`LlmClientProvider`] bridge and insert into a fresh
    /// [`ProviderRegistry`] under `"mock"`. `ProviderRegistry::insert`
    /// auto-wraps the bridged provider in a `BreakeredProvider`, so
    /// `RunContext::llm_client()` resolves through the
    /// `BreakeredClient` adapter end-to-end.
    fn scripted_registry(scripted: Arc<ScriptedLlmClient>) -> Arc<ProviderRegistry> {
        let dyn_client: Arc<dyn LlmClient> = scripted as Arc<dyn LlmClient>;
        let bridge: Arc<dyn crate::llm::Provider> = Arc::new(dyn_client);
        let mut registry = ProviderRegistry::default();
        registry.insert("mock".into(), dyn_client);
        Arc::new(registry)
    }

    fn build_ctx(
        home: Arc<MoaganHome>,
        provider_name: &str,
        registry: Arc<ProviderRegistry>,
        config: Arc<Config>,
    ) -> RunContext {
        RunContext::new_with_config(
            RunId::new(),
            home,
            registry,
            provider_name.to_owned(),
            "mock-model".to_owned(),
            Parallelism::new(1),
            Telemetry::noop(),
            String::new(),
            "standard".to_owned(),
            config,
        )
    }

    /// Helper: build a RunContext attached to a tempfile-isolated
    /// `MoaganHome` and the given provider registry. The
    /// `MOAGAN_HOME` env var is *not* touched (Telemetry::noop()
    /// short-circuits the SQLite path).
    fn harness(
        provider: Arc<ScriptedLlmClient>,
        config: Arc<Config>,
    ) -> (TempDir, RunContext, Arc<ScriptedLlmClient>) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Arc::new(MoaganHome::at(tmp.path().to_path_buf()));
        home.ensure().unwrap();
        let registry = scripted_registry(provider.clone());
        let ctx = build_ctx(home, "mock", registry, config);
        (tmp, ctx, provider)
    }

    fn config_with_discovery(discovery: DiscoveryWiringConfig) -> Arc<Config> {
        Arc::new(Config {
            discovery,
            ..Config::default()
        })
    }

    /// `pick_persona` returns `Ok(None)` when the wiring is
    /// disabled. The provider must not be touched.
    #[tokio::test]
    async fn pick_persona_returns_none_when_disabled() {
        let mock = scripted_client(vec![]);
        let cfg = config_with_discovery(DiscoveryWiringConfig {
            persona_enabled: false,
            ..DiscoveryWiringConfig::default()
        });
        let (_tmp, ctx, mock) = harness(mock, cfg);
        let out = pick_persona(&ctx, vec!["skeptic".into(), "optimist".into()])
            .await
            .unwrap();
        assert_eq!(out, None);
        assert_eq!(mock.call_count(), 0);
    }

    /// `pick_angle` returns `Ok(None)` when the wiring is
    /// disabled. The legacy must not be mutated.
    #[tokio::test]
    async fn pick_angle_returns_none_when_disabled() {
        let mock = scripted_client(vec![]);
        let cfg = config_with_discovery(DiscoveryWiringConfig {
            angle_enabled: false,
            ..DiscoveryWiringConfig::default()
        });
        let (_tmp, ctx, mock) = harness(mock, cfg);
        let mut legacy = EpistemicLegacy::empty();
        let out = pick_angle(&ctx, &mut legacy, vec!["jwt".into(), "mtls".into()])
            .await
            .unwrap();
        assert_eq!(out, None);
        assert!(legacy.preferred_strategies.is_empty());
        assert_eq!(mock.call_count(), 0);
    }

    /// `pick_persona` short-circuits to `Ok(None)` on an empty
    /// candidate list — the model contract is explicit that an
    /// empty pool yields `"selected": ""` which is not useful to
    /// surface, so we skip the call entirely.
    #[tokio::test]
    async fn pick_persona_skips_empty_candidate_list() {
        let mock = scripted_client(vec![]);
        let cfg = config_with_discovery(DiscoveryWiringConfig {
            persona_enabled: true,
            ..DiscoveryWiringConfig::default()
        });
        let (_tmp, ctx, mock) = harness(mock, cfg);
        let out = pick_persona(&ctx, vec![]).await.unwrap();
        assert_eq!(out, None);
        assert_eq!(mock.call_count(), 0);
    }

    /// `pick_angle` on a successful round-trip appends the chosen
    /// angle to `EpistemicLegacy::preferred_strategies` using the
    /// canonical `angle:<id>` prefix so downstream consumers can
    /// distinguish angle rows from other strategy rows.
    #[tokio::test]
    async fn pick_angle_persists_to_legacy() {
        let mock = scripted_client(vec![
            r#"{
            "problem": "auth",
            "existing_angles": ["jwt", "mtls"],
            "selected": "oauth2_pkce",
            "rationale": "delegates to a trusted IdP",
            "schema_version": "angle_picker.v1"
        }"#
            .to_owned(),
        ]);
        let cfg = config_with_discovery(DiscoveryWiringConfig {
            angle_enabled: true,
            angle_clusters_min: 1,
            ..DiscoveryWiringConfig::default()
        });
        let (_tmp, ctx, mock) = harness(mock, cfg);
        let mut legacy = EpistemicLegacy::empty();
        let out = pick_angle(&ctx, &mut legacy, vec!["jwt".into(), "mtls".into()])
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("oauth2_pkce"));
        assert_eq!(
            legacy.preferred_strategies,
            vec!["angle:oauth2_pkce".to_owned()]
        );
        assert_eq!(mock.call_count(), 1);
    }
}
