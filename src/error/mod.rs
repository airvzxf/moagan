//! Crate-wide error type. `thiserror` for the library; `main.rs` wraps
//! everything in `anyhow::Result`.
//!
//! Exit codes follow T01-06 §12.3:
//! 0 ok, 1 generic, 2 invalid args, 3 invalid api key, 4 plan exhausted,
//! 5 timeout, 6 cancelled, 7 schema violation, 8 io error.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::atomic::writer::ArtifactMeta;
use crate::error_code::ErrorCode;

/// Stable process exit codes from T01-06 §12.3 and D.12.14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// Successful execution.
    Ok = 0,
    /// Unclassified failure.
    GenericError = 1,
    /// Invalid CLI arguments from the baseline contract.
    InvalidArgs = 2,
    /// Missing or invalid provider API key.
    ApiKeyInvalid = 3,
    /// Provider plan exhausted.
    PlanExhausted = 4,
    /// Operation timed out under the baseline contract.
    Timeout = 5,
    /// Operation was cancelled.
    Cancelled = 6,
    /// Persistent schema violation.
    SchemaViolation = 7,
    /// I/O failure under the baseline contract.
    IoError = 8,
    /// Token budget exhausted.
    BudgetExhausted = 9,
    /// More user input is required.
    NeedsInput = 10,
    /// Configured budget was exceeded.
    BudgetExceeded = 20,
    /// Provider plan paused the run.
    PlanPaused = 30,
    /// Provider request failed.
    ProviderError = 40,
    /// Timeout from the extended catalog.
    TimeoutExit = 50,
    /// Invalid arguments from the extended catalog.
    InvalidArgsExit = 60,
    /// I/O failure from the extended catalog.
    IoErrorExit = 70,
    /// Run context or state failure.
    ContextError = 80,
    /// Export verification failed.
    ExportVerificationFailed = 90,
    #[allow(missing_docs)]
    Storage = 91,
    #[allow(missing_docs)]
    Llm = 92,
    #[allow(missing_docs)]
    Sandbox = 93,
    #[allow(missing_docs)]
    Research = 94,
    #[allow(missing_docs)]
    Resume = 95,
    #[allow(missing_docs)]
    Discovery = 96,
    /// Process interrupted by SIGINT.
    SigInt = 130,
}

impl From<ErrorCode> for ExitCode {
    fn from(code: ErrorCode) -> Self {
        let out = match code {
            ErrorCode::Storage => Self::Storage,
            ErrorCode::Llm => Self::Llm,
            ErrorCode::Sandbox => Self::Sandbox,
            ErrorCode::Research => Self::Research,
            ErrorCode::Resume => Self::Resume,
            ErrorCode::Discovery => Self::Discovery,
            _ => Self::GenericError,
        };
        tracing::trace!(?code, exit_code = out as i32, "error::ExitCode::from");
        out
    }
}

/// Library error type. All public APIs return `Result<T, Error>`.
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure with structured context.
    #[error(transparent)]
    Io(IoError),

    /// User-supplied argument violated a constraint.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    /// API key is missing or malformed.
    #[error("invalid api key: {message}")]
    InvalidApiKey {
        /// Human-readable reason (typically `"http 401: ..."` or
        /// `"http 403: ..."`).
        message: String,
        /// HTTP status code captured at error-construction time, if
        /// the upstream returned one. `None` for non-HTTP sources
        /// (config validation, missing env, etc.).
        http_status: Option<u16>,
    },

    /// Provider plan is exhausted (token budget consumed). Persistent
    /// failure — opening the breaker per-(provider, role) is the right
    /// response; do NOT retry until the cooldown elapses.
    #[error("plan exhausted: {message}")]
    PlanExhausted {
        /// Human-readable reason (typically `"http 429: ..."`).
        message: String,
        /// HTTP status code captured at error-construction time. For
        /// this variant it is almost always `Some(429)`; `None` only
        /// for callers that synthesise the error locally.
        http_status: Option<u16>,
    },

    /// Provider returned a transient rate-limit (HTTP 429 with
    /// `Retry-After` header). The adaptive
    /// [`crate::llm::governor::ThrottleGovernor`] absorbs these by
    /// reducing per-role concurrency and increasing backoff; the
    /// breaker does NOT trip on this class. When `retry_after_ms`
    /// is `Some(_)`, the value comes from the `Retry-After` response
    /// header (seconds in HTTP date or delta-seconds form).
    #[error("provider throttled: {message} (retry_after_ms={retry_after_ms:?})")]
    Throttled {
        /// Optional `Retry-After` from the upstream response. `None`
        /// when the header was absent or unparseable.
        retry_after_ms: Option<u64>,
        /// The full upstream error message; surfaced verbatim so
        /// post-mortem can correlate the throttle hit with the
        /// specific RPM / TPM / 429 message the upstream returned.
        message: String,
        /// HTTP status code captured at error-construction time. For
        /// this variant it is almost always `Some(429)`; `None` for
        /// callers that synthesise the error locally.
        http_status: Option<u16>,
    },

    /// Operation timed out.
    #[error("timeout: {message}")]
    Timeout {
        /// Human-readable reason (typically `"http 408: ..."`,
        /// `"http 504: ..."` or `"http 524: ..."`).
        message: String,
        /// HTTP status code captured at error-construction time, if
        /// the upstream returned one.
        http_status: Option<u16>,
    },

    /// Operation cancelled by user or supervisor.
    #[error("cancelled: {0}")]
    Cancelled(String),

    /// Schema-validated output failed its contract.
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    /// State machine transition is illegal at the current state.
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// Another holder owns the requested run lease.
    #[error("lock held: {0}")]
    LockHeld(String),

    /// Provider returned a non-recoverable error.
    #[error("provider error: {message}")]
    Provider {
        /// Human-readable reason (typically `"http 5xx: ..."`,
        /// `"upstream 5xx: ..."` or `"network: ..."`).
        message: String,
        /// HTTP status code captured at error-construction time, if
        /// the upstream returned one. `None` for non-HTTP sources
        /// (network errors, sqlite, cache, etc.).
        http_status: Option<u16>,
    },

    /// Mock provider ran out of canned responses.
    #[error("mock provider exhausted")]
    MockExhausted,

    /// Cache lookup failed.
    #[error("cache: {0}")]
    Cache(String),

    /// Cancellation propagated.
    #[error(transparent)]
    Cancel(#[from] CancelSignal),

    /// The dispatcher needs an explicit confirmation to proceed
    /// (e.g. a destructive plan without `--yes`). Mirrors the
    /// `ExitCode::NeedsInput` value so CI scripts can branch on
    /// exit code 10.
    #[error("needs input: {0}")]
    NeedsInput(String),

    /// The discovery phase produced too many failed sketches to be
    /// trustworthy. Triggered when more than half of the planned
    /// attempts fail (with a minimum-attempts guard so small runs
    /// do not abort prematurely). Maps to
    /// [`ExitCode::ContextError`] because the run context
    /// (`run_dir`) is in a degraded state and continuing would
    /// persist low-quality proposals downstream.
    #[error(
        "discovery quality too low: {failed}/{total} attempts failed (threshold {threshold_pct}%)"
    )]
    DiscoveryQualityTooLow {
        /// Number of attempts that produced no usable sketch.
        failed: usize,
        /// Total number of attempts the phase issued.
        total: usize,
        /// Threshold percentage that triggered the abort.
        threshold_pct: usize,
    },

    /// E10 (catalog 10-integrada-v0 §D.20.7): the intake phase's
    /// `Role::HostilePromptDetector` classified the user's raw
    /// prompt as `hostile`, and the configured `HostilePolicy`
    /// rejected the run outright. The inner string carries the
    /// detector's `reasons[0]` (the strongest injection signal) so
    /// the operator sees why the run was rejected without having
    /// to cross-reference the telemetry stream. Maps to
    /// [`ExitCode::ContextError`] because the run is in a
    /// degraded state (the caller asked us to act on a hostile
    /// input) and refusing to continue is the safe default.
    #[error("hostile prompt: {0}")]
    HostilePrompt(String),

    /// D.29.1 (`safe_path` helper): a caller-supplied path either
    /// contained `..` traversal or resolved through a symlink to
    /// a location outside the declared root. The inner string is
    /// the offending candidate as supplied so operators see the
    /// exact input that triggered the rejection. Maps to
    /// [`ExitCode::InvalidArgs`] (exit 2) because the violation is
    /// a malformed user input, not a degraded run state.
    #[error("path traversal detected: {0}")]
    PathTraversal(String),

    /// D.29.2: a payload exceeded its configured size cap. The
    /// inner string carries the `{label}: {bytes} > {cap}` form
    /// from `crate::llm::size_limits::check_size` so the
    /// post-mortem log can pinpoint which budget blew
    /// (`prompt` / `response` / `attachment`). Maps to
    /// [`ExitCode::ProviderError`] because an oversized LLM
    /// response body is functionally equivalent to HTTP 413
    /// from the upstream, and the breaker policy for an
    /// HTTP-413-shaped error already lives there. Not
    /// circuit-opening: a single oversized response does not
    /// mean the provider is unhealthy.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    /// PR-5: the request asked for a capability the model
    /// advertises it does not support. Two triggers, both routed
    /// here so callers can match a single variant instead of two:
    ///
    /// - The models.dev entry has `attachment: false` but the
    ///   [`crate::llm::wire::Request`] carries one or more
    ///   attachments (the gate refuses to silently drop the
    ///   payload — that would change the request identity the
    ///   cache key is built on).
    /// - The models.dev entry lists an input modality the
    ///   attached content uses (`image`, `audio`, `pdf`, …) but
    ///   the entry's `modalities.input` does not include it.
    ///
    /// The inner string is the human-readable reason (e.g.
    /// `"image attachment refused: model accepts [text] only"`).
    /// Maps to [`ExitCode::ProviderError`] because the upstream
    /// is the source of truth for capabilities, and to
    /// [`ErrorCode::Unsupported`] because the failure mode is
    /// "this model cannot do that" rather than a transport
    /// outage. Not circuit-opening: a wrong-capability choice
    /// reflects a caller bug, not a provider health signal.
    #[error("modality not supported by model: {0}")]
    ModalityUnsupported(String),

    /// K.4 sub-1: a research backend dependency is missing or the
    /// upstream pipeline returned no usable signal. The current
    /// trigger is `pdftotext` not being on `PATH` (the binary
    /// ships with the `poppler-utils` system package — see
    /// [`docs/proposal-04-cuarta-etapa.md`](../docs/proposal-04-cuarta-etapa.md)
    /// §4 for the install hint), but the variant stays open for
    /// future "research pipeline unavailable" signals (PDF host
    /// not allowlisted, allowlist blocked, …).
    ///
    /// Maps to [`ErrorCode::Research`] so dashboards can branch
    /// on the wire form, and to [`ExitCode::Research`] (94)
    /// because the failure is on the research surface, not the
    /// provider surface. Not circuit-opening: a missing local
    /// tool is operator configuration, not a remote outage.
    #[error("research unavailable: {0}")]
    ResearchUnavailable(String),
}

impl Error {
    /// HTTP status code captured at error-construction time, if any.
    /// Returns `Some(code)` for errors that originated at the HTTP
    /// transport layer (e.g. `429`, `503`); `None` for errors that
    /// originated below the transport (sqlite, cache, schema
    /// validation, capability gate, network layer, etc.).
    ///
    /// This is the field that lets the telemetry layer populate
    /// `calls.http_status` for failed calls — without it, every
    /// error row ended up with `http_status = NULL` and the operator
    /// had to parse the message string to know whether the upstream
    /// returned 429, 500, or just dropped the connection.
    pub fn http_status(&self) -> Option<u16> {
        let out = match self {
            Self::InvalidApiKey { http_status, .. }
            | Self::PlanExhausted { http_status, .. }
            | Self::Throttled { http_status, .. }
            | Self::Timeout { http_status, .. }
            | Self::Provider { http_status, .. } => *http_status,
            _ => None,
        };
        tracing::trace!(?out, "error::Error::http_status");
        out
    }

    /// Public, stable error code. Maps every `Error` variant to
    /// the closest `ErrorCode` (D.12.8). The mapping is
    /// best-effort: variants that do not have a clean bucket fall
    /// back to `ErrorCode::UnhandledError`. Wire form is
    /// `SCREAMING_SNAKE_CASE` (D.12.12).
    pub fn code(&self) -> ErrorCode {
        let out = match self {
            Self::Io(_) => ErrorCode::Io,
            Self::InvalidArgs(_) => ErrorCode::InvalidArgs,
            Self::InvalidApiKey { .. } => ErrorCode::Auth,
            Self::PlanExhausted { .. } => ErrorCode::QuotaExceeded,
            Self::Throttled { .. } => ErrorCode::Http429,
            Self::Timeout { .. } => ErrorCode::TimeoutPhase,
            Self::Cancelled(_) => ErrorCode::Cancelled,
            Self::SchemaViolation(_) => ErrorCode::SchemaViolation,
            Self::InvalidState(_) => ErrorCode::InvalidState,
            Self::LockHeld(_) => ErrorCode::InvalidState,
            Self::Provider { .. } => ErrorCode::InvalidResponse,
            Self::MockExhausted => ErrorCode::NeedsInput,
            Self::Cache(_) => ErrorCode::Io,
            Self::Cancel(_) => ErrorCode::Cancelled,
            Self::NeedsInput(_) => ErrorCode::NeedsInput,
            Self::DiscoveryQualityTooLow { .. } => ErrorCode::InvalidState,
            Self::HostilePrompt(_) => ErrorCode::HostilePrompt,
            Self::PathTraversal(_) => ErrorCode::InvalidArgs,
            Self::PayloadTooLarge(_) => ErrorCode::InputTooLarge,
            Self::ModalityUnsupported(_) => ErrorCode::Unsupported,
            Self::ResearchUnavailable(_) => ErrorCode::Research,
        };
        tracing::trace!(?out, "error::Error::code");
        out
    }

    /// Return the stable process exit code for this error.
    pub fn exit_code(&self) -> ExitCode {
        let out = match self {
            Self::InvalidArgs(_) => ExitCode::InvalidArgs,
            Self::InvalidApiKey { .. } => ExitCode::ApiKeyInvalid,
            Self::PlanExhausted { .. } => ExitCode::PlanExhausted,
            Self::Throttled { .. } => ExitCode::ProviderError,
            Self::Timeout { .. } => ExitCode::Timeout,
            Self::Cancelled(_) | Self::Cancel(_) => ExitCode::Cancelled,
            Self::SchemaViolation(_) => ExitCode::SchemaViolation,
            Self::Io(_) => ExitCode::IoError,
            Self::MockExhausted | Self::Provider { .. } => ExitCode::ProviderError,
            Self::Cache(_) | Self::InvalidState(_) | Self::LockHeld(_) => ExitCode::ContextError,
            Self::NeedsInput(_) => ExitCode::NeedsInput,
            Self::DiscoveryQualityTooLow { .. } => ExitCode::ContextError,
            Self::HostilePrompt(_) => ExitCode::ContextError,
            Self::PathTraversal(_) => ExitCode::InvalidArgs,
            Self::PayloadTooLarge(_) => ExitCode::ProviderError,
            Self::ModalityUnsupported(_) => ExitCode::ProviderError,
            Self::ResearchUnavailable(_) => ExitCode::Research,
        };
        tracing::trace!(?out, "error::Error::exit_code");
        out
    }

    /// Should this error count toward the per-provider circuit
    /// breaker? Mirrors [`crate::error_code::ErrorCode::is_circuit_opening`]
    /// but operates directly on the [`Error`] variant so the policy
    /// does not depend on the [`crate::Error::code`] mapping — the
    /// latter collapses several variants onto the same code
    /// (`Provider` → `InvalidResponse`, `Timeout` → `TimeoutPhase`)
    /// and loses the HTTP-status distinction the breaker is
    /// supposed to act on.
    ///
    /// **v0.9.8 policy change** — `PlanExhausted` is no longer an
    /// opener. Rationale: the upstream can return HTTP 429 for
    /// two distinct reasons, and the keyword scan in
    /// `classify_throttled_or_plan_exhausted` (`llm/http.rs`)
    /// cannot reliably distinguish them:
    ///
    /// 1. **True plan exhaustion** — the account has hit its
    ///    monthly quota. The breaker should open so we don't
    ///    hammer a paying-customer limit.
    /// 2. **Saturation / rate-limit** — the per-window RPM/TPM cap
    ///    was exceeded because we sent too many calls too fast.
    ///    The breaker should NOT open; the
    ///    [`crate::llm::governor::ThrottleGovernor`] is the right
    ///    tool (AIMD on the per-(provider, role) concurrency
    ///    cap with exponential backoff).
    ///
    /// MiniMax-M3 returns `"Token Plan rate limit reached: ..."`
    /// for **both** signals — the keyword "plan" matches
    /// unconditionally, so the classifier routes every 429 into
    /// `PlanExhausted` and the breaker fires as if the quota
    /// were permanently gone. The throttle governor would have
    /// back-off'd gracefully instead.
    ///
    /// Until we can prove the upstream differentiates the two
    /// signals (e.g. via a different `error.type` or a
    /// `Retry-After` header that distinguishes "wait N seconds"
    /// from "wait until next billing cycle"), the safer default
    /// is to let the throttle handle every 429 and reserve the
    /// breaker for signals the upstream makes unambiguous
    /// (auth, 5xx, timeout).
    ///
    /// Openers:
    ///
    /// - [`Error::InvalidApiKey`] — `ErrorCode::Auth`, HTTP 401/403
    ///   from the provider. The catalog adds `Auth` to the
    ///   circuit-opening set so a provider that rejects the
    ///   credentials is sidelined instead of hammered.
    /// - [`Error::Provider`] — generic 5xx / upstream-error bucket.
    ///   `classify_status` (in `llm/http.rs`) routes HTTP 500..=599
    ///   here, so this is the path the breaker needs to catch the
    ///   common "provider is down" signal.
    /// - [`Error::Timeout`] — HTTP 408/504/524 or any phase-level
    ///   timeout. The breaker treats sustained timeouts as a
    ///   provider-health signal, distinct from the retry budget's
    ///   per-attempt backoff.
    ///
    /// Non-openers:
    ///
    /// - [`Error::PlanExhausted`] — HTTP 429. The throttle
    ///   governor absorbs these via AIMD; opening the breaker
    ///   here would cause every role to be sidelined for the
    ///   cooldown window the moment a single 429 burst happens.
    /// - [`Error::Throttled`] — HTTP 429 with `Retry-After`.
    ///   Same rationale as `PlanExhausted`; the throttle
    ///   governor is the dedicated path for these.
    /// - [`Error::SchemaViolation`], [`Error::InvalidArgs`],
    ///   [`Error::InvalidState`] — operator / contract errors that
    ///   have nothing to do with the provider's health.
    /// - [`Error::Cancelled`] / [`Error::Cancel`] — cooperative
    ///   shutdown; the breaker must not count an operator cancel as
    ///   a provider outage.
    /// - [`Error::MockExhausted`] — the canned-response queue ran
    ///   out; the provider itself is fine.
    /// - [`Error::Io`] / [`Error::Cache`] — local disk / cache
    ///   issues; tripping a remote breaker on a local I/O blip is
    ///   the wrong granularity.
    pub fn is_circuit_opening(&self) -> bool {
        let out = matches!(
            self,
            Self::InvalidApiKey { .. } | Self::Provider { .. } | Self::Timeout { .. }
        );
        tracing::trace!(?out, "error::Error::is_circuit_opening");
        out
    }

    /// Categorize the provider-side errors that demand *recovery* —
    /// distinguishing transient throttling (handled by
    /// [`crate::llm::governor::ThrottleGovernor`]) from persistent
    /// plan exhaustion (handled by [`crate::llm::circuit_breaker`])
    /// from generic provider faults (handled by the retry budget
    /// already configured on the call site).
    ///
    /// The two failure modes look similar upstream — both surface
    /// as HTTP 429 — but require opposite actions. Treating them
    /// identically caused the v0.9.5 cascade where one role's 429
    /// opened the breaker for every other role on the same
    /// provider. The split lands each mode with the recovery path
    /// that fits.
    pub fn provider_cause(&self) -> Option<ProviderCause> {
        let out = Some(match self {
            Self::Throttled {
                retry_after_ms,
                message,
                ..
            } => ProviderCause::Throttled {
                retry_after: *retry_after_ms,
                message: message.clone(),
            },
            Self::PlanExhausted { message, .. } => ProviderCause::PlanExhausted {
                message: message.clone(),
            },
            Self::Provider { message, .. } => ProviderCause::Other {
                code: 0,
                message: message.clone(),
            },
            _ => return None,
        });
        tracing::trace!(
            cause = ?out,
            "error::Error::provider_cause"
        );
        out
    }
}

/// Recovery-side categorization produced by
/// [`Error::provider_cause`]. The throttle governor consumes the
/// `Throttled` arm; the per-(provider, role) circuit breaker
/// consumes the `PlanExhausted` arm; everything else falls through
/// to the standard retry-budget path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCause {
    /// Persistent quota / plan exhaustion. Open the breaker per
    /// `(provider, role)`; cancel remaining retries.
    PlanExhausted {
        /// Verbatim upstream error message so post-mortem can
        /// correlate the trip with the request that triggered it.
        message: String,
    },
    /// Transient rate-limit (HTTP 429 with `Retry-After`). Reduce
    /// per-role concurrency and increase backoff via the
    /// [`crate::llm::governor::ThrottleGovernor`]; do **not** open
    /// the breaker.
    Throttled {
        /// `Retry-After` from the upstream response in milliseconds.
        /// `None` when the header was absent or unparseable.
        retry_after: Option<u64>,
        /// Verbatim upstream error message; surfaced for telemetry.
        message: String,
    },
    /// Generic provider fault (5xx, transport, parse). Recovery is
    /// the call-site retry budget; neither governor nor breaker
    /// reacts.
    Other {
        /// HTTP status code (`0` for transport / parse failures
        /// where no status line was parsed).
        code: u16,
        /// Verbatim upstream error message; surfaced for telemetry.
        message: String,
    },
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(IoError::Raw(e))
    }
}

impl From<IoError> for Error {
    fn from(e: IoError) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Provider {
            message: format!("sqlite: {e}"),
            http_status: None,
        }
    }
}

impl From<r2d2::Error> for Error {
    fn from(e: r2d2::Error) -> Self {
        Error::Provider {
            message: format!("sqlite pool: {e}"),
            http_status: None,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::SchemaViolation(format!("json: {e}"))
    }
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Error::InvalidArgs(format!("toml: {e}"))
    }
}

/// Structured I/O errors. Splits the raw `io::Error` from the
/// context-rich variants so callers can match precisely.
#[derive(Debug, Error)]
pub enum IoError {
    /// Unstructured `io::Error` from a stdlib call.
    #[error("io: {0}")]
    Raw(io::Error),

    /// Destination path had no parent directory.
    #[error("path has no parent: {path}")]
    NoParent {
        /// Path that lacked a parent.
        path: PathBuf,
    },

    /// `create_dir_all` failed.
    #[error("create dir {path}: {source}")]
    CreateDir {
        /// Directory that failed to be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `File::create` failed.
    #[error("create file {path}: {source}")]
    CreateFile {
        /// File that failed to be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// Write to file failed.
    #[error("write {path}: {source}")]
    Write {
        /// File that failed to be written.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `sync_all` failed.
    #[error("sync {path}: {source}")]
    Sync {
        /// File that failed to be synced.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `File::open` for parent directory failed.
    #[error("open dir {path}: {source}")]
    OpenDir {
        /// Directory that failed to open.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `rename` failed.
    #[error("rename {from} -> {to}: {source}")]
    Rename {
        /// Source path of the rename.
        from: PathBuf,
        /// Destination path of the rename.
        to: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// `fs::read` failed.
    #[error("read {path}: {source}")]
    Read {
        /// Path that was being read.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// Sidecar / config file failed to parse. Used by the profile
    /// loader (TOML) so the error travels with the offending path
    /// instead of bubbling through `Error::InvalidArgs`.
    #[error("parse {path}: {source}")]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying parse error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Sidecar metadata failed to serialize.
    #[error("serialize meta: {0}")]
    SerializeMeta(#[source] serde_json::Error),

    /// Sidecar metadata failed to deserialize.
    #[error("deserialize meta: {0}")]
    DeserializeMeta(#[source] serde_json::Error),

    /// Data file does not match its sidecar metadata.
    #[error("meta mismatch at {path}: expected {expected:?}, got {got:?}")]
    MetaMismatch {
        /// Path whose integrity is in question.
        path: PathBuf,
        /// What the sidecar advertises.
        expected: Box<ArtifactMeta>,
        /// What the data file actually contains.
        got: Box<ArtifactMeta>,
    },
}

/// Signal that work was cancelled cooperatively. Distinct from
/// `Error::Cancelled` so callers can match the structural signal.
#[derive(Debug, Error)]
#[error("cancel signal")]
pub struct CancelSignal;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Map the error to the documented exit code (T01-06 §12.3).
pub fn exit_code(err: &Error) -> u8 {
    let code = err.exit_code() as u8;
    tracing::trace!(?err, code, "error::exit_code");
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_discriminants_are_stable() {
        assert_eq!(ExitCode::Ok as i32, 0);
        assert_eq!(ExitCode::IoError as i32, 8);
        assert_eq!(ExitCode::ProviderError as i32, 40);
        assert_eq!(ExitCode::SigInt as i32, 130);
    }

    #[test]
    fn error_code_extended_variants_count() {
        let variants = [
            ErrorCode::Storage,
            ErrorCode::Llm,
            ErrorCode::Sandbox,
            ErrorCode::Research,
            ErrorCode::Resume,
            ErrorCode::Discovery,
        ];
        assert_eq!(variants.len(), 6);
        assert_eq!(
            variants.map(|code| ExitCode::from(code) as i32),
            [91, 92, 93, 94, 95, 96]
        );
    }

    #[test]
    fn exit_code_storage_maps_to_91() {
        assert_eq!(ExitCode::from(ErrorCode::Storage), ExitCode::Storage);
        assert_eq!(ExitCode::Storage as i32, 91);
    }

    #[test]
    fn exit_code_llm_maps_to_92() {
        assert_eq!(ExitCode::from(ErrorCode::Llm), ExitCode::Llm);
        assert_eq!(ExitCode::Llm as i32, 92);
    }

    #[test]
    fn error_exit_code_method_maps_baseline_variants() {
        assert_eq!(
            Error::Io(IoError::Raw(io::Error::other("x"))).exit_code(),
            ExitCode::IoError
        );
        assert_eq!(
            Error::InvalidArgs("x".into()).exit_code(),
            ExitCode::InvalidArgs
        );
        assert_eq!(
            Error::InvalidApiKey {
                message: "x".into(),
                http_status: None,
            }
            .exit_code(),
            ExitCode::ApiKeyInvalid
        );
        assert_eq!(
            Error::PlanExhausted {
                message: "x".into(),
                http_status: None,
            }
            .exit_code(),
            ExitCode::PlanExhausted
        );
        assert_eq!(
            Error::Timeout {
                message: "x".into(),
                http_status: None,
            }
            .exit_code(),
            ExitCode::Timeout
        );
        assert_eq!(
            Error::Cancelled("x".into()).exit_code(),
            ExitCode::Cancelled
        );
        assert_eq!(
            Error::SchemaViolation("x".into()).exit_code(),
            ExitCode::SchemaViolation
        );
    }

    #[test]
    fn error_exit_code_method_maps_extended_variants() {
        assert_eq!(
            Error::Provider {
                message: "x".into(),
                http_status: None,
            }
            .exit_code(),
            ExitCode::ProviderError
        );
        assert_eq!(Error::MockExhausted.exit_code(), ExitCode::ProviderError);
        assert_eq!(Error::Cache("x".into()).exit_code(), ExitCode::ContextError);
        assert_eq!(
            Error::InvalidState("x".into()).exit_code(),
            ExitCode::ContextError
        );
        assert_eq!(
            Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .exit_code(),
            ExitCode::ContextError
        );
    }
    #[test]
    fn compatibility_exit_code_function_returns_numeric_code() {
        assert_eq!(exit_code(&Error::InvalidArgs("x".into())), 2);
        assert_eq!(
            exit_code(&Error::InvalidApiKey {
                message: "x".into(),
                http_status: None,
            }),
            3
        );
        assert_eq!(
            exit_code(&Error::PlanExhausted {
                message: "x".into(),
                http_status: None,
            }),
            4
        );
        assert_eq!(
            exit_code(&Error::Timeout {
                message: "x".into(),
                http_status: None,
            }),
            5
        );
        assert_eq!(exit_code(&Error::Cancelled("x".into())), 6);
        assert_eq!(exit_code(&Error::Cancel(CancelSignal)), 6);
        assert_eq!(exit_code(&Error::SchemaViolation("x".into())), 7);
        assert_eq!(exit_code(&Error::MockExhausted), 40);
        assert_eq!(
            exit_code(&Error::Provider {
                message: "x".into(),
                http_status: None,
            }),
            40
        );
    }

    #[test]
    fn cancel_signal_display() {
        let s = format!("{}", CancelSignal);
        assert!(s.contains("cancel"));
    }

    /// `Error::code()` must return the canonical bucket for each
    /// variant. Pin every mapping so a refactor that re-routes a
    /// variant trips the test.
    #[test]
    fn code_maps_every_variant() {
        use std::io;

        assert_eq!(
            Error::Io(IoError::Raw(io::Error::other("x"))).code(),
            ErrorCode::Io
        );
        assert_eq!(
            Error::InvalidArgs("x".into()).code(),
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            Error::InvalidApiKey {
                message: "x".into(),
                http_status: None,
            }
            .code(),
            ErrorCode::Auth
        );
        assert_eq!(
            Error::PlanExhausted {
                message: "x".into(),
                http_status: None,
            }
            .code(),
            ErrorCode::QuotaExceeded
        );
        assert_eq!(
            Error::Timeout {
                message: "x".into(),
                http_status: None,
            }
            .code(),
            ErrorCode::TimeoutPhase
        );
        assert_eq!(Error::Cancelled("x".into()).code(), ErrorCode::Cancelled);
        assert_eq!(
            Error::SchemaViolation("x".into()).code(),
            ErrorCode::SchemaViolation
        );
        assert_eq!(
            Error::InvalidState("x".into()).code(),
            ErrorCode::InvalidState
        );
        assert_eq!(
            Error::Provider {
                message: "x".into(),
                http_status: None,
            }
            .code(),
            ErrorCode::InvalidResponse
        );
        assert_eq!(Error::MockExhausted.code(), ErrorCode::NeedsInput);
        assert_eq!(Error::Cache("x".into()).code(), ErrorCode::Io);
        assert_eq!(Error::Cancel(CancelSignal).code(), ErrorCode::Cancelled);
        assert_eq!(
            Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .code(),
            ErrorCode::InvalidState
        );
        // E10: hostile-prompt error maps to the dedicated bucket
        // so the post-execution review can branch on the wire form.
        assert_eq!(
            Error::HostilePrompt("ignore previous instructions".into()).code(),
            ErrorCode::HostilePrompt
        );
        // D.29.2: oversized payloads share the `InputTooLarge`
        // bucket so the wire form does not fork from the
        // proposal-03 intent (D.20.4 used `InputTooLarge`).
        assert_eq!(
            Error::PayloadTooLarge("response: 11000000 > 10485760".into()).code(),
            ErrorCode::InputTooLarge
        );
        // PR-5: a request that asked the model for a capability it
        // does not have (attachment on a non-attachment model, image
        // to a text-only model) maps to the `Unsupported` bucket so
        // the post-mortem review can branch on the wire form.
        assert_eq!(
            Error::ModalityUnsupported(
                "image attachment refused: model accepts [text] only".into()
            )
            .code(),
            ErrorCode::Unsupported
        );
        // K.4 sub-1: a research backend dependency (e.g.
        // `pdftotext` missing) maps to the dedicated `Research`
        // bucket so dashboards can branch on the wire form.
        assert_eq!(
            Error::ResearchUnavailable("pdftotext binary not found; install poppler-utils".into())
                .code(),
            ErrorCode::Research
        );
    }

    /// The code form must round-trip through serde unchanged so
    /// external tooling can decode the on-disk error log.
    #[test]
    fn code_serializes_to_screaming_snake_case() {
        let cases = [
            (Error::InvalidArgs("x".into()), "INVALID_ARGS"),
            (
                Error::InvalidApiKey {
                    message: "x".into(),
                    http_status: None,
                },
                "AUTH",
            ),
            (Error::Cancelled("x".into()), "CANCELLED"),
            (Error::SchemaViolation("x".into()), "SCHEMA_VIOLATION"),
            (Error::InvalidState("x".into()), "INVALID_STATE"),
            (Error::MockExhausted, "NEEDS_INPUT"),
        ];
        for (err, expected) in cases {
            let code = err.code();
            assert_eq!(code.stable(), expected, "code mismatch for {err:?}");
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json.trim_matches('"'), expected);
        }
    }

    /// Every code returned by `code()` must classify consistently:
    /// retriable codes are a subset of retriable-or-cancel; user
    /// errors (`InvalidArgs`, `InvalidState`) must never retriable.
    #[test]
    fn code_is_consistent_with_policy_helpers() {
        let variants = [
            Error::InvalidArgs("x".into()),
            Error::InvalidApiKey {
                message: "x".into(),
                http_status: None,
            },
            Error::PlanExhausted {
                message: "x".into(),
                http_status: None,
            },
            Error::Timeout {
                message: "x".into(),
                http_status: None,
            },
            Error::Cancelled("x".into()),
            Error::SchemaViolation("x".into()),
            Error::InvalidState("x".into()),
            Error::MockExhausted,
        ];
        for err in variants {
            let code = err.code();
            // Sanity: code must be one of the catalog variants.
            assert!(!code.stable().is_empty());
            // Cancelled codes must never be retriable.
            if matches!(err, Error::Cancelled(_) | Error::Cancel(_)) {
                assert!(!code.is_retriable(), "{code:?} must not be retriable");
            }
            // InvalidArgs and InvalidState must never be retriable
            // — they are operator / developer errors.
            if matches!(err, Error::InvalidArgs(_) | Error::InvalidState(_)) {
                assert!(!code.is_retriable(), "{code:?} must not be retriable");
            }
        }
    }

    /// `code()` must be `&self -> ErrorCode` (Copy), so the call
    /// site can pass the code by value without a clone.
    #[test]
    fn code_is_copy() {
        let err = Error::InvalidArgs("x".into());
        let a = err.code();
        let b = err.code();
        assert_eq!(a, b);
    }

    // --- is_circuit_opening -----------------------------------------
    // Pin every variant's classification so the breaker wrapper in
    // provider.rs (and any future caller) can rely on the policy
    // without re-deriving it.

    /// Openers cover the 5xx / auth / timeout set the breaker
    /// exists to catch. **v0.9.8**: `PlanExhausted` and `Throttled`
    /// are no longer openers — see `is_circuit_opening` doc-comment
    /// for the full rationale (the upstream returns the same
    /// 429 body for true quota exhaustion and for saturation;
    /// the throttle governor handles both).
    #[test]
    fn is_circuit_opening_classifies_openers() {
        assert!(
            Error::InvalidApiKey {
                message: "x".into(),
                http_status: None,
            }
            .is_circuit_opening()
        );
        assert!(
            Error::Provider {
                message: "x".into(),
                http_status: None,
            }
            .is_circuit_opening()
        );
        assert!(
            Error::Timeout {
                message: "x".into(),
                http_status: None,
            }
            .is_circuit_opening()
        );
    }

    /// `PlanExhausted` and `Throttled` (the two HTTP-429 buckets)
    /// must NOT open the breaker post-v0.9.8 — the throttle
    /// governor is the dedicated handler for those. The other
    /// non-openers are operator errors, contract mismatches, or
    /// local I/O — they do not indicate a provider outage.
    #[test]
    fn is_circuit_opening_classifies_non_openers() {
        assert!(!Error::SchemaViolation("x".into()).is_circuit_opening());
        assert!(!Error::InvalidArgs("x".into()).is_circuit_opening());
        assert!(!Error::InvalidState("x".into()).is_circuit_opening());
        assert!(!Error::Cancelled("x".into()).is_circuit_opening());
        assert!(!Error::Cancel(CancelSignal).is_circuit_opening());
        assert!(!Error::MockExhausted.is_circuit_opening());
        assert!(!Error::Io(IoError::Raw(io::Error::other("x"))).is_circuit_opening());
        assert!(!Error::Cache("x".into()).is_circuit_opening());
        assert!(!Error::NeedsInput("x".into()).is_circuit_opening());
        // v0.9.8: the 429 buckets are explicitly non-openers.
        assert!(
            !Error::PlanExhausted {
                message: "http 429: throttled".into(),
                http_status: Some(429),
            }
            .is_circuit_opening(),
            "v0.9.8: PlanExhausted must NOT trip the breaker; the throttle governor handles 429s"
        );
        assert!(
            !Error::Throttled {
                retry_after_ms: None,
                message: "throttled".into(),
                http_status: Some(429),
            }
            .is_circuit_opening(),
            "v0.9.8: Throttled must NOT trip the breaker; the throttle governor handles 429s"
        );
        assert!(
            !Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .is_circuit_opening()
        );
    }

    /// The opener set matches the [`ErrorCode::is_circuit_opening`]
    /// set for the unambiguous signals (5xx + Auth + transport),
    /// and the 429 buckets are explicitly NOT in the opener set
    /// even though `ErrorCode::Http429` says they should be — the
    /// keyword scan in `llm/http.rs::classify_status` cannot tell
    /// true quota exhaustion from saturation, and the throttle
    /// governor is the right tool for both.
    #[test]
    fn is_circuit_opening_aligns_with_error_code_policy() {
        // Unambiguous openers: keep the `Error::Provider` check
        // (covers Http500/502/503/504 upstream errors).
        assert!(ErrorCode::Http500.is_circuit_opening());
        assert!(
            Error::Provider {
                message: "http 500: boom".into(),
                http_status: None,
            }
            .is_circuit_opening(),
            "Error::Provider must trip the breaker (covers Http500/502/503/504 upstream errors)"
        );
        // Auth: 401/403 is unambiguous, the breaker should open.
        assert!(ErrorCode::Auth.is_circuit_opening());
        assert!(
            Error::InvalidApiKey {
                message: "http 401: bad".into(),
                http_status: None,
            }
            .is_circuit_opening(),
            "Error::InvalidApiKey must trip the breaker (covers Auth)"
        );
        // v0.9.8: 429 is an intentional divergence from
        // `ErrorCode::Http429::is_circuit_opening()`. The upstream
        // does not distinguish quota exhaustion from saturation,
        // so we let the throttle governor handle every 429. This
        // test documents the divergence so a future reader
        // doesn't "fix" it back without understanding the cost.
        assert!(ErrorCode::Http429.is_circuit_opening());
        assert!(
            !Error::PlanExhausted {
                message: "http 429: throttled".into(),
                http_status: Some(429),
            }
            .is_circuit_opening(),
            "v0.9.8 DIVERGENCE: ErrorCode::Http429 says trip, but Error::PlanExhausted does NOT — see is_circuit_opening doc-comment"
        );
    }

    // --- http_status ---------------------------------------------
    // Pin the accessor that the telemetry layer relies on for
    // `calls.http_status`. Without it, every error row ended up
    // with `http_status = NULL` and the operator had to parse the
    // message string to know whether the upstream returned 429,
    // 500, or just dropped the connection.

    #[test]
    fn http_status_returns_captured_code_for_http_transport_variants() {
        assert_eq!(
            Error::InvalidApiKey {
                message: "http 401: bad".into(),
                http_status: Some(401),
            }
            .http_status(),
            Some(401)
        );
        assert_eq!(
            Error::PlanExhausted {
                message: "http 429: throttled".into(),
                http_status: Some(429),
            }
            .http_status(),
            Some(429)
        );
        assert_eq!(
            Error::Throttled {
                retry_after_ms: Some(1000),
                message: "throttled".into(),
                http_status: Some(429),
            }
            .http_status(),
            Some(429)
        );
        assert_eq!(
            Error::Timeout {
                message: "http 504: gw".into(),
                http_status: Some(504),
            }
            .http_status(),
            Some(504)
        );
        assert_eq!(
            Error::Provider {
                message: "upstream 503: svc".into(),
                http_status: Some(503),
            }
            .http_status(),
            Some(503)
        );
    }

    #[test]
    fn http_status_returns_none_when_field_not_set() {
        // HTTP-transport variants with `http_status: None` (e.g. a
        // synthetic error constructed in tests, or a local failure
        // that bypassed the transport layer) must report `None`.
        assert_eq!(
            Error::InvalidApiKey {
                message: "synthetic".into(),
                http_status: None,
            }
            .http_status(),
            None
        );
        assert_eq!(
            Error::Provider {
                message: "network: ...".into(),
                http_status: None,
            }
            .http_status(),
            None
        );
    }

    #[test]
    fn http_status_returns_none_for_non_transport_variants() {
        // Variants that never carry an HTTP status (sqlite, cache,
        // schema, validation, cancellation) must report `None`
        // regardless of input.
        assert_eq!(Error::InvalidArgs("x".into()).http_status(), None);
        assert_eq!(Error::SchemaViolation("x".into()).http_status(), None);
        assert_eq!(Error::InvalidState("x".into()).http_status(), None);
        assert_eq!(Error::Cancelled("x".into()).http_status(), None);
        assert_eq!(Error::Cache("x".into()).http_status(), None);
        assert_eq!(
            Error::DiscoveryQualityTooLow {
                failed: 6,
                total: 10,
                threshold_pct: 50,
            }
            .http_status(),
            None
        );
        assert_eq!(Error::MockExhausted.http_status(), None);
    }
}
