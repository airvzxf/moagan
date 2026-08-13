//! Per-model opt-out for `response_format: json_object`.
//!
//! Some models in the OpenCode Go and OpenAI-compat roster ignore
//! `response_format` (prose-prefixed or empty content); providers
//! that route to those models must omit the field from the request
//! body so the heuristic parser has a chance to recover the JSON.
//!
//! For the same models the JSON contract rides entirely on the
//! system prompt. [`STUBBORN_MODEL_JSON_PREFIX`] is the explicit
//! "CRITICAL OUTPUT CONTRACT" header prepended to the role's
//! normal prompt when the active model is in the opt-out list —
//! see [`render_system_prompt_with_prefix`].

use super::role::Role;

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

/// Stubborn-model "CRITICAL OUTPUT CONTRACT" prefix. Prepended to
/// the role's normal system prompt when the active model is in the
/// opt-out list, so the JSON contract rides on a strong top-of-
/// prompt reinforcement instead of the brief "Return a JSON object
/// (no prose, no markdown):" one-liner that those models ignore.
///
/// The text spells out the wire contract byte-by-byte (first
/// non-whitespace character MUST be `{`, last MUST be `}`, double-
/// quoted ASCII keys, no markdown fences / `<think>` blocks /
/// greetings, escape rules for inner quotes and newlines) so even
/// models that ignore `response_format: json_object` will produce
/// a tolerant-extractor-friendly reply.
///
/// The prefix is intentionally generic — it does NOT bake in the
/// role's per-field schema. The role's own system prompt, which
/// follows, supplies the schema. Adding this prefix does not
/// change the contract of any existing function; it only adds a
/// reinforcement layer on top.
pub const STUBBORN_MODEL_JSON_PREFIX: &str = "\
CRITICAL OUTPUT CONTRACT
=======================
You MUST reply with a single JSON object and NOTHING else.
- The first non-whitespace character of your reply MUST be `{`.
- The last non-whitespace character of your reply MUST be `}`.
- Inside the object, every key MUST be a double-quoted ASCII string.
- No markdown fences, no commentary, no `<think>` blocks, no greetings.
- If a key's value is a string, escape inner quotes with `\\\"` and inner newlines with `\\n`.
Reply now with the JSON object only.
=======================
";

/// Returns `true` when [`render_system_prompt_with_prefix`] should
/// prepend [`STUBBORN_MODEL_JSON_PREFIX`] to the role's base system
/// prompt. Mirrors [`model_skips_response_format`] so the same set
/// of models that ignore `response_format: json_object` also get
/// the prompt reinforcement.
fn is_stubborn_model(model: &str) -> bool {
    model_skips_response_format(model)
}

/// Render the final system prompt for a role on a given model.
///
/// - When `model` is in the opt-out list (see `is_stubborn_model`)
///   the [`STUBBORN_MODEL_JSON_PREFIX`] is prepended, followed by a
///   blank line, followed by the role's `base_prompt`.
/// - Otherwise the `base_prompt` is returned byte-for-byte. The
///   "normal" branch deliberately allocates a fresh `String` so
///   callers can swap the rendered value into the `Request.system`
///   field without worrying about whether the role slice was
///   borrowed from a prompt file.
///
/// The `role` parameter is reserved for future per-role
/// adjustments (e.g. suppressing the prefix on prose-only roles)
/// and is currently unused. Keeping it in the signature lets
/// callers splice the helper in at the request-construction site
/// without further refactoring later.
pub fn render_system_prompt_with_prefix(_role: &Role, model: &str, base_prompt: &str) -> String {
    if is_stubborn_model(model) {
        format!("{}\n\n{}", STUBBORN_MODEL_JSON_PREFIX, base_prompt)
    } else {
        base_prompt.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_lock<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
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
            assert!(model_skips_response_format("my-custom-model"));
            assert!(model_skips_response_format("OTHER-MODEL"));
            assert!(!model_skips_response_format("not-in-the-list"));
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
        });
    }

    #[test]
    fn env_var_empty_is_ignored() {
        with_lock(|| unsafe {
            std::env::set_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT", "");
            assert!(!model_skips_response_format("gpt-4"));
            assert!(!model_skips_response_format("glm-5.1-not-the-default"));
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
        });
    }

    #[test]
    fn env_var_whitespace_is_trimmed() {
        with_lock(|| unsafe {
            std::env::set_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT", " spaced-model , another ");
            assert!(model_skips_response_format("spaced-model"));
            assert!(model_skips_response_format("another"));
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
        });
    }

    /// Track C (PR-C6): the prefix injection helper must agree with
    /// the existing `model_skips_response_format` predicate, i.e.
    /// every static entry of [`DEFAULT_OPT_OUT`] is a "stubborn"
    /// model that needs the JSON-only prefix.
    #[test]
    fn is_stubborn_model_true_for_each_static_opted_out() {
        for m in DEFAULT_OPT_OUT {
            assert!(
                is_stubborn_model(m),
                "expected `{m}` to be flagged as a stubborn model"
            );
        }
    }

    /// Track C (PR-C6): a model that honours `response_format`
    /// does NOT need the prompt prefix. The four assertions cover
    /// the four sample providers called out in the task description.
    #[test]
    fn is_stubborn_model_false_for_normal_model() {
        assert!(!is_stubborn_model("minimax-m3"));
        assert!(!is_stubborn_model("deepseek-v4-flash"));
        assert!(!is_stubborn_model("mimo-v2.5"));
        assert!(!is_stubborn_model("gpt-5.6-luna"));
    }

    /// Track C (PR-C6): the runtime env var
    /// `MOAGAN_RESPONSE_FORMAT_OPT_OUT` must extend the static list
    /// for the prefix helper, exactly the way it does for
    /// `model_skips_response_format`. Same lock convention as the
    /// existing env-var tests above.
    #[test]
    fn is_stubborn_model_respects_env_extension() {
        with_lock(|| unsafe {
            std::env::set_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT", "foo-bar");
            assert!(is_stubborn_model("foo-bar"));
            std::env::remove_var("MOAGAN_RESPONSE_FORMAT_OPT_OUT");
            assert!(!is_stubborn_model("foo-bar"));
        });
    }

    /// Track C (PR-C6): for a stubborn model the rendered system
    /// prompt starts with the `CRITICAL OUTPUT CONTRACT` header
    /// and contains the role's base prompt verbatim. The base
    /// prompt is placed AFTER the prefix so the per-role schema
    /// still travels with the call.
    #[test]
    fn render_system_prompt_with_prefix_prepends_for_stubborn() {
        let role = Role::Intake;
        let base = "Return a JSON object (no prose, no markdown).";
        let rendered = render_system_prompt_with_prefix(&role, "glm-5.1", base);
        assert!(
            rendered.starts_with(STUBBORN_MODEL_JSON_PREFIX),
            "stubborn-model render must start with the prefix"
        );
        assert!(
            rendered.contains(base),
            "stubborn-model render must still contain the role's base prompt"
        );
        assert!(
            rendered.ends_with(base),
            "stubborn-model render must place the base prompt at the tail"
        );
    }

    /// Track C (PR-C6): for a non-stubborn model the helper must
    /// return the base prompt unchanged. The role's prose-only
    /// roles (e.g. `PersonaPicker`) and the JSON-emitting roles
    /// share the same code path; both must pass through byte-for-
    /// byte so the other ~15 providers see no behaviour change.
    #[test]
    fn render_system_prompt_with_prefix_passthrough_for_normal() {
        let role = Role::Propose;
        let base = "Return a JSON object (no prose, no markdown).";
        let rendered = render_system_prompt_with_prefix(&role, "minimax-m3", base);
        assert_eq!(rendered, base);
    }
}
