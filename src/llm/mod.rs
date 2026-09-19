//! LLM module: SDK-side client trait + request/response shapes, role
//! enum, capability matrix, mock implementation, cache, rate
//! limiter, circuit breaker, auto-probe subsystem, and the
//! versioned prompt registry.

pub mod api_keys;
pub mod api_keys_file;
pub mod cache;
pub mod capabilities;
pub mod capability;
pub mod circuit_breaker;
pub mod client;
pub mod control_tokens;
pub mod cost;
pub mod embed;
pub mod governor;
pub mod http;
pub mod json_extractor;
pub mod json_strategy;
pub mod modal_gate;
pub mod models_dev;
pub mod param_rejections;
pub mod probe;
pub mod probe_table;
pub mod prompt_cache;
pub mod prompts;
pub mod rate_limiter;
pub mod response_format_opt_out;
pub mod retry_budget;
pub mod role;
pub mod size_limits;
pub mod sse_parser;
pub mod temperature_probe;
pub mod top_k_probe;
pub mod top_p_probe;

pub use client::omit_param_llm as omit_param;
pub use client::{
    AnthropicClient, BreakeredClient, LlmCapabilities, LlmClient, LlmClientRegistry, LlmError,
    LlmRequest, LlmResponse, MockClient, MockResponse, OpenAIClient, OpenAIVariant,
    ProviderRegistry, Request, Response, SdkKind, Usage, WireFormatId, registry_key,
};
pub use client::{Attachment, CacheHashAlgo, CallRecord, Message, ToolChoice, build_cache_key};
// Pre-#933 compat re-exports. Kept so legacy call sites
// (`crate::llm::Provider`, `crate::llm::BreakeredProvider`,
// `crate::llm::provider::attach_parallelism_rate_limit`, …) keep
// compiling while the migration moves forward. The compat
// shims are deprecated and will be removed in a follow-up PR.
#[allow(deprecated)]
pub use client::compat::{
    BreakeredProvider, Provider, RateLimitConfig, Response as CompatResponse, SaturationEvent,
    SaturationSink, attach_parallelism_rate_limit, registry_from_config,
    registry_from_config_with_home_and_sink, registry_from_config_with_sink,
    registry_from_config_with_sink_active, wire_format_id,
};
/// Pre-#933 `crate::llm::provider` module shim. The legacy module
/// was deleted in #933; the symbols here route to the post-#933
/// surface through [`crate::llm::client::compat`].
#[allow(deprecated)]
pub mod provider {
    pub use crate::llm::client::compat::Provider as ProviderTrait;
    pub use crate::llm::client::compat::{
        BreakeredProvider, Provider, ProviderRegistry, RateLimitConfig, SaturationSink,
        attach_parallelism_rate_limit, registry_from_config,
        registry_from_config_with_home_and_sink, registry_from_config_with_sink,
        registry_from_config_with_sink_active, wire_format_id,
    };
}

// Pre-#933 module re-exports. The legacy `MockProvider`,
// `MinimaxProvider`, `DeepSeekProvider`, `AnthropicCompatProvider`,
// `OpenAICompatProvider`, and `OpenAICompatibleProvider` concrete
// impls were deleted as part of #933. The SDK impls in
// `client::*` cover all the cases the legacy impls handled —
// tests that named a concrete `*Provider` impl should migrate to
// `crate::llm::client::MockClient` for the role they previously
// played. Kept as `mod` stubs with deprecated type aliases so
// legacy `use crate::llm::minimax::MinimaxProvider;` paths still
// resolve (the modules compile to a deprecation warning).
#[allow(missing_docs)]
#[deprecated(note = "post-#933 module deleted; use crate::llm::client::MockClient for tests")]
pub mod mock {
    #[deprecated(note = "post-#933 alias for crate::llm::client::MockClient")]
    pub type MockProvider = crate::llm::client::MockClient;
    #[deprecated(note = "post-#933 alias for crate::llm::client::MockResponse")]
    pub type MockResponse = crate::llm::client::MockResponse;
}
#[allow(missing_docs)]
#[deprecated(note = "post-#933 module deleted; use crate::llm::client::MockClient")]
pub mod minimax {
    #[deprecated(note = "post-#933 alias for crate::llm::client::MockClient")]
    pub type MinimaxProvider = crate::llm::client::MockClient;
}
#[allow(missing_docs)]
#[deprecated(note = "post-#933 module deleted; SDK impls in crate::llm::client::*")]
pub mod deepseek {
    #[deprecated(note = "post-#933 alias for crate::llm::client::MockClient")]
    pub type DeepSeekProvider = crate::llm::client::MockClient;
}
#[allow(missing_docs)]
#[deprecated(note = "post-#933 module deleted; SDK impls in crate::llm::client::*")]
pub mod anthropic_compat {
    #[deprecated(note = "post-#933 alias for crate::llm::client::AnthropicClient")]
    pub type AnthropicCompatProvider = crate::llm::client::AnthropicClient;
}
#[allow(missing_docs)]
#[deprecated(note = "post-#933 module deleted; SDK impls in crate::llm::client::*")]
pub mod openai_compat {
    #[deprecated(note = "post-#933 alias for crate::llm::client::OpenAIClient")]
    pub type OpenAICompatProvider = crate::llm::client::OpenAIClient;
}
#[allow(missing_docs)]
#[deprecated(note = "post-#933 module deleted; SDK impls in crate::llm::client::*")]
pub mod openai_compatible {
    #[deprecated(note = "post-#933 alias for crate::llm::client::OpenAIClient")]
    pub type OpenAICompatibleProvider = crate::llm::client::OpenAIClient;
    #[deprecated(note = "post-#933 shim")]
    pub use crate::llm::client::compat::into_legacy_pair as wire_format_pair;
}
pub use models_dev::{
    CATALOG_FILE_NAME, CATALOG_SCHEMA_VERSION, CatalogLoad, Cost, DEFAULT_REFRESH_HOURS,
    InterleavedField, Limits, MODELS_DEV_URL, Modalities, ModelsDevCatalog, ModelsDevEntry,
    ModelsDevProvider, ReasoningOption,
};
pub use role::Role;
pub use top_k_probe::{Entry as TopKEntry, TopKTable, TopKTableFile};
pub use top_p_probe::{Entry as TopPEntry, TopPTable, TopPTableFile};
