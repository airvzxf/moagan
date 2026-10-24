//! Behaviour of the probe TTL: parsing the env value, freshness of a
//! sidecar row, and the registry skipping probes of fresh rows.

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Config, ModelConfig, ProviderConfig};
use crate::fs_layout::MoaganHome;
use crate::test_support::with_moagan_home;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
}

const DAY: Duration = Duration::from_secs(86_400);

// ---------------------------------------------------------------- parse

#[test]
fn an_unset_or_blank_ttl_is_one_day() {
    assert_eq!(DEFAULT_PROBE_TTL, DAY);
    assert_eq!(parse_ttl(None), DAY);
    assert_eq!(parse_ttl(Some("")), DAY);
    assert_eq!(parse_ttl(Some("   ")), DAY);
}

#[test]
fn a_whole_number_of_seconds_is_the_ttl() {
    assert_eq!(parse_ttl(Some("3600")), Duration::from_secs(3600));
    assert_eq!(parse_ttl(Some(" 60 ")), Duration::from_secs(60));
    assert_eq!(parse_ttl(Some("0")), Duration::ZERO);
}

#[test]
fn anything_else_falls_back_to_the_default() {
    for raw in ["-5", "1.5", "1h", "abc"] {
        assert_eq!(parse_ttl(Some(raw)), DAY, "{raw:?}");
    }
}

// ---------------------------------------------------------------- fresh

#[test]
fn a_row_verified_within_the_ttl_is_fresh() {
    assert!(is_fresh("2026-10-13T11:00:00Z", now(), DAY));
    assert!(is_fresh("2026-10-12T12:00:01Z", now(), DAY));
    assert!(is_fresh("2026-10-13T12:00:00Z", now(), DAY), "age zero");
}

#[test]
fn a_row_as_old_as_the_ttl_or_older_is_stale() {
    assert!(!is_fresh("2026-10-12T12:00:00Z", now(), DAY));
    assert!(!is_fresh("2026-09-12T11:24:30Z", now(), DAY));
}

#[test]
fn other_offsets_are_compared_in_utc() {
    // 13:30 at +02:00 is 11:30 UTC: 30 minutes old.
    assert!(is_fresh(
        "2026-10-13T13:30:00+02:00",
        now(),
        Duration::from_secs(3600)
    ));
    // 11:30 at -02:00 is 13:30 UTC: in the future.
    assert!(!is_fresh("2026-10-13T11:30:00-02:00", now(), DAY));
}

#[test]
fn a_future_or_unreadable_timestamp_is_not_fresh() {
    assert!(!is_fresh("2026-10-13T12:00:01Z", now(), DAY));
    assert!(!is_fresh("", now(), DAY));
    assert!(!is_fresh("yesterday", now(), DAY));
}

#[test]
fn a_zero_ttl_is_never_fresh() {
    assert!(!is_fresh("2026-10-13T12:00:00Z", now(), Duration::ZERO));
}

// ------------------------------------------------------------- registry

/// Upstream accepting every probe with a valid Anthropic-compat body.
async fn accepting_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    server
}

fn minimax_config(endpoint: String) -> Config {
    let mut cfg = Config::default();
    cfg.providers_by_section.insert(
        "minimax".to_owned(),
        ProviderConfig {
            models: vec![ModelConfig {
                id: "MiniMax-M3".to_owned(),
                endpoint: None,
                max_tokens: None,
                omit_max_tokens: false,
            }],
            endpoint: Some(endpoint),
            temperature: None,
            top_p: None,
            omit_max_tokens: false,
            max_token_auto: None,
            max_token_auto_enabled: None,
            max_token_auto_save: true,
            temperature_auto_enabled: None,
            top_p_auto_enabled: None,
            top_k_auto_enabled: None,
            plan: None,
            extra_body: None,
            request_timeout_secs: None,
        },
    );
    cfg
}

/// One invocation: build the registry as `run`/`discover` do and wait
/// for every probe it spawned; returns the upstream's request count.
async fn invoke(cfg: &Config, home: &MoaganHome, server: &MockServer) -> usize {
    let reg = crate::cli::run::build_registry_for_with_active(
        cfg,
        "minimax:MiniMax-M3",
        None,
        None,
        Some(&[("minimax".to_owned(), "MiniMax-M3".to_owned())]),
        Some(home),
    )
    .unwrap();
    if let Some(t) = reg.max_tokens_table() {
        t.await_ready().await;
    }
    if let Some(t) = reg.temperature_table() {
        t.await_ready().await;
    }
    if let Some(t) = reg.top_p_table() {
        t.await_ready().await;
    }
    if let Some(t) = reg.top_k_table() {
        t.await_ready().await;
    }
    server.received_requests().await.unwrap().len()
}

/// Rewrite every `verified_at` of the four sidecars to a year-old date.
fn age_sidecars(home: &MoaganHome) {
    for path in [
        home.max_tokens_auto_path(),
        home.temperatures_auto_path(),
        home.top_p_auto_path(),
        home.top_k_auto_path(),
    ] {
        let body = std::fs::read_to_string(&path).unwrap();
        let aged: String = body
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("verified_at") {
                    "verified_at = \"2025-10-13T00:00:00Z\"".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, aged + "\n").unwrap();
    }
}

/// Sets `MINIMAX_API_KEY` for the test and restores it on drop.
struct KeyEnv(Option<String>);

impl KeyEnv {
    fn set(value: &str) -> Self {
        let previous = std::env::var("MINIMAX_API_KEY").ok();
        // SAFETY: serialised by `TEST_API_KEYS_LOCK`, held by the caller.
        unsafe { std::env::set_var("MINIMAX_API_KEY", value) };
        Self(previous)
    }
}

impl Drop for KeyEnv {
    fn drop(&mut self) {
        // SAFETY: serialised by `TEST_API_KEYS_LOCK`, held by the caller.
        unsafe {
            match self.0.take() {
                Some(v) => std::env::set_var("MINIMAX_API_KEY", v),
                None => std::env::remove_var("MINIMAX_API_KEY"),
            }
        }
    }
}

#[test]
fn a_second_invocation_within_the_ttl_makes_no_probe_request() {
    let _keys = crate::TEST_API_KEYS_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _key = KeyEnv::set("probe-test-key");
    with_moagan_home("probe-ttl", |tmp| {
        let home = MoaganHome::at(tmp.to_path_buf());
        home.ensure().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = accepting_upstream().await;
            let cfg = minimax_config(format!("{}/anthropic/v1/messages", server.uri()));

            let first = invoke(&cfg, &home, &server).await;
            assert!(first > 0, "the first invocation probes");
            for path in [
                home.max_tokens_auto_path(),
                home.temperatures_auto_path(),
                home.top_p_auto_path(),
                home.top_k_auto_path(),
            ] {
                assert!(path.is_file(), "{} written", path.display());
            }

            let second = invoke(&cfg, &home, &server).await;
            assert_eq!(second, first, "no probe request within the TTL");

            age_sidecars(&home);
            let third = invoke(&cfg, &home, &server).await;
            assert!(third > second, "a stale row is verified again");
        });
    });
}
