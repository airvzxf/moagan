//! LLM module: provider trait, role enum, wire types, mock + minimax
//! implementations, cache, rate limiter, circuit breaker, and the
//! versioned prompt registry.

pub mod anthropic_compat;
pub mod api_keys;
pub mod api_keys_file;
pub mod budget;
pub mod cache;
pub mod capabilities;
pub mod capability;
pub mod circuit_breaker;
pub mod control_tokens;
pub mod deepseek;
pub mod embed;
pub mod http;
pub mod json_extractor;
pub mod json_strategy;
pub mod minimax;
pub mod mock;
pub mod models_dev;
pub mod openai_compat;
pub mod opencode_go;
pub mod opencode_go_anthropic;
pub mod opencode_go_responses;
pub mod probe;
pub mod probe_table;
pub mod prompt_cache;
pub mod prompts;
pub mod provider;
pub mod provider_pool;
pub mod rate_limiter;
pub mod response_format_opt_out;
pub mod retry_budget;
pub mod role;
pub mod size_limits;
pub mod sse_parser;
pub mod streaming;
pub mod wire;
pub mod wire_format;

pub use mock::{MockProvider, MockResponse};
pub use models_dev::{
    CATALOG_FILE_NAME, CATALOG_SCHEMA_VERSION, CatalogLoad, Cost, DEFAULT_REFRESH_HOURS,
    InterleavedField, Limits, MODELS_DEV_URL, Modalities, ModelsDevCatalog, ModelsDevEntry,
    ModelsDevProvider, ReasoningOption,
};
pub use provider::{Provider, ProviderRegistry, registry_from_config};
pub use provider_pool::{ProviderPool, ProviderPoolEntry};
pub use role::Role;
pub use wire::{CallRecord, Request, Response, Usage};

pub mod registry {
    //! Re-export of the prompt registry helper so callers can do
    //! `moagan::llm::registry::prompt_set_hash()`.
    pub use super::prompts::prompt_set_hash;
}
