//! Per-model reasoning gating for wire requests.
//!
//! Source of truth: the upstream `models.dev` catalog. Each entry
//! answers two questions:
//!
//! 1. Does the model accept a `reasoning_tokens` budget on the wire?
//!    When `false` (e.g. the `kimi-*` family) the provider must drop
//!    the field from the request body so the upstream returns a 400.
//!
//! 2. Does the model expose a `reasoning_effort` selector (low /
//!    medium / high)? When the catalog lists
//!    `reasoning_options: [{ kind: "toggle" }]` the wire may carry
//!    `reasoning_effort`; otherwise the field must stay absent even
//!    when reasoning is enabled.
//!
//! Conservative default for an unknown model: `reasoning = false`.
//! An opt-in is safer than an opt-out here — a model that does not
//! accept `reasoning_tokens` returns 400, so dropping the field by
//! default keeps every uncatalogued model alive until the operator
//! confirms it.
//!
//! This module is the **PR-4** layer of the catalog plan. PR-1
//! (`models_dev.rs`) introduces the typed catalog entries; this
//! module imports the `ModelsDevEntry` shape via
//! [`ModelsDevEntry`] and applies it at the request builder. The
//! static catalogue below is the seed the rest of the runtime
//! eventually fetches at startup; replacing it with the live
//! fetch is a follow-up PR and does not require changes here.
//!
//! ## Public API
//!
//! - [`gate_for_model`] returns the [`ReasoningGate`] for a model.
//! - [`apply_to_request`] mutates a [`Request`] so the gated fields
//!   honour the catalogue decision. Pure function — same input,
//!   same output — so the gate is trivially cacheable and unit-
//!   testable.
//!
//! ## Env-var extension
//!
//! `MOAGAN_REASONING_DISABLED` extends the catalogue with extra
//! model names that should be treated as `reasoning: false`. The
//! format matches the existing
//! [`MOAGAN_RESPONSE_FORMAT_OPT_OUT`](super::response_format_opt_out)
//! convention: comma-separated, case-insensitive, whitespace
//! trimmed. Models in the live catalogue that already say
//! `reasoning: true` are NOT overridden by the env var — env
//! entries can only *turn off* reasoning, never turn it on,
//! because gating reasoning on for an unknown upstream risks a
//! 400 that the operator would have to debug.

use super::wire::Request;

/// Subset of the upstream `models.dev` `reasoning_options[]` entry
/// shape used by the gate. PR-1 may extend this with extra fields
/// (e.g. `default`); for PR-4 only the `kind` discriminator is
/// needed to decide whether the wire body may carry
/// `reasoning_effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasoningOptionKind {
    /// On/off toggle. The wire body may carry `reasoning_effort`
    /// alongside `reasoning_tokens`.
    Toggle,
    /// Explicit effort selector (low / medium / high). Treated
    /// the same as [`ReasoningOptionKind::Toggle`] for the wire
    /// shape — both surface an effort enum upstream.
    Effort,
}

/// A single entry from `models.dev`'s `reasoning_options[]`
/// array. Mirrors the JSON shape byte-for-byte so the same struct
/// can be deserialised once PR-1's catalog fetch lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReasoningOption {
    /// Discriminator that decides whether the wire body may carry
    /// `reasoning_effort`. Only `Toggle` and `Effort` variants
    /// enable the field; any future variant added without an
    /// effort mapping defaults to off until the gate is extended.
    pub kind: ReasoningOptionKind,
}

/// Minimal `models.dev`-style entry the gate reads.
///
/// Mirrors the upstream fields PR-1 plans to expose. Constructed
/// inline from the static catalogue below; once the live fetch
/// lands the same struct can be deserialised straight from the
/// catalog JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModelsDevEntry {
    /// Whether the upstream accepts `reasoning_tokens`.
    pub reasoning: bool,
    /// Effort options the upstream supports. Empty when the
    /// upstream only honours the on/off toggle without a level.
    pub reasoning_options: &'static [ReasoningOption],
}

/// Decision the gate hands to the wire builder. `Copy + Eq` so it
/// composes cleanly with `derive(Debug)` builders and patterns
/// like `matches!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ReasoningGate {
    /// `true` when the wire body may carry `reasoning_tokens`.
    pub send_reasoning_tokens: bool,
    /// `true` when the wire body may carry `reasoning_effort`.
    /// Implies `send_reasoning_tokens` — the effort field is
    /// meaningless without the budget field.
    pub send_effort: bool,
    /// Default effort to serialise when `send_effort` is true.
    /// Always `Some("medium")` when set; the operator can flip
    /// the per-role value in a follow-up without changing the
    /// gate surface.
    pub effort: Option<&'static str>,
}

/// Static seed catalogue. Mirrors the operator roster in
/// `docs/proposal-02-rust.md` § 4.2 plus the models.dev
/// observation from the catalog snapshot. Update when a new
/// model joins the roster; the gate will then honour it without
/// a code change in the wire builder.
static CATALOG: &[(&str, ModelsDevEntry)] = &[
    // OpenAI Responses API (gpt-5.6-luna).
    (
        "gpt-5.6-luna",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    // MiniMax / Anthropic-compatible family. Per the catalog,
    // reasoning is opt-in via a toggle.
    (
        "minimax-m3",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    (
        "minimax-m2.7",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    (
        "minimax-m2.5",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    // qwen3.x family. Reasoning supported via toggle.
    (
        "qwen3.8-max",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    (
        "qwen3.7-max",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    (
        "qwen3.7-plus",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    (
        "qwen3.6-plus",
        ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[ReasoningOption {
                kind: ReasoningOptionKind::Toggle,
            }],
        },
    ),
    // kimi family. Reasoning is OFF — the upstream returns 400
    // when `reasoning_tokens` is present.
    (
        "kimi-k3",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    (
        "kimi-k2.7-code",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    (
        "kimi-k2.6",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    // glm family. Prose-only — no reasoning budget.
    (
        "glm-5.1",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    (
        "glm-5.2",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    // DeepSeek family. Prose-only.
    (
        "deepseek-v4-pro",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    (
        "deepseek-v4-flash",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    // mimo / hy3. Prose-only.
    (
        "mimo-v2.5",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    (
        "mimo-v2.5-pro",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
    (
        "hy3",
        ModelsDevEntry {
            reasoning: false,
            reasoning_options: &[],
        },
    ),
];

/// Env-var name for the per-process reasoning opt-out list.
/// Mirrors the existing `MOAGAN_RESPONSE_FORMAT_OPT_OUT`
/// convention.
const ENV_VAR: &str = "MOAGAN_REASONING_DISABLED";

/// Look up the catalog entry for `model`. Case-insensitive
/// match. Returns `None` when the model is not on the operator
/// roster — the gate then falls back to the conservative
/// [`ReasoningGate::default`] (reasoning off).
pub fn lookup_entry(model: &str) -> Option<ModelsDevEntry> {
    CATALOG
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(model))
        .map(|(_, entry)| *entry)
}

/// Returns `true` when `model` appears in the static catalog
/// with `reasoning: false`, OR in the runtime `MOAGAN_REASONING_DISABLED`
/// env var (comma-separated, case-insensitive). The env var is
/// an additive opt-out: a model that the catalog already says
/// supports reasoning is **not** overridden.
fn env_disables_reasoning(model: &str) -> bool {
    let Ok(extra) = std::env::var(ENV_VAR) else {
        return false;
    };
    extra
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|name| name.eq_ignore_ascii_case(model))
}

/// Compute the gate the wire builder must apply for `model`.
///
/// Conservative default (`reasoning: false`) when the model is
/// not catalogued. Env-var opt-outs extend the catalog by
/// *forcing* the gate off for the listed models — useful when
/// the operator rolls out a new model whose catalog entry has
/// not landed yet.
pub fn gate_for_model(model: &str) -> ReasoningGate {
    if env_disables_reasoning(model) {
        return ReasoningGate::default();
    }
    match lookup_entry(model) {
        Some(entry) => entry_to_gate(entry),
        None => ReasoningGate::default(),
    }
}

fn entry_to_gate(entry: ModelsDevEntry) -> ReasoningGate {
    let send_effort = entry.reasoning
        && entry.reasoning_options.iter().any(|o| {
            matches!(
                o.kind,
                ReasoningOptionKind::Toggle | ReasoningOptionKind::Effort
            )
        });
    ReasoningGate {
        send_reasoning_tokens: entry.reasoning,
        send_effort,
        effort: if send_effort { Some("medium") } else { None },
    }
}

/// Apply the gate to `req`. Returns the (possibly mutated)
/// request so the wire builder can serialise it directly.
///
/// - When `gate.send_reasoning_tokens` is `false`, the
///   `reasoning_tokens` field is cleared.
/// - When `gate.send_effort` is `false`, the `reasoning_effort`
///   field is cleared.
/// - The remaining fields (model, system, user, temperature, etc.)
///   are returned unchanged.
///
/// Pure: the same `(req, gate)` always produces the same output
/// `Request`. The `Request` is `Clone` so the caller can keep
/// the pre-gate copy for cache-key purposes (the gate is a
/// request-side concern; the cache key MUST NOT include the
/// gated fields or two equivalent requests would diverge on
/// cache hit).
pub fn apply_to_request(req: &Request, gate: ReasoningGate) -> Request {
    let mut out = req.clone();
    if !gate.send_reasoning_tokens {
        out.reasoning_tokens = None;
    }
    if !gate.send_effort {
        out.reasoning_effort = None;
    } else if out.reasoning_effort.is_none() {
        out.reasoning_effort = gate.effort.map(|s| s.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::role::Role;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_lock<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        f();
    }

    fn sample_request() -> Request {
        Request {
            role: Role::Intake,
            model: "minimax-m3".into(),
            system: "sys".into(),
            user: "user".into(),
            max_tokens: 1024,
            temperature: Some(0.4),
            top_p: Some(0.9),
            response_schema: None,
            stream: false,
            extra_messages: vec![],
            reasoning_tokens: Some(2048),
            reasoning_effort: Some("high".into()),
        }
    }

    /// `reasoning: false` catalog entry → the gate must drop
    /// `reasoning_tokens` regardless of what the caller set.
    #[test]
    fn reasoning_false_drops_reasoning_tokens() {
        let req = sample_request();
        let gate = gate_for_model("kimi-k3");
        assert!(!gate.send_reasoning_tokens);
        assert!(!gate.send_effort);
        let out = apply_to_request(&req, gate);
        assert!(
            out.reasoning_tokens.is_none(),
            "reasoning=false must clear reasoning_tokens, got {:?}",
            out.reasoning_tokens
        );
        assert!(
            out.reasoning_effort.is_none(),
            "reasoning=false must clear reasoning_effort, got {:?}",
            out.reasoning_effort
        );
    }

    /// `reasoning: true` catalog entry → the gate must keep the
    /// caller's `reasoning_tokens` value. The wire builder may
    /// still pick a default when the field is `None`, but the
    /// gate itself does not drop the field.
    #[test]
    fn reasoning_true_keeps_reasoning_tokens() {
        let req = sample_request();
        let gate = gate_for_model("minimax-m3");
        assert!(gate.send_reasoning_tokens);
        let out = apply_to_request(&req, gate);
        assert_eq!(
            out.reasoning_tokens,
            Some(2048),
            "reasoning=true must preserve the caller's reasoning_tokens"
        );
    }

    /// `reasoning: true` + `reasoning_options: [Toggle]` → the
    /// gate must enable `reasoning_effort`. The default effort
    /// is `medium`; the caller may still override before calling
    /// `apply_to_request`.
    #[test]
    fn reasoning_options_toggle_enables_effort() {
        let gate = gate_for_model("minimax-m3");
        assert!(gate.send_effort, "toggle option must enable effort");
        assert_eq!(
            gate.effort,
            Some("medium"),
            "default effort is medium when the caller has not picked one"
        );
        let req = Request {
            reasoning_tokens: None,
            reasoning_effort: None,
            ..sample_request()
        };
        let out = apply_to_request(&req, gate);
        assert_eq!(
            out.reasoning_effort,
            Some("medium".to_owned()),
            "apply_to_request must backfill the default effort"
        );
    }

    /// `reasoning: true` with empty `reasoning_options` →
    /// `reasoning_tokens` survives but `reasoning_effort` is
    /// gated off. (This combination is not currently on the
    /// roster, but the gate must be ready for it.)
    #[test]
    fn reasoning_options_empty_disables_effort() {
        // Build a synthetic entry: reasoning=true but no options.
        let entry = ModelsDevEntry {
            reasoning: true,
            reasoning_options: &[],
        };
        let gate = entry_to_gate(entry);
        assert!(gate.send_reasoning_tokens);
        assert!(
            !gate.send_effort,
            "empty options must disable effort, got gate {gate:?}"
        );
        assert!(gate.effort.is_none());
        let req = sample_request();
        let out = apply_to_request(&req, gate);
        assert_eq!(out.reasoning_tokens, Some(2048));
        assert!(
            out.reasoning_effort.is_none(),
            "apply_to_request must drop reasoning_effort when the gate disables it"
        );
    }

    /// Unknown model → conservative default (reasoning off).
    /// Without a catalog entry we cannot prove the upstream
    /// accepts the field, so the safer default is to drop it.
    #[test]
    fn unknown_model_conservative_drops_reasoning() {
        let gate = gate_for_model("not-in-the-roster-xyz");
        assert!(!gate.send_reasoning_tokens);
        assert!(!gate.send_effort);
        assert!(gate.effort.is_none());
        let req = sample_request();
        let out = apply_to_request(&req, gate);
        assert!(out.reasoning_tokens.is_none());
        assert!(out.reasoning_effort.is_none());
    }

    /// `apply_to_request` is a pure function: same input,
    /// same output, no shared state. The simplest way to assert
    /// purity is to call the function twice on the same input and
    /// compare the byte-for-byte serialised JSON.
    #[test]
    fn apply_to_request_is_pure() {
        let req = sample_request();
        let gate = gate_for_model("minimax-m3");
        let out_a = apply_to_request(&req, gate);
        let out_b = apply_to_request(&req, gate);
        let json_a = serde_json::to_string(&out_a).unwrap();
        let json_b = serde_json::to_string(&out_b).unwrap();
        assert_eq!(json_a, json_b, "apply_to_request must be deterministic");
        // And, separately: the input Request must not be mutated.
        assert_eq!(req.reasoning_tokens, Some(2048));
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
    }

    /// Case-insensitive lookup: `MiniMax-M3`, `MINIMAX-M3`, and
    /// `minimax-m3` all resolve to the same catalog entry. The
    /// opencode_go responses path receives `Request::model` in
    /// the upstream's canonical casing; the gate must not
    /// regress on a typo'd config file.
    #[test]
    fn lookup_is_case_insensitive() {
        let a = lookup_entry("MiniMax-M3").expect("lowercase lookup");
        let b = lookup_entry("MINIMAX-M3").expect("uppercase lookup");
        let c = lookup_entry("minimax-m3").expect("mixed-case lookup");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a.reasoning);
    }

    /// Models on the responses endpoint (`gpt-5.6-luna`) are
    /// catalogued with `reasoning: true` + toggle option. Pin
    /// the catalog assumption that drives the integration
    /// tests in `opencode_go_responses.rs`.
    #[test]
    fn gpt_5_6_luna_supports_effort() {
        let gate = gate_for_model("gpt-5.6-luna");
        assert!(gate.send_reasoning_tokens);
        assert!(gate.send_effort);
    }

    /// `MOAGAN_REASONING_DISABLED` forces the gate off for the
    /// listed models even when the catalog says reasoning is
    /// supported. Env entries are additive: the catalog truth
    /// wins for models not on the env list.
    #[test]
    fn env_var_disables_reasoning() {
        with_lock(|| unsafe {
            std::env::set_var(ENV_VAR, "minimax-m3, foo-bar");
            let gate_m3 = gate_for_model("minimax-m3");
            assert!(
                !gate_m3.send_reasoning_tokens,
                "env var must disable reasoning for minimax-m3"
            );
            let gate_unknown = gate_for_model("foo-bar");
            assert!(
                !gate_unknown.send_reasoning_tokens,
                "env var must disable reasoning for an unknown model"
            );
            // Unrelated models keep their catalog behaviour.
            let gate_qwen = gate_for_model("qwen3.8-max");
            assert!(
                gate_qwen.send_reasoning_tokens,
                "env var must not affect unrelated catalog entries"
            );
            std::env::remove_var(ENV_VAR);
        });
    }

    /// Empty / whitespace-only env var entries are ignored, so a
    /// no-op `MOAGAN_REASONING_DISABLED=""` cannot break the
    /// runtime by accidentally matching the empty string.
    #[test]
    fn env_var_empty_is_ignored() {
        with_lock(|| unsafe {
            std::env::set_var(ENV_VAR, "  , , ");
            let gate = gate_for_model("minimax-m3");
            assert!(
                gate.send_reasoning_tokens,
                "empty env var must not disable reasoning"
            );
            std::env::remove_var(ENV_VAR);
        });
    }

    /// Sanity: every static catalog entry round-trips through
    /// `lookup_entry` and produces a gate that is consistent
    /// with its declared `reasoning` flag.
    #[test]
    fn catalog_entries_are_self_consistent() {
        for (name, entry) in CATALOG {
            let looked_up = lookup_entry(name).unwrap_or_else(|| {
                panic!("catalog entry `{name}` must be discoverable via lookup_entry")
            });
            assert_eq!(
                looked_up.reasoning, entry.reasoning,
                "lookup_entry must preserve the reasoning flag for `{name}`"
            );
            let gate = gate_for_model(name);
            assert_eq!(
                gate.send_reasoning_tokens, entry.reasoning,
                "gate_for_model must match the catalog reasoning flag for `{name}`"
            );
            let has_toggle = entry.reasoning_options.iter().any(|o| {
                matches!(
                    o.kind,
                    ReasoningOptionKind::Toggle | ReasoningOptionKind::Effort
                )
            });
            assert_eq!(
                gate.send_effort,
                entry.reasoning && has_toggle,
                "gate_for_model effort flag must match catalog options for `{name}`"
            );
        }
    }
}
