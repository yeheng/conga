//! Session builder — composable construction of AgentSession services.
//!
//! Replaces the monolithic `with_services` constructor with a clean builder.
//! All intermediate services are constructed inside `build()` as local variables —
//! no partial initialization, no `Option` fields, no `expect()` panics.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::warn;

use crate::error::AgentError;
use crate::kernel::RuntimeContext;
use crate::session::compactor::ContextCompactor;
use crate::session::config::AgentConfigExt;

use crate::session::finalizer::ResponseFinalizer;
use crate::session::{prompt, AgentConfig, AgentSession};
use gasket_providers::LlmProvider;
use gasket_storage::{EventStore, SessionStore};

/// Bundle of embedding-specific dependencies for session construction.
///
/// All three fields are required: the broadcast sender is **not** optional —
/// it is the channel the indexer listens on, so omitting it would silently
/// detach the indexer from event flow. Callers that don't want embedding
/// indexing simply don't build a session with embedding at all.
#[cfg(feature = "embedding")]
pub struct EmbeddingContext {
    pub searcher: Arc<gasket_embedding::RecallSearcher>,
    pub indexer: gasket_embedding::EmbeddingIndexer,
    pub event_store_tx: tokio::sync::broadcast::Sender<gasket_types::SessionEvent>,
}

/// Wiki preparation prompt appended to system prompt when wiki is enabled.
///
/// Instructs the agent to proactively query wiki via tools before responding,
/// replacing the old automatic context injection mechanism.
const WIKI_PREPARATION_PROMPT: &str = "\
## Wiki Knowledge System

You have access to a personal wiki knowledge base via these tools:
- `wiki_search(query)`: Search wiki pages by keyword
- `wiki_read(path)`: Read a specific wiki page by path
- `wiki_write(path, title, content)`: Create or update a wiki page

**Preparation Protocol**: Before responding to any user query, always use `wiki_search` \
to check if relevant knowledge already exists in the wiki. This ensures your responses \
build upon accumulated knowledge rather than starting from scratch.";

/// Builder for constructing an `AgentSession`.
///
/// Holds only the external inputs; all internal services are built locally
/// inside `build()` in the correct dependency order.
pub struct SessionBuilder {
    provider: Arc<dyn LlmProvider>,
    workspace: PathBuf,
    config: AgentConfig,
    tools: Arc<crate::tools::ToolRegistry>,
    sqlite_store: Arc<gasket_storage::SqliteStore>,
    /// Optional semantic recall searcher + indexer (embedding feature).
    #[cfg(feature = "embedding")]
    embedding_recall: Option<(
        Arc<gasket_embedding::RecallSearcher>,
        gasket_embedding::EmbeddingIndexer,
    )>,
    /// Optional shared broadcast sender so the AgentSession's EventStore
    /// shares the same channel as the embedding infrastructure.
    #[cfg(feature = "embedding")]
    event_store_tx: Option<tokio::sync::broadcast::Sender<gasket_types::SessionEvent>>,
}

impl SessionBuilder {
    /// Start building a session with required dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        workspace: PathBuf,
        config: AgentConfig,
        tools: Arc<crate::tools::ToolRegistry>,
        sqlite_store: Arc<gasket_storage::SqliteStore>,
    ) -> Self {
        Self {
            provider,
            workspace,
            config,
            tools,
            sqlite_store,
            #[cfg(feature = "embedding")]
            embedding_recall: None,
            #[cfg(feature = "embedding")]
            event_store_tx: None,
        }
    }

    /// Attach embedding recall infrastructure (searcher + indexer).
    /// Required for semantic history recall and the `history_search` tool.
    #[cfg(feature = "embedding")]
    pub fn with_embedding_recall(
        mut self,
        searcher: Arc<gasket_embedding::RecallSearcher>,
        indexer: gasket_embedding::EmbeddingIndexer,
    ) -> Self {
        self.embedding_recall = Some((searcher, indexer));
        self
    }

    /// Share the broadcast sender from an external EventStore so that
    /// the AgentSession's EventStore and the embedding indexer listen
    /// on the same channel.
    #[cfg(feature = "embedding")]
    pub fn with_event_store_tx(
        mut self,
        tx: tokio::sync::broadcast::Sender<gasket_types::SessionEvent>,
    ) -> Self {
        self.event_store_tx = Some(tx);
        self
    }

    /// Build the complete `AgentSession`.
    ///
    /// All services are constructed in dependency order as local variables —
    /// the compiler guarantees every value is initialized before use.
    pub async fn build(self) -> Result<AgentSession, AgentError> {
        // ── 1. Storage layer ─────────────────────────────────────────
        let pool = self.sqlite_store.pool();
        let session_store = SessionStore::new(pool.clone());
        #[cfg(feature = "embedding")]
        let event_store = if let Some(tx) = self.event_store_tx {
            EventStore::with_pool_and_sender(pool.clone(), tx)
        } else {
            EventStore::new(pool.clone())
        };
        #[cfg(not(feature = "embedding"))]
        let event_store = EventStore::new(pool.clone());

        // ── 2. Query provider for real model limits ──────────────────
        let model_limits = self
            .provider
            .model_limits(&self.config.model)
            .await
            .ok()
            .flatten();
        let effective_max_tokens = if let Some(ref limits) = model_limits {
            let capped = self.config.max_tokens.min(limits.max_output_tokens as u32);
            if capped != self.config.max_tokens {
                tracing::info!(
                    "[SessionBuilder] Limiting max_tokens from {} to {} (model cap)",
                    self.config.max_tokens,
                    capped
                );
            }
            capped
        } else {
            self.config.max_tokens
        };

        // ── 3. Kernel runtime context ────────────────────────────────
        let mut kernel_config = self.config.to_kernel_config();
        kernel_config.max_tokens = effective_max_tokens;
        let pending_asks = Arc::new(crate::session::PendingAskRegistryImpl::new());

        let runtime_ctx = RuntimeContext {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            config: kernel_config,
            role: gasket_types::AgentRole::Orchestrator,
            checkpoint_callback: None,
            refs: gasket_types::SessionRefs {
                spawner: None,
                token_tracker: None,
                session_key: None,
                outbound_tx: None,
                aggregator_cancel: None,
                pending_asks: Some(
                    pending_asks.clone() as gasket_types::pending_ask::DynPendingAskRegistry
                ),
                synthesis_callback: None,
            },
        };
        // ── 4. Context compactor ─────────────────────────────────────
        let mut history_config = gasket_storage::HistoryConfig {
            max_events: self.config.memory_window,
            ..Default::default()
        };
        if let Some(ref limits) = model_limits {
            let capped = history_config.token_budget.min(limits.max_input_tokens);
            if capped != history_config.token_budget {
                tracing::info!(
                    "[SessionBuilder] Limiting token_budget from {} to {} (model cap)",
                    history_config.token_budget,
                    capped
                );
            }
            history_config.token_budget = capped;
        }
        let pending_done = tokio_util::task::TaskTracker::new();

        let mut compactor = ContextCompactor::new(
            self.provider.clone(),
            event_store.clone(),
            session_store.clone(),
            self.config.model.clone(),
            history_config.token_budget,
        )
        .with_cooldown_secs(self.config.compaction_cooldown_secs)
        .with_task_tracker(pending_done.clone());
        if let Some(ref prompt_text) = self.config.prompts.summarization {
            compactor = compactor.with_summarization_prompt(prompt_text.clone());
        }
        let mut checkpoint_config = crate::session::compactor::CheckpointConfig::default();
        if let Some(ref prompt_text) = self.config.prompts.checkpoint {
            checkpoint_config.prompt = prompt_text.clone();
        }
        compactor = compactor.with_checkpoint_config(checkpoint_config);
        let compactor = Some(Arc::new(compactor));

        // ── 6. System prompt and skills (merged) ─────────────────────
        let mut system_prompt = match prompt::load_system_prompt(
            &self.workspace,
            prompt::BOOTSTRAP_FILES_FULL,
            self.config.prompts.identity_prefix.as_deref(),
        )
        .await
        {
            Ok(sp) => sp,
            Err(e) => {
                warn!("Failed to load system prompt: {}", e);
                String::new()
            }
        };
        if let Some(skills) = prompt::load_skills_context(&self.workspace).await {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&skills);
        }

        // ── 7. Wiki availability check (prompt only) ──────────────
        if is_wiki_available(&self.config) {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(WIKI_PREPARATION_PROMPT);
        }

        // ── 8. Hook registry ─────────────────────────────────────────
        let hooks = crate::session::history::builder::build_default_hooks_builder().build_shared();

        #[cfg(feature = "embedding")]
        let embedding_indexer = self.embedding_recall.map(|(_, indexer)| indexer);

        // ── 9. ContextBuilder — encapsulates all pipeline dependencies ──
        let context_builder = crate::session::history::builder::ContextBuilder::new(
            event_store,
            session_store,
            system_prompt,
            None,
            hooks,
            history_config,
        );

        // ── 10. Wiki PageStore for finalizer (index.md rebuild) ──
        let wiki_root = self.workspace.join("wiki");
        let page_store = if wiki_root.exists() {
            Some(crate::wiki::PageStore::new(pool.clone(), wiki_root))
        } else {
            None
        };

        let finalizer = ResponseFinalizer::new(
            context_builder.hooks().clone(),
            context_builder.event_store().clone(),
            compactor.clone(),
            None,
            effective_max_tokens,
            self.config.after_response_hook_timeout_secs,
            page_store,
        );

        let mut config = self.config;
        config.max_tokens = effective_max_tokens;
        let initial_model = config.model.clone();

        Ok(AgentSession {
            runtime_ctx,
            active_model: parking_lot::Mutex::new(initial_model),
            context_builder,
            compactor,
            pricing: None,
            pipeline: crate::session::pipeline::RequestPipeline::new(finalizer, pending_done),
            pending_asks,
            #[cfg(feature = "embedding")]
            embedding_indexer,
        })
    }
}

// ---------------------------------------------------------------------------
// Internal helpers — pure functions, no builder state
// ---------------------------------------------------------------------------

/// Check if wiki is configured and minimally available.
///
/// Returns true if wiki config is enabled and the base path exists.
/// Does NOT initialize PageStore/PageIndex — that happens
/// during tool registration in `tools/builder.rs`.
fn is_wiki_available(config: &AgentConfig) -> bool {
    config
        .wiki
        .as_ref()
        .is_some_and(|cfg| cfg.enabled && std::path::Path::new(&cfg.base_path).exists())
}

/// Build an AgentSession with all services initialized.
pub async fn build_session(
    provider: Arc<dyn LlmProvider>,
    workspace: PathBuf,
    config: AgentConfig,
    tools: Arc<crate::tools::ToolRegistry>,
    sqlite_store: Arc<gasket_storage::SqliteStore>,
) -> Result<AgentSession, AgentError> {
    SessionBuilder::new(provider, workspace, config, tools, sqlite_store)
        .build()
        .await
}

/// Build an AgentSession with embedding recall support.
#[cfg(feature = "embedding")]
pub async fn build_session_with_embedding(
    provider: Arc<dyn LlmProvider>,
    workspace: PathBuf,
    config: AgentConfig,
    tools: Arc<crate::tools::ToolRegistry>,
    sqlite_store: Arc<gasket_storage::SqliteStore>,
    embedding: EmbeddingContext,
) -> Result<AgentSession, AgentError> {
    SessionBuilder::new(provider, workspace, config, tools, sqlite_store)
        .with_embedding_recall(embedding.searcher, embedding.indexer)
        .with_event_store_tx(embedding.event_store_tx)
        .build()
        .await
}
