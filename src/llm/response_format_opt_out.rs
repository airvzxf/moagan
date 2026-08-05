//! Per-model opt-out for `response_format: json_object`.
//!
//! Some models in the OpenCode Go and OpenAI-compat roster ignore
//! `response_format` (prose-prefixed or empty content); providers
//! that route to those models must omit the field from the request
//! body so the heuristic parser has a chance to recover the JSON.

const DEFAULT_OPT_OUT: &[&str] = &[
    "glm-5.1",
    "glm-5.2",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "deepseek-v4-pro",
    "kimi-k3",
];

/// Returns `true` when the configured model is in the
/// [`DEFAULT_OPT_OUT`] list or in the runtime
/// `MOAGAN_RESPONSE_FORMAT_OPT_OUT` env var (comma-separated).
/// Providers that route to opted-out models must omit
/// `response_format: json_object` from the request body.
pub fn model_skips_response_format(model: &str) -> bool {
    if let Ok(extra) = std::env::var("MOAGAN_RESPONSE_FORMAT_OPT_OUT") {
        for m in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if model.eq_ignore_ascii_case(m) {
                return true;
            }
        }
    }
    DEFAULT_OPT_OUT
        .iter()
        .any(|m| model.eq_ignore_ascii_case(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_lock<F: FnOnce()>(f: F) {
        drop(ENV_LOCK.lock());
        f();
    }

    #[test]
    fn recognizes_glm_5_1() {
        assert!(model_skips_response_format("glm-5.1"));
    }

    #[test]
    fn recognizes_glm_5_2() {
        assert!(model_skips_response_format("glm-5.2"));
    }

    #[test]
    fn recognizes_kimi_k2_6() {
        assert!(model_skips_response_format("kimi-k2.6"));
    }

    #[test]
    fn recognizes_kimi_k2_7_code() {
        assert!(model_skips_response_format("kimi-k2.7-code"));
    }

    #[test]
    fn recognizes_deepseek_v4_pro() {
        assert!(model_skips_response_format("deepseek-v4-pro"));
    }

    #[test]
    fn recognizes_kimi_k3() {
        assert!(model_skips_response_format("kimi-k3"));
    }

    #[test]
    fn case_insensitive() {
        assert!(model_skips_response_format("GLM-5.1"));
        assert!(model_skips_response_format("Kimi-K3"));
        assert!(model_skips_response_format("DeepSeek-V4-Pro"));
    }

    #[test]
    fn allows_normal_models() {
        assert!(!model_skips_response_format("gpt-4o-mini"));
        assert!(!model_skips_response_format("minimax-haiku"));
        assert!(!model_skips_response_format("deepseek-v4-flash"));
        assert!(!model_skips_response_format("qwen3.7-max"));
    }

    #[test]
    fn env_var_extends_opt_out() {
        with_lock(|| unsafe {
            std::env::set_var(
                "MOAGAN_RESPONSE_FORMAT_OPT_OUT",
                "my-custom-model,other-model",
            );
        });
        assert!(model_skips_response_format("my-custom-model"));
        assert!(model_skips_response_format("OTHER-MODEL"));
        assert!(!model_skips_response_format("not-in-the-list"));
        with_lock(|| unsafe {
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
        });
    }

    #[test]
    fn env_var_empty_is_ignored() {
        with_lock(|| unsafe {
            std::env::set_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT", "");
        });
        assert!(!model_skips_response_format("gpt-4"));
        assert!(!model_skips_response_format("glm-5.1-not-the-default"));
        with_lock(|| unsafe {
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
        });
    }

    #[test]
    fn env_var_whitespace_is_trimmed() {
        with_lock(|| unsafe {
            std::env::set_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT", " spaced-model , another ");
        });
        assert!(model_skips_response_format("spaced-model"));
        assert!(model_skips_response_format("another"));
        with_lock(|| unsafe {
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
        });
    }
}
