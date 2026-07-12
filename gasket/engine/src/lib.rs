//! # Gasket Engine
//!
//! This crate serves as the **orchestration facade** for the gasket system.
//!
//! ## Architecture
//!
//! - **engine**: Orchestration layer (AgentSession, executor, tools, hooks)
//! - **core**: Convenience re-export layer (pub use of common types)
//! - **providers**: LLM provider abstractions
//! - **storage**: Event sourcing and persistence
//!
//! ## Design Principles
//!
//! - **Direct store refs**: Components hold `Arc<EventStore>` directly
//! - **Event sourcing**: All state changes persisted as events
//! - **Streaming-first**: SSE streaming with backpressure support

// NOTE: `agent/` module removed — migrated to `kernel/` + `session/` + `subagents/`
pub mod bootstrap;
pub mod config;
pub mod cron;
pub mod error;
pub mod external_tools;
pub mod heartbeat;
pub mod hooks;
pub mod kernel;
pub mod session;
pub mod skills;
pub mod subagents;
pub mod token_tracker;
pub mod tools;
pub mod vault;

// ── Root-level re-exports (used by external crates at crate root) ──

pub use error::ConfigValidationError;
pub use gasket_storage::JsonStore;
pub use gasket_types::SubagentSpawner;
pub use session::AgentConfig;
pub use subagents::ModelResolver;

// ── Facade re-exports (merged from gasket-core) ─────────────

// Providers
pub mod providers {
    pub use crate::config::app_config::ProviderRegistry;
    #[cfg(feature = "provider-anthropic")]
    pub use gasket_providers::build_anthropic_provider;
    #[cfg(feature = "provider-gemini")]
    pub use gasket_providers::build_gemini_provider;
    #[cfg(feature = "provider-minimax")]
    pub use gasket_providers::build_minimax_provider;
    #[cfg(feature = "provider-copilot")]
    pub use gasket_providers::CopilotProvider;
    #[cfg(feature = "provider-moonshot")]
    pub use gasket_providers::MoonshotProvider;
    pub use gasket_providers::{
        build_http_client, build_provider, parse_json_args, ChatMessage, ChatRequest, ChatResponse,
        ChatStream, ChatStreamChunk, ChatStreamDelta, FinishReason, FunctionCall,
        FunctionDefinition, LlmProvider, MessageRole, ModelSpec, OpenAICompatibleProvider,
        ProviderBuildError, ProviderConfig, ProviderResult, ThinkingConfig, ToolCall,
        ToolCallDelta, ToolDefinition, Usage,
    };
}

// Embedding (re-exported for CLI when feature is enabled)
#[cfg(feature = "embedding")]
pub mod embedding {
    pub use gasket_embedding::vector_store;
    pub use gasket_embedding::{
        EmbeddingIndexer, EmbeddingProvider, MemoryIndex, RecallConfig, RecallSearcher, VectorStore,
    };
}
