//! Per-model JSON recovery strategy.
//!
//! Different models in the OpenCode Go roster (and the chat-completions
//! fall-back to OpenAI-compat providers) emit parseable JSON through
//! different recovery paths. This module encodes the per-model default
//! as a typed enum so the parse chain
//! ([`crate::llm::json_extractor`]) and the dispatcher retry loop
//! ([`crate::phases::phase::RunContext::call_with_retry_parse`]) can
//! branch on the model without hard-coding a model-name → behaviour
//! map in either place.
//!
//! # Why a typed enum?
//!
//! A previous spike wired the per-model table as a `match model`
//! inside the parse chain. That produced three problems the typed
//! enum resolves cleanly:
//!
//! 1. Every new model that joined the stubborn-MiniMax-M3 cohort
//!    required touching both the chain and the dispatcher. The
//!    enum centralises the policy in one table
//!    ([`STRATEGY_BY_MODEL`]) and lets the chain / dispatcher
//!    dispatch on the variant.
//! 2. The "no strategy" call site could not be distinguished from
//!    the "Lenient" call site without a sentinel value. The
//!    [`Default`] impl on the enum gives the call site a single
//!    place to ask "what do I do if I don't know?".
//! 3. The PR-C2 continuation re-call
//!    ([`crate::llm::role::Role::Continuation`]) and the new
//!    `PromptPrefill` retry are both "on parse failure, try
//!    something on the request side". The enum lets `phase.rs`
//!    compute the retry budget via
//!    [`max_continuation_attempts`] / [`needs_assistant_prefill`]
//!    instead of branching on model names.
//!
//! # What each variant does
//!
//! - [`Strict`](JsonRecoveryStrategy::Strict) — only the direct
//!   `serde_json::from_str` runs. If it fails, the chain returns the
//!   parse error verbatim; no tolerant extraction, no m3 repair, no
//!   continuation, no prefill. Used for models that already honour
//!   `response_format: json_object` strictly (e.g. `gpt-5.6-luna`).
//! - [`Lenient`](JsonRecoveryStrategy::Lenient) — the full recovery
//!   chain: control-token strip, tolerant extraction (PR-C3
//!   iterative brackets + PR-C4 chat-template strip), m3 repair.
//!   Used as the default for chat-completions providers that return
//!   prose-prefixed / bracket-broken JSON (kimi-*, glm-*, hy3,
//!   mimo-*).
//! - [`Continuation`](JsonRecoveryStrategy::Continuation) — same as
//!   Lenient for the single parse attempt. On **truncated** response
//!   (`finish_reason="length"` / `"max_tokens"`), the dispatcher
//!   re-issues the call as
//!   [`Role::Continuation`](crate::llm::role::Role::Continuation)
//!   up to [`max_continuation_attempts`] times (2 in production).
//!   On parse failure of a **non-truncated** response, the dispatcher
//!   falls through to the normal parse-failure retry budget (5
//!   attempts for Parse/Schema per the per-mode matrix in
//!   `retry_budget.rs`). Used for models that occasionally truncate
//!   (`minimax-*`).
//! - [`PromptPrefill`](JsonRecoveryStrategy::PromptPrefill) —
//!   same as Lenient for the first attempt; on parse failure the
//!   dispatcher retries ONCE with an assistant prefill of `{`
//!   appended to the request body. The prefill is a response-side
//!   hint; [`needs_assistant_prefill`] returns `true` so the
//!   OpenAI-compat body builder knows to push the prefill message.
//!   Used for stubborn models that ignore `response_format`
//!   (`deepseek-v4-pro`, `deepseek-v4-flash`).
//!
//! # Boundary with [`response_format_opt_out`](crate::llm::response_format_opt_out)
//!
//! The opt-out list is a *request-side* decision (omit
//! `response_format: json_object` from the wire body). The strategy
//! enum is a *parse-side* decision (what to do with a malformed
//! response). They overlap in the model list but operate on
//! independent mechanisms; this module does not consult the
//! opt-out list and [`response_format_opt_out`] does not consult
//! the strategy table.

use std::collections::HashMap;

/// The four recovery strategies the dispatcher / chain understands.
/// The variants are ordered most-permissive-first to last; the
/// `Default` impl returns [`Lenient`](Self::Lenient) so unrecognised
/// models get the safest fallback (most-extraction, no retries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum JsonRecoveryStrategy {
    /// Direct parse only. If the response is not valid JSON, the
    /// chain returns the parse error verbatim. No tolerant
    /// extraction, no m3 repair, no continuation, no prefill.
    /// Use for models that already honour `response_format:
    /// json_object` strictly (e.g. `gpt-5.6-luna` via the
    /// Responses API).
    Strict,
    /// Full recovery chain (control-token strip + tolerant
    /// extraction + m3 repair). On parse failure the dispatcher
    /// does NOT retry — this is the steady-state default for the
    /// chat-completions providers that return prose-prefixed or
    /// bracket-broken JSON (kimi-*, glm-*, hy3, mimo-*).
    #[default]
    Lenient,
    /// Lenient plus bounded focused continuation on TRUNCATED responses.
    /// On a truncated response, the dispatcher re-calls the model as
    /// [`Role::Continuation`](crate::llm::role::Role::Continuation)
    /// up to [`max_continuation_attempts`] (2 in production) times. On
    /// parse failure of a non-truncated response, the dispatcher falls
    /// through to the normal parse-failure retry budget. Use for models
    /// that occasionally truncate (`minimax-*`).
    Continuation,
    /// Lenient with an assistant prefill of `{` injected at the
    /// body-builder level. On first parse failure the dispatcher
    /// retries ONCE with the prefill; if that retry still fails to
    /// parse, the chain falls back to Lenient's normal error.
    /// Use for stubborn models that ignore `response_format`
    /// (`deepseek-v4-pro`, `deepseek-v4-flash`).
    PromptPrefill,
}

/// Per-model default strategy. The order matters: `strategy_for`
/// walks the slice top-to-bottom and returns the first match
/// (case-insensitive), so a more specific name must come before a
/// more general one (e.g. `kimi-k2.7-code` before `kimi` would be
/// required if `kimi` existed as a prefix entry — currently we do
/// not have any prefix entries, but the ordering invariant is
/// documented here so future edits keep it intact).
///
/// Models not on this list fall through to
/// [`DEFAULT_STRATEGY`].
pub const STRATEGY_BY_MODEL: &[(&str, JsonRecoveryStrategy)] = &[
    // kimi — chat-template leaks plus Llama-style bracket
    // variants. Lenient handles both; no retry needed.
    ("kimi-k2.7-code", JsonRecoveryStrategy::Lenient),
    ("kimi-k3", JsonRecoveryStrategy::Lenient),
    ("kimi-k2.6", JsonRecoveryStrategy::Lenient),
    // glm — OpenCode Go chat-completions; Lenient.
    ("glm-5.1", JsonRecoveryStrategy::Lenient),
    ("glm-5.2", JsonRecoveryStrategy::Lenient),
    // hy3 — OpenCode Go; Lenient.
    ("hy3", JsonRecoveryStrategy::Lenient),
    // mimo — chat-completions; Lenient.
    ("mimo-v2.5", JsonRecoveryStrategy::Lenient),
    ("mimo-v2.5-pro", JsonRecoveryStrategy::Lenient),
    // deepseek — stubborn, ignores `response_format`. Prefill
    // nudges the model into starting with `{` so the parse chain
    // sees a balanced fragment on the retry.
    ("deepseek-v4-pro", JsonRecoveryStrategy::PromptPrefill),
    ("deepseek-v4-flash", JsonRecoveryStrategy::PromptPrefill),
    // minimax — Anthropic-compat. Rarely truncates; the
    // Continuation retry is the safety net.
    ("minimax-m3", JsonRecoveryStrategy::Continuation),
    ("minimax-m2.7", JsonRecoveryStrategy::Continuation),
    ("minimax-m2.5", JsonRecoveryStrategy::Continuation),
    // gpt-5.6-luna — Responses API honours
    // `response_format: json_object` strictly. Strict skips every
    // recovery pass and surfaces the raw parse error so the
    // post-execution review can see when the model genuinely
    // produces malformed JSON.
    ("gpt-5.6-luna", JsonRecoveryStrategy::Strict),
];

/// Catch-all for unrecognised models. `Lenient` is the safest
/// fallback — it does not retry, but it does run every
/// parse-side recovery pass. A model that already honours
/// `response_format` will pass the direct parse on the first
/// attempt and the Lenient extra passes become no-ops.
pub const DEFAULT_STRATEGY: JsonRecoveryStrategy = JsonRecoveryStrategy::Lenient;

/// Resolve the recovery strategy for `model`.
///
/// Lookup order:
///
/// 1. If `profile_overrides` is `Some` and contains `model`
///    (case-sensitive), return the override. This is the
///    runtime-extensibility hook — future PRs wire a CLI flag
///    that lets operators pin a specific strategy without
///    shipping a new binary.
/// 2. Otherwise walk [`STRATEGY_BY_MODEL`] top-to-bottom and
///    return the first case-insensitive match.
/// 3. If no entry matches, return [`DEFAULT_STRATEGY`].
///
/// The first-match-walk in step 2 is intentionally case-insensitive
/// so an operator who wrote `provider.model = "Kimi-K3"` in their
/// TOML still gets the right strategy — the model's wire identifier
/// (`kimi-k3`) is the canonical form, but a casing slip on the
/// config side should not produce a silent fallback.
pub fn strategy_for(
    model: &str,
    profile_overrides: Option<&HashMap<String, JsonRecoveryStrategy>>,
) -> JsonRecoveryStrategy {
    if let Some(overrides) = profile_overrides
        && let Some(s) = overrides.get(model)
    {
        return *s;
    }
    for (k, v) in STRATEGY_BY_MODEL {
        if model.eq_ignore_ascii_case(k) {
            return *v;
        }
    }
    DEFAULT_STRATEGY
}

/// Returns `true` when the dispatcher should inject an assistant
/// prefill message of `{` at the body-builder level
/// ([`crate::llm::openai_compat::OpenAiCompatProvider::build_chat_request`]).
///
/// Currently only [`PromptPrefill`](JsonRecoveryStrategy::PromptPrefill)
/// requires the prefill. The helper exists so the body builder
/// does not have to import the enum.
pub fn needs_assistant_prefill(s: JsonRecoveryStrategy) -> bool {
    matches!(s, JsonRecoveryStrategy::PromptPrefill)
}

/// Maximum number of continuation re-calls the dispatcher should
/// issue for `s`. `Strict` and `Lenient` both return `0` because
/// neither triggers a continuation retry. `Continuation` returns
/// `2`, matching the existing
/// [`MAX_CONTINUATIONS`](crate::phases::phase::RunContext::call_with_retry_parse)
/// constant that PR-C2 introduced. `PromptPrefill` returns `0`
/// because its retry is the prefill, not the continuation.
///
/// This is the single source of truth for the per-strategy retry
/// budget. Callers that used to hard-code `MAX_CONTINUATIONS = 2`
/// inside the retry loop should now consult this helper so a
/// future bump (e.g. `Continuation = 3`) flows through every call
/// site at once.
pub fn max_continuation_attempts(s: JsonRecoveryStrategy) -> u8 {
    match s {
        JsonRecoveryStrategy::Strict => 0,
        JsonRecoveryStrategy::Lenient => 0,
        JsonRecoveryStrategy::Continuation => 2,
        JsonRecoveryStrategy::PromptPrefill => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Default ------------------------------------------------------

    #[test]
    fn default_is_lenient() {
        assert_eq!(
            JsonRecoveryStrategy::default(),
            JsonRecoveryStrategy::Lenient
        );
    }

    #[test]
    fn default_strategy_constant_is_lenient() {
        assert_eq!(DEFAULT_STRATEGY, JsonRecoveryStrategy::Lenient);
    }

    // --- strategy_for: per-model table --------------------------------

    #[test]
    fn strategy_for_kimi_models_is_lenient() {
        for model in ["kimi-k2.7-code", "kimi-k3", "kimi-k2.6"] {
            assert_eq!(
                strategy_for(model, None),
                JsonRecoveryStrategy::Lenient,
                "kimi model {model} should resolve to Lenient"
            );
        }
    }

    #[test]
    fn strategy_for_glm_models_is_lenient() {
        for model in ["glm-5.1", "glm-5.2"] {
            assert_eq!(
                strategy_for(model, None),
                JsonRecoveryStrategy::Lenient,
                "glm model {model} should resolve to Lenient"
            );
        }
    }

    #[test]
    fn strategy_for_hy3_is_lenient() {
        assert_eq!(strategy_for("hy3", None), JsonRecoveryStrategy::Lenient);
    }

    #[test]
    fn strategy_for_mimo_models_is_lenient() {
        for model in ["mimo-v2.5", "mimo-v2.5-pro"] {
            assert_eq!(
                strategy_for(model, None),
                JsonRecoveryStrategy::Lenient,
                "mimo model {model} should resolve to Lenient"
            );
        }
    }

    #[test]
    fn strategy_for_deepseek_models_is_prompt_prefill() {
        for model in ["deepseek-v4-pro", "deepseek-v4-flash"] {
            assert_eq!(
                strategy_for(model, None),
                JsonRecoveryStrategy::PromptPrefill,
                "deepseek model {model} should resolve to PromptPrefill"
            );
        }
    }

    #[test]
    fn strategy_for_minimax_models_is_continuation() {
        for model in ["minimax-m3", "minimax-m2.7", "minimax-m2.5"] {
            assert_eq!(
                strategy_for(model, None),
                JsonRecoveryStrategy::Continuation,
                "minimax model {model} should resolve to Continuation"
            );
        }
    }

    #[test]
    fn strategy_for_gpt_5_6_luna_is_strict() {
        assert_eq!(
            strategy_for("gpt-5.6-luna", None),
            JsonRecoveryStrategy::Strict
        );
    }

    // --- strategy_for: lookup semantics -------------------------------

    #[test]
    fn strategy_for_unknown_model_returns_default() {
        // Unknown model with no overrides: falls through to DEFAULT_STRATEGY.
        assert_eq!(
            strategy_for("gpt-7-future", None),
            JsonRecoveryStrategy::Lenient
        );
        assert_eq!(strategy_for("", None), JsonRecoveryStrategy::Lenient);
    }

    #[test]
    fn strategy_for_lookup_is_case_insensitive() {
        // The lookup walks STRATEGY_BY_MODEL with eq_ignore_ascii_case
        // so an operator's TOML `model = "Kimi-K3"` still resolves.
        assert_eq!(strategy_for("KIMI-K3", None), JsonRecoveryStrategy::Lenient);
        assert_eq!(
            strategy_for("Minimax-M3", None),
            JsonRecoveryStrategy::Continuation
        );
        assert_eq!(
            strategy_for("DeepSeek-V4-Pro", None),
            JsonRecoveryStrategy::PromptPrefill
        );
        assert_eq!(
            strategy_for("GPT-5.6-LUNA", None),
            JsonRecoveryStrategy::Strict
        );
    }

    #[test]
    fn strategy_for_first_match_wins() {
        // The table has both `kimi-k3` and a hypothetical `kimi-k3-lite`
        // would collide on the prefix. We pin the invariant: the
        // current table has no overlapping prefixes, and a future
        // entry that does overlap must come BEFORE the more general
        // one (we don't have a `kimi` prefix entry, so a model named
        // `kimi-k3-future` would NOT match `kimi-k3` — it would fall
        // through to Lenient).
        assert_eq!(
            strategy_for("kimi-k3-future", None),
            JsonRecoveryStrategy::Lenient, // = DEFAULT_STRATEGY
            "models that don't match an entry fall through to default"
        );
    }

    // --- strategy_for: profile overrides ------------------------------

    #[test]
    fn strategy_for_profile_override_wins_over_table() {
        // An operator who pinned `deepseek-v4-pro` to Strict via the
        // (future) profile-overrides hook must see that override win
        // over the per-model default.
        let mut overrides = HashMap::new();
        overrides.insert("deepseek-v4-pro".to_string(), JsonRecoveryStrategy::Strict);
        assert_eq!(
            strategy_for("deepseek-v4-pro", Some(&overrides)),
            JsonRecoveryStrategy::Strict
        );
    }

    #[test]
    fn strategy_for_profile_override_can_be_downgraded_to_lenient() {
        // The inverse: pin a default-Strict model to Lenient.
        let mut overrides = HashMap::new();
        overrides.insert("gpt-5.6-luna".to_string(), JsonRecoveryStrategy::Lenient);
        assert_eq!(
            strategy_for("gpt-5.6-luna", Some(&overrides)),
            JsonRecoveryStrategy::Lenient
        );
    }

    #[test]
    fn strategy_for_profile_override_only_affects_target_model() {
        // An override for `gpt-5.6-luna` must NOT change the
        // resolution for a different model.
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-5.6-luna".to_string(),
            JsonRecoveryStrategy::PromptPrefill,
        );
        assert_eq!(
            strategy_for("gpt-5.6-luna", Some(&overrides)),
            JsonRecoveryStrategy::PromptPrefill
        );
        // Other models stay on their table value (or default).
        assert_eq!(
            strategy_for("kimi-k3", Some(&overrides)),
            JsonRecoveryStrategy::Lenient
        );
        assert_eq!(
            strategy_for("gpt-7-future", Some(&overrides)),
            JsonRecoveryStrategy::Lenient
        );
    }

    #[test]
    fn strategy_for_empty_profile_overrides_is_no_op() {
        let overrides: HashMap<String, JsonRecoveryStrategy> = HashMap::new();
        // Empty map behaves identically to `None`.
        assert_eq!(
            strategy_for("kimi-k3", Some(&overrides)),
            strategy_for("kimi-k3", None)
        );
    }

    #[test]
    fn strategy_for_override_key_is_case_sensitive() {
        // Override map keys are case-sensitive (the user's TOML
        // identifies a specific model); only the table walk in
        // step 2 is case-insensitive.
        let mut overrides = HashMap::new();
        overrides.insert("kimi-k3".to_string(), JsonRecoveryStrategy::Strict);
        assert_eq!(
            strategy_for("kimi-k3", Some(&overrides)),
            JsonRecoveryStrategy::Strict
        );
        // Different casing on the override key falls through to
        // the table walk, which is itself case-insensitive — so
        // the table value for `KIMI-K3` (the canonical `kimi-k3`
        // row) is `Lenient`, NOT `Strict`. This pins the
        // case-sensitivity contract of the override map.
        assert_eq!(
            strategy_for("KIMI-K3", Some(&overrides)),
            JsonRecoveryStrategy::Lenient // matches the table via eq_ignore_ascii_case
        );
    }

    // --- helpers ------------------------------------------------------

    #[test]
    fn needs_assistant_prefill_only_for_prompt_prefill() {
        assert!(needs_assistant_prefill(JsonRecoveryStrategy::PromptPrefill));
        assert!(!needs_assistant_prefill(JsonRecoveryStrategy::Strict));
        assert!(!needs_assistant_prefill(JsonRecoveryStrategy::Lenient));
        assert!(!needs_assistant_prefill(JsonRecoveryStrategy::Continuation));
    }

    #[test]
    fn max_continuation_attempts_per_strategy() {
        assert_eq!(max_continuation_attempts(JsonRecoveryStrategy::Strict), 0);
        assert_eq!(max_continuation_attempts(JsonRecoveryStrategy::Lenient), 0);
        assert_eq!(
            max_continuation_attempts(JsonRecoveryStrategy::Continuation),
            2
        );
        assert_eq!(
            max_continuation_attempts(JsonRecoveryStrategy::PromptPrefill),
            0
        );
    }

    // --- table consistency -------------------------------------------

    #[test]
    fn strategy_table_has_no_duplicate_keys() {
        // Defensive: if two entries collide (case-insensitive),
        // strategy_for would still pick the FIRST, but the table
        // would carry dead entries. Pin the invariant.
        let mut seen: Vec<String> = Vec::new();
        for (k, _) in STRATEGY_BY_MODEL {
            let lower = k.to_ascii_lowercase();
            assert!(
                !seen.contains(&lower),
                "STRATEGY_BY_MODEL contains a case-insensitive duplicate for {k}"
            );
            seen.push(lower);
        }
    }
}
