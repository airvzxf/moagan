//! Integration tests for the dual-mode config loader: verify that
//! legacy `[providers.X]` TOML still loads and propagates the
//! operator-set per-model `max_tokens`, AND that the new
//! `[[providers.X]]` array-of-tables form loads cleanly.

#[cfg(test)]
mod tests {
    use crate::config::Config;

    #[test]
    fn legacy_toml_round_trip_via_config_load() {
        // Build a minimal legacy TOML with the v0.12 single-table
        // shape plus a per-model `max_tokens = 131072`. Confirm
        // `toml::from_str` deserialises it, then the bridge populates
        // `providers_legacy["minimax"].models[0].max_tokens` with the
        // operator-set value (the side-channel preserves it for
        // backwards compat until PR #4 lands `resolve_max_tokens`).
        let legacy_toml = r#"
[providers.minimax]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
temperature = 0.42

[[providers.minimax.models]]
id = "MiniMax-M3"
max_tokens = 131072
"#;
        let mut cfg: Config = toml::from_str(legacy_toml).expect("legacy TOML parses");
        cfg.compute_legacy_providers().expect("bridge succeeds");
        let minimax = cfg
            .providers_legacy
            .get("minimax")
            .expect("minimax section present after bridge");
        assert_eq!(minimax.models.len(), 1);
        assert_eq!(minimax.models[0].id, "MiniMax-M3");
        assert_eq!(
            minimax.models[0].max_tokens,
            Some(131_072),
            "legacy per-model max_tokens must propagate through the bridge"
        );
        assert_eq!(minimax.temperature, Some(0.42));
    }

    #[test]
    fn new_toml_round_trip_via_config_load() {
        let new_toml = r#"
[[providers.minimax]]
endpoint = "https://api.minimax.io/anthropic/v1/messages"
models = ["MiniMax-M3", "MiniMax-M2.5"]
temperature = 0.42
"#;
        let mut cfg: Config = toml::from_str(new_toml).expect("new TOML parses");
        cfg.compute_legacy_providers().expect("bridge succeeds");
        let minimax = cfg
            .providers_legacy
            .get("minimax")
            .expect("minimax section present after bridge");
        assert_eq!(minimax.models.len(), 2);
        assert_eq!(minimax.models[0].id, "MiniMax-M3");
        assert_eq!(minimax.models[0].max_tokens, None);
        assert_eq!(minimax.temperature, Some(0.42));
    }

    #[test]
    fn deserialize_providers_map_handles_empty_section() {
        // An empty `providers = {}` is well-formed (the dual-mode
        // deserializer sees an empty outer table and produces no
        // entries). Bridge the empty config and confirm
        // `providers_legacy` is also empty.
        let mut cfg: Config = toml::from_str("providers = {}").expect("empty providers map parses");
        cfg.compute_legacy_providers().expect("bridge succeeds");
        assert!(cfg.providers_legacy.is_empty());
    }
}
