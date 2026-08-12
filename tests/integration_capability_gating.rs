//! Integration tests for the capability resolver + temperature gate.
//!
//! Pins the contract that `CapabilityResolver::gate_request` drops
//! `temperature` from the wire body when the upstream catalog says
//! the model does not honour it (e.g. `kimi-k3`), and keeps it
//! when the upstream does (e.g. `minimax/MiniMax-M3`).
//!
//! The wire body is built via [`WireFormat::encode_value`] from
//! `moagan::llm::wire_format` so the test exercises the same path
//! the production providers take on every call. The
//! `MessagesRequestBody` uses `#[serde(skip_serializing_if =
//! "Option::is_none")]` on `temperature`, so an empty field is the
//! signal that the gate ran; the assertions below read both the
//! presence AND the byte-level shape of the body to catch future
//! regressions where the gate silently regresses.

use std::collections::BTreeMap;
use std::sync::Arc;

use moagan::llm::capability::{CapabilityResolver, ResolvedConfig};
use moagan::llm::models_dev::{
    CATALOG_SCHEMA_VERSION, Cost, Limits, Modalities, ModelsDevCatalog, ModelsDevEntry,
    ModelsDevProvider,
};
use moagan::llm::role::Role;
use moagan::llm::wire::Request;
use moagan::llm::wire_format::{AnthropicWire, WireFormat};

/// Build a `Request` with `temperature` set so the test can confirm
/// the gate either drops or preserves the field verbatim.
fn sample_request(model: &str) -> Request {
    Request {
        role: Role::Intake,
        model: model.to_owned(),
        system: "sys".to_owned(),
        user: "user".to_owned(),
        max_tokens: 1024,
        temperature: Some(0.6),
        top_p: Some(0.95),
        response_schema: None,
        stream: false,
        extra_messages: vec![],
    }
}

/// Build a minimal catalog with one row that says
/// `temperature: false` (the canonical PR-3 example: `kimi-k3`).
fn catalog_with_kimi_k3() -> Arc<ModelsDevCatalog> {
    let mut models = BTreeMap::new();
    models.insert(
        "kimi-k3".to_owned(),
        ModelsDevEntry {
            id: "kimi-k3".to_owned(),
            name: "Kimi k3".to_owned(),
            family: Some("kimi".to_owned()),
            attachment: false,
            reasoning: true,
            reasoning_options: vec![],
            tool_call: true,
            temperature: false,
            interleaved: None,
            modalities: Modalities {
                input: vec!["text".to_owned()],
                output: vec!["text".to_owned()],
            },
            limit: Limits {
                context: 131_072,
                output: 8_192,
            },
            cost: Cost {
                input: 0.6,
                output: 2.4,
                cache_read: 0.06,
                cache_write: 0.75,
            },
            open_weights: false,
            release_date: None,
            last_updated: None,
        },
    );
    let mut providers = BTreeMap::new();
    providers.insert(
        "kimi".to_owned(),
        ModelsDevProvider {
            id: "kimi".to_owned(),
            name: "Kimi".to_owned(),
            api: None,
            doc: None,
            models,
        },
    );
    Arc::new(ModelsDevCatalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        fetched_at_unix: 1_700_000_000,
        providers,
    })
}

/// Build a minimal catalog with `minimax/MiniMax-M3` (the canonical
/// PR-3 control case: `temperature: true`).
fn catalog_with_minimax() -> Arc<ModelsDevCatalog> {
    let mut models = BTreeMap::new();
    models.insert(
        "MiniMax-M3".to_owned(),
        ModelsDevEntry {
            id: "MiniMax-M3".to_owned(),
            name: "MiniMax-M3".to_owned(),
            family: Some("minimax".to_owned()),
            attachment: false,
            reasoning: true,
            reasoning_options: vec![],
            tool_call: true,
            temperature: true,
            interleaved: None,
            modalities: Modalities {
                input: vec!["text".to_owned()],
                output: vec!["text".to_owned()],
            },
            limit: Limits {
                context: 524_288,
                output: 128_000,
            },
            cost: Cost {
                input: 0.3,
                output: 1.2,
                cache_read: 0.03,
                cache_write: 0.375,
            },
            open_weights: true,
            release_date: None,
            last_updated: None,
        },
    );
    let mut providers = BTreeMap::new();
    providers.insert(
        "minimax".to_owned(),
        ModelsDevProvider {
            id: "minimax".to_owned(),
            name: "minimax".to_owned(),
            api: None,
            doc: None,
            models,
        },
    );
    Arc::new(ModelsDevCatalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        fetched_at_unix: 1_700_000_000,
        providers,
    })
}

/// When the catalog says `temperature: false` (`kimi-k3`), the
/// gate drops the field and the wire body carries no `temperature`
/// key at all. This is the PR-3 happy-path assertion that the
/// upstream's documented limitation actually changes the wire.
#[test]
fn capability_gate_drops_temperature_for_kimi_k3() {
    let resolver = CapabilityResolver::new(Some(catalog_with_kimi_k3()));
    let req = sample_request("kimi-k3");
    let gated = resolver.gate_request("kimi", "kimi-k3", &req);
    assert!(
        gated.temperature.is_none(),
        "resolver must clear temperature when catalog says drop it"
    );

    let body = AnthropicWire
        .encode_value(&gated)
        .expect("encode_value must succeed for a valid Request");
    assert!(
        body.get("temperature").is_none(),
        "wire body must NOT carry temperature; got: {body}"
    );
    // Sanity: the rest of the wire shape stays untouched so a future
    // change to `body_from_request` cannot regress into silently
    // dropping other fields alongside the gate.
    assert_eq!(body["model"], "kimi-k3");
    assert_eq!(body["system"], "sys");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["top_p"], 0.95);
}

/// Control case: when the catalog says `temperature: true`
/// (`minimax/MiniMax-M3`), the gate is a no-op and the wire body
/// carries the field verbatim. This pins that PR-3 does not break
/// the steady-state behaviour for the providers the pipeline
/// already calls.
#[test]
fn capability_gate_keeps_temperature_for_minimax() {
    let resolver = CapabilityResolver::new(Some(catalog_with_minimax()));
    let req = sample_request("MiniMax-M3");
    let gated = resolver.gate_request("minimax", "MiniMax-M3", &req);
    assert_eq!(gated.temperature, Some(0.6));

    let body = AnthropicWire
        .encode_value(&gated)
        .expect("encode_value must succeed for a valid Request");
    assert_eq!(
        body["temperature"], 0.6,
        "wire body must carry temperature=0.6 verbatim; got: {body}"
    );
}

/// Config override that flips `temperature` back to true must
/// override the catalog row. The wire body MUST carry the field
/// even though the catalog says drop it. Pins the precedence
/// "config > catalog > hardcoded" documented in
/// `src/llm/capability.rs`.
#[test]
fn capability_config_override_wins_over_catalog() {
    let mut cfg = ResolvedConfig::default();
    cfg.override_pair("kimi", "kimi-k3", |mut c| {
        c.temperature = true;
        c
    });
    let resolver = CapabilityResolver::new(Some(catalog_with_kimi_k3())).with_config(cfg);
    let req = sample_request("kimi-k3");
    let gated = resolver.gate_request("kimi", "kimi-k3", &req);
    assert_eq!(
        gated.temperature,
        Some(0.6),
        "config override must keep the field even when the catalog says drop it"
    );

    let body = AnthropicWire.encode_value(&gated).expect("encode_value");
    assert_eq!(
        body["temperature"], 0.6,
        "wire body must carry temperature when the operator pinned it via config"
    );
}
