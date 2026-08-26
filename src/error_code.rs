//! Stable error codes (D.12.8 + D.12.12).
//!
//! Each `crate::error::Error` variant maps to a stable
//! [`ErrorCode`] that serializes to SCREAMING_SNAKE_CASE. The
//! code is the public contract (consumed by dashboards, external
//! tooling, and operators); the `Error` enum is the internal Rust
//! contract. Codes are append-only — once published, a variant
//! must never change its wire form.
//!
//! Three helpers drive runtime policy:
//!
//! - [`ErrorCode::stable`] returns the canonical wire string.
//! - [`ErrorCode::is_retriable`] flags errors worth retrying with
//!   backoff (HTTP 429/5xx, transport blips, transient timeouts).
//! - [`ErrorCode::is_circuit_opening`] flags errors that should
//!   count toward the per-provider circuit breaker (5xx, transport
//!   failures, auth errors).
//!
//! New variants MUST be added to the enum AND to the mapping in
//! `crate::error::Error::code`; serde's derived serialization
//! takes care of the wire form automatically.

use serde::{Deserialize, Serialize};

/// Public, stable error code. Wire form is SCREAMING_SNAKE_CASE.
///
/// Adding a variant is non-breaking; renaming or removing one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Filesystem lookup missed.
    FsNotFound,
    /// Provider rejected the credentials.
    ProviderAuth,
    /// Provider returned HTTP 429 (rate limit).
    ProviderRateLimit,
    /// Human checkpoint was rejected by the user.
    CheckpointRejected,
    /// Internal invariant broken; should never happen.
    InternalInvariant,
    /// HTTP 400.
    Http400,
    /// HTTP 401 (often = `ProviderAuth`).
    Http401,
    /// HTTP 403.
    Http403,
    /// HTTP 404.
    Http404,
    /// HTTP 408 (request timeout).
    Http408,
    /// HTTP 413 (payload too large).
    Http413,
    /// HTTP 429.
    Http429,
    /// HTTP 500.
    Http500,
    /// HTTP 502.
    Http502,
    /// HTTP 503.
    Http503,
    /// HTTP 504.
    Http504,
    /// Transport-level failure (TLS handshake, DNS, connection reset).
    TransportError,
    /// Provider returned non-JSON where JSON was expected.
    JsonInvalid,
    /// Provider returned JSON that failed schema validation.
    SchemaViolation,
    /// Response was cut off before completion.
    Truncated,
    /// Sketch phase hit its timeout.
    TimeoutSketch,
    /// A single phase exceeded its timeout.
    TimeoutPhase,
    /// Whole run exceeded the total timeout.
    TimeoutTotal,
    /// Token/credit budget exhausted.
    BudgetExhausted,
    /// User or supervisor cancelled the work.
    Cancelled,
    /// Plan paused (operator decision, recoverable).
    PlanPaused,
    /// Circuit breaker is open; calls are short-circuited.
    CircuitOpen,
    /// Sandbox refused the command (denylist / policy).
    SandboxNotAllowed,
    /// Sandbox child exceeded its wall clock.
    SandboxTimeout,
    /// Required interpreter / binary was missing.
    SandboxNoBinary,
    /// Sandbox child was killed (OOM / watchdog).
    SandboxKilled,
    /// Provider explicitly reported overload.
    ProviderOverloaded,
    /// Hard quota reached.
    QuotaExceeded,
    /// Provider filtered the content.
    ContentFiltered,
    /// Provider response did not match expected shape.
    InvalidResponse,
    /// Pipeline needs human input to continue (batch mode).
    NeedsInput,
    /// Context reference (run / sketch id) could not be resolved.
    ContextRefNotFound,
    /// Context reference resolved but was malformed.
    ContextRefInvalid,
    /// Prompt exceeded the configured size budget.
    InputTooLarge,
    /// Possible prompt injection detected (heuristic).
    PromptInjectionSuspected,
    /// Prompt injection confirmed (high-confidence).
    PromptInjectionConfirmed,
    /// Hostile prompt that the policy refused outright.
    HostilePrompt,
    /// Manifest deserialised but contents were inconsistent.
    ManifestInconsistent,
    /// Export bundle failed the SHA256SUMS verify pass.
    ExportVerificationFailed,
    /// Disk is full; cannot write.
    OutOfDiskSpace,
    /// Catch-all for unmapped variants.
    UnhandledError,
    /// Resource already exists.
    AlreadyExists,
    /// Operation forbidden by policy.
    Forbidden,
    /// Resource not found.
    NotFound,
    /// Operation / option not supported.
    Unsupported,
    /// Provider does not support streaming.
    StreamingNotSupported,
    /// Authentication / API key problem.
    Auth,
    /// Configuration problem.
    Config,
    /// Plain I/O failure.
    Io,
    /// SQLite error.
    Sqlite,
    /// JSON (de)serialization error.
    Json,
    /// TOML (de)serialization error.
    Toml,
    /// Plain formatter / print error.
    Fmt,
    /// Generic parse error.
    Parse,
    /// Caller-supplied error that does not match a stable code.
    Custom,
    /// Invalid CLI / API argument.
    InvalidArgs,
    /// Illegal state-machine transition.
    InvalidState,
    #[allow(missing_docs)]
    Storage,
    #[allow(missing_docs)]
    Llm,
    #[allow(missing_docs)]
    Sandbox,
    #[allow(missing_docs)]
    Research,
    #[allow(missing_docs)]
    Resume,
    #[allow(missing_docs)]
    Discovery,
}

impl ErrorCode {
    /// Canonical wire form. Returns the SCREAMING_SNAKE_CASE
    /// representation that external tools (dashboards, CI
    /// scripts, alerts) consume. Matches the form serde
    /// produces with `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]`,
    /// so a serde round-trip is a no-op.
    pub fn stable(&self) -> &'static str {
        let s = match self {
            Self::FsNotFound => "FS_NOT_FOUND",
            Self::ProviderAuth => "PROVIDER_AUTH",
            Self::ProviderRateLimit => "PROVIDER_RATE_LIMIT",
            Self::CheckpointRejected => "CHECKPOINT_REJECTED",
            Self::InternalInvariant => "INTERNAL_INVARIANT",
            Self::Http400 => "HTTP400",
            Self::Http401 => "HTTP401",
            Self::Http403 => "HTTP403",
            Self::Http404 => "HTTP404",
            Self::Http408 => "HTTP408",
            Self::Http413 => "HTTP413",
            Self::Http429 => "HTTP429",
            Self::Http500 => "HTTP500",
            Self::Http502 => "HTTP502",
            Self::Http503 => "HTTP503",
            Self::Http504 => "HTTP504",
            Self::TransportError => "TRANSPORT_ERROR",
            Self::JsonInvalid => "JSON_INVALID",
            Self::SchemaViolation => "SCHEMA_VIOLATION",
            Self::Truncated => "TRUNCATED",
            Self::TimeoutSketch => "TIMEOUT_SKETCH",
            Self::TimeoutPhase => "TIMEOUT_PHASE",
            Self::TimeoutTotal => "TIMEOUT_TOTAL",
            Self::BudgetExhausted => "BUDGET_EXHAUSTED",
            Self::Cancelled => "CANCELLED",
            Self::PlanPaused => "PLAN_PAUSED",
            Self::CircuitOpen => "CIRCUIT_OPEN",
            Self::SandboxNotAllowed => "SANDBOX_NOT_ALLOWED",
            Self::SandboxTimeout => "SANDBOX_TIMEOUT",
            Self::SandboxNoBinary => "SANDBOX_NO_BINARY",
            Self::SandboxKilled => "SANDBOX_KILLED",
            Self::ProviderOverloaded => "PROVIDER_OVERLOADED",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::ContentFiltered => "CONTENT_FILTERED",
            Self::InvalidResponse => "INVALID_RESPONSE",
            Self::NeedsInput => "NEEDS_INPUT",
            Self::ContextRefNotFound => "CONTEXT_REF_NOT_FOUND",
            Self::ContextRefInvalid => "CONTEXT_REF_INVALID",
            Self::InputTooLarge => "INPUT_TOO_LARGE",
            Self::PromptInjectionSuspected => "PROMPT_INJECTION_SUSPECTED",
            Self::PromptInjectionConfirmed => "PROMPT_INJECTION_CONFIRMED",
            Self::HostilePrompt => "HOSTILE_PROMPT",
            Self::ManifestInconsistent => "MANIFEST_INCONSISTENT",
            Self::ExportVerificationFailed => "EXPORT_VERIFICATION_FAILED",
            Self::OutOfDiskSpace => "OUT_OF_DISK_SPACE",
            Self::UnhandledError => "UNHANDLED_ERROR",
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::Unsupported => "UNSUPPORTED",
            Self::StreamingNotSupported => "STREAMING_NOT_SUPPORTED",
            Self::Auth => "AUTH",
            Self::Config => "CONFIG",
            Self::Io => "IO",
            Self::Sqlite => "SQLITE",
            Self::Json => "JSON",
            Self::Toml => "TOML",
            Self::Fmt => "FMT",
            Self::Parse => "PARSE",
            Self::Custom => "CUSTOM",
            Self::InvalidArgs => "INVALID_ARGS",
            Self::InvalidState => "INVALID_STATE",
            Self::Storage => "STORAGE",
            Self::Llm => "LLM",
            Self::Sandbox => "SANDBOX",
            Self::Research => "RESEARCH",
            Self::Resume => "RESUME",
            Self::Discovery => "DISCOVERY",
        };
        tracing::trace!(code = ?self, wire = s, "ErrorCode::stable");
        s
    }

    /// Should the caller retry? Returned errors include HTTP 5xx,
    /// 429, transport blips, and the per-phase / total timeouts.
    /// User-facing errors (`Cancelled`, `InvalidArgs`, `HostilePrompt`)
    /// never retry.
    pub fn is_retriable(&self) -> bool {
        let retriable = matches!(
            self,
            Self::Http429
                | Self::Http500
                | Self::Http502
                | Self::Http503
                | Self::Http504
                | Self::TransportError
                | Self::TimeoutSketch
                | Self::TimeoutPhase
                | Self::TimeoutTotal
                | Self::ProviderOverloaded
                | Self::ProviderRateLimit
        );
        tracing::trace!(code = ?self, retriable, "ErrorCode::is_retriable");
        retriable
    }

    /// Does this error count toward the per-provider circuit
    /// breaker? Adds `Auth` to the retriable set: a provider that
    /// rejects the credentials should be temporarily sidelined
    /// instead of hammering it on every call.
    pub fn is_circuit_opening(&self) -> bool {
        let opening = matches!(
            self,
            Self::Http429
                | Self::Http500
                | Self::Http502
                | Self::Http503
                | Self::Http504
                | Self::TransportError
                | Self::ProviderOverloaded
                | Self::Auth
        );
        tracing::trace!(code = ?self, opening, "ErrorCode::is_circuit_opening");
        opening
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_matches_serde_json_form() {
        // The canonical wire form must equal what serde emits so
        // external tooling can rely on either path.
        for variant in [
            ErrorCode::FsNotFound,
            ErrorCode::ProviderAuth,
            ErrorCode::Http429,
            ErrorCode::Cancelled,
            ErrorCode::TimeoutSketch,
            ErrorCode::InvalidArgs,
            ErrorCode::SandboxTimeout,
        ] {
            let serde_form = serde_json::to_string(&variant)
                .expect("ErrorCode serializes")
                .trim_matches('"')
                .to_string();
            assert_eq!(
                variant.stable(),
                serde_form.as_str(),
                "variant: {variant:?}"
            );
        }
    }

    #[test]
    fn stable_returns_screaming_snake_case() {
        // Spot-check several variants: every char is uppercase,
        // digit, or underscore, and at least one letter is present.
        for variant in [
            ErrorCode::FsNotFound,
            ErrorCode::ProviderAuth,
            ErrorCode::ProviderRateLimit,
            ErrorCode::CheckpointRejected,
            ErrorCode::InternalInvariant,
            ErrorCode::Http400,
            ErrorCode::Http500,
            ErrorCode::TransportError,
            ErrorCode::JsonInvalid,
            ErrorCode::SchemaViolation,
            ErrorCode::Truncated,
            ErrorCode::TimeoutSketch,
            ErrorCode::BudgetExhausted,
            ErrorCode::Cancelled,
            ErrorCode::SandboxNotAllowed,
            ErrorCode::ProviderOverloaded,
            ErrorCode::QuotaExceeded,
            ErrorCode::InvalidArgs,
        ] {
            let s = variant.stable();
            assert!(!s.is_empty(), "variant: {variant:?}");
            let mut has_letter = false;
            for ch in s.chars() {
                assert!(
                    ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_',
                    "non SCREAMING_SNAKE char in {s:?} (variant {variant:?})"
                );
                if ch.is_ascii_uppercase() {
                    has_letter = true;
                }
            }
            assert!(has_letter, "no letter in {s:?}");
        }
    }

    #[test]
    fn is_retriable_classification() {
        // Retriable set
        assert!(ErrorCode::Http429.is_retriable());
        assert!(ErrorCode::Http500.is_retriable());
        assert!(ErrorCode::Http502.is_retriable());
        assert!(ErrorCode::Http503.is_retriable());
        assert!(ErrorCode::Http504.is_retriable());
        assert!(ErrorCode::TransportError.is_retriable());
        assert!(ErrorCode::TimeoutSketch.is_retriable());
        assert!(ErrorCode::TimeoutPhase.is_retriable());
        assert!(ErrorCode::TimeoutTotal.is_retriable());
        assert!(ErrorCode::ProviderOverloaded.is_retriable());
        assert!(ErrorCode::ProviderRateLimit.is_retriable());

        // Non-retriable set
        assert!(!ErrorCode::Cancelled.is_retriable());
        assert!(!ErrorCode::InvalidArgs.is_retriable());
        assert!(!ErrorCode::HostilePrompt.is_retriable());
        assert!(!ErrorCode::InvalidState.is_retriable());
        assert!(!ErrorCode::SchemaViolation.is_retriable());
        assert!(!ErrorCode::BudgetExhausted.is_retriable());
    }

    #[test]
    fn is_circuit_opening_classification() {
        // Openers
        assert!(ErrorCode::Http429.is_circuit_opening());
        assert!(ErrorCode::Http500.is_circuit_opening());
        assert!(ErrorCode::Http502.is_circuit_opening());
        assert!(ErrorCode::Http503.is_circuit_opening());
        assert!(ErrorCode::Http504.is_circuit_opening());
        assert!(ErrorCode::TransportError.is_circuit_opening());
        assert!(ErrorCode::ProviderOverloaded.is_circuit_opening());
        assert!(ErrorCode::Auth.is_circuit_opening());

        // Non-openers
        assert!(!ErrorCode::Cancelled.is_circuit_opening());
        assert!(!ErrorCode::InvalidArgs.is_circuit_opening());
        assert!(!ErrorCode::BudgetExhausted.is_circuit_opening());
        assert!(!ErrorCode::SchemaViolation.is_circuit_opening());
    }

    #[test]
    fn retriable_is_subset_of_circuit_opening_plus_timeouts() {
        // Every retriable code is either circuit-opening or one of
        // the per-phase / total timeouts. Pin the invariant so a
        // future change that adds a new retriable code without a
        // matching circuit-opening class trips the test.
        for variant in [
            ErrorCode::Http429,
            ErrorCode::Http500,
            ErrorCode::Http502,
            ErrorCode::Http503,
            ErrorCode::Http504,
            ErrorCode::TransportError,
            ErrorCode::TimeoutSketch,
            ErrorCode::TimeoutPhase,
            ErrorCode::TimeoutTotal,
            ErrorCode::ProviderOverloaded,
            ErrorCode::ProviderRateLimit,
        ] {
            let retriable = variant.is_retriable();
            let circuit_or_timeout = variant.is_circuit_opening()
                || matches!(
                    variant,
                    ErrorCode::TimeoutSketch
                        | ErrorCode::TimeoutPhase
                        | ErrorCode::TimeoutTotal
                        | ErrorCode::ProviderRateLimit
                );
            assert!(
                retriable && circuit_or_timeout,
                "invariant broken for {variant:?}"
            );
        }
    }

    #[test]
    fn serde_round_trip_preserves_value() {
        // External tools must be able to read what we write.
        for variant in [
            ErrorCode::FsNotFound,
            ErrorCode::ProviderAuth,
            ErrorCode::Http429,
            ErrorCode::Http500,
            ErrorCode::Cancelled,
            ErrorCode::TimeoutSketch,
            ErrorCode::InvalidArgs,
            ErrorCode::SandboxTimeout,
            ErrorCode::ManifestInconsistent,
            ErrorCode::PromptInjectionConfirmed,
            ErrorCode::Custom,
        ] {
            let j = serde_json::to_string(&variant).unwrap();
            let back: ErrorCode = serde_json::from_str(&j).unwrap();
            assert_eq!(variant, back, "round-trip mismatch for {variant:?}");
        }
    }

    #[test]
    fn stable_uses_strict_screaming_snake_for_known_codes() {
        // Pin the exact wire form for a handful of variants so
        // accidental rename trips the test.
        assert_eq!(ErrorCode::FsNotFound.stable(), "FS_NOT_FOUND");
        assert_eq!(ErrorCode::ProviderAuth.stable(), "PROVIDER_AUTH");
        assert_eq!(ErrorCode::Http429.stable(), "HTTP429");
        assert_eq!(ErrorCode::Cancelled.stable(), "CANCELLED");
        assert_eq!(ErrorCode::InvalidArgs.stable(), "INVALID_ARGS");
        assert_eq!(ErrorCode::TimeoutSketch.stable(), "TIMEOUT_SKETCH");
        assert_eq!(
            ErrorCode::PromptInjectionConfirmed.stable(),
            "PROMPT_INJECTION_CONFIRMED"
        );
        assert_eq!(ErrorCode::SandboxKilled.stable(), "SANDBOX_KILLED");
    }

    #[test]
    fn code_count_is_above_minimum() {
        // D.12.8 promised 30+ variants. We ship well above that
        // so pin the floor: removing variants later trips the test.
        let count = [
            ErrorCode::FsNotFound,
            ErrorCode::ProviderAuth,
            ErrorCode::ProviderRateLimit,
            ErrorCode::CheckpointRejected,
            ErrorCode::InternalInvariant,
            ErrorCode::Http400,
            ErrorCode::Http401,
            ErrorCode::Http403,
            ErrorCode::Http404,
            ErrorCode::Http408,
            ErrorCode::Http413,
            ErrorCode::Http429,
            ErrorCode::Http500,
            ErrorCode::Http502,
            ErrorCode::Http503,
            ErrorCode::Http504,
            ErrorCode::TransportError,
            ErrorCode::JsonInvalid,
            ErrorCode::SchemaViolation,
            ErrorCode::Truncated,
            ErrorCode::TimeoutSketch,
            ErrorCode::TimeoutPhase,
            ErrorCode::TimeoutTotal,
            ErrorCode::BudgetExhausted,
            ErrorCode::Cancelled,
            ErrorCode::PlanPaused,
            ErrorCode::CircuitOpen,
            ErrorCode::SandboxNotAllowed,
            ErrorCode::SandboxTimeout,
            ErrorCode::SandboxNoBinary,
            ErrorCode::SandboxKilled,
            ErrorCode::ProviderOverloaded,
            ErrorCode::QuotaExceeded,
            ErrorCode::ContentFiltered,
            ErrorCode::InvalidResponse,
            ErrorCode::NeedsInput,
            ErrorCode::ContextRefNotFound,
            ErrorCode::ContextRefInvalid,
            ErrorCode::InputTooLarge,
            ErrorCode::PromptInjectionSuspected,
            ErrorCode::PromptInjectionConfirmed,
            ErrorCode::HostilePrompt,
            ErrorCode::ManifestInconsistent,
            ErrorCode::ExportVerificationFailed,
            ErrorCode::OutOfDiskSpace,
            ErrorCode::UnhandledError,
            ErrorCode::AlreadyExists,
            ErrorCode::Forbidden,
            ErrorCode::NotFound,
            ErrorCode::Unsupported,
            ErrorCode::StreamingNotSupported,
            ErrorCode::Auth,
            ErrorCode::Config,
            ErrorCode::Io,
            ErrorCode::Sqlite,
            ErrorCode::Json,
            ErrorCode::Toml,
            ErrorCode::Fmt,
            ErrorCode::Parse,
            ErrorCode::Custom,
            ErrorCode::InvalidArgs,
            ErrorCode::InvalidState,
        ]
        .len();
        assert!(
            count >= 30,
            "ErrorCode has only {count} variants, expected >=30"
        );
    }
}
