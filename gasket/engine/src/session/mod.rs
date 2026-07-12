//! Session management layer — wraps the kernel with stateful lifecycle.
//!
//! AgentSession owns session state (events, prompts, memory, compaction)
//! and handles per-turn request orchestration via `process_turn`.
//! The preprocess → execute → postprocess flow is inlined into a single
//! async function for clarity — no trait objects, no cross-file state搬运.

pub mod builder;
pub mod compactor;
pub mod config;
pub mod finalizer;
pub mod history;
pub mod pending_ask;
pub mod prompt;
pub mod skills_loader;

pub use compactor::{ContextCompactor, UsageStats, WatermarkInfo};
pub use config::{AgentConfig, EvolutionConfig};
pub use pending_ask::PendingAskRegistryImpl;
pub use skills_loader::{find_builtin_skills_dir, load_skills};

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{info, warn};

use crate::error::AgentError;
use crate::hooks::HookRegistry;
use crate::kernel::{self, ExecutionResult, RuntimeContext, StreamEvent};
use crate::session::finalizer::ResponseFinalizer;
use crate::session::history::builder::BuildOutcome;
use crate::token_tracker::ModelPricing;
use futures_util::StreamExt;
use crate::tools::{SubagentSpawner, ToolRegistry};
use async_trait::async_trait;
use gasket_types::events::ChatEvent;
use gasket_types::pending_ask::PendingAskRegistry;
use gasket_types::SessionKey;

/// Outcome of `handle_inbound`.
pub enum HandleOutcome {
    /// Inbound was consumed by a pending `ask_user`. No reply emitted.
    Consumed,
    /// Inbound triggered a normal LLM turn; consumer can stream events and
    /// await the result.
    Replied {
        events: tokio::sync::mpsc::Receiver<gasket_types::events::ChatEvent>,
        result: tokio::task::JoinHandle<Result<AgentResponse, AgentError>>,
    },
}

/// Response from agent processing
#[derive(Debug, Clone)]
pub struct AgentResponse {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tools_used: Vec<String>,
    pub model: Option<String>,
    pub token_usage: Option<gasket_types::TokenUsage>,
    pub cost: f64,
}

impl AgentResponse {
    /// Create from a kernel `ExecutionResult` + resolved model name.
    ///
    /// Cost is initialized to 0 — the finalizer calculates actual cost from pricing.
    pub(crate) fn from_execution(result: ExecutionResult, model: Option<String>) -> Self {
        Self {
            content: result.content,
            reasoning_content: result.reasoning_content,
            tools_used: result.tools_used,
            model,
            token_usage: result.token_usage,
            cost: 0.0,
        }
    }
}

impl From<AgentResponse> for gasket_types::SubagentResponse {
    fn from(r: AgentResponse) -> Self {
        Self {
            content: r.content,
            reasoning_content: r.reasoning_content,
            tools_used: r.tools_used,
            model: r.model,
            token_usage: r.token_usage,
            cost: r.cost,
        }
    }
}

/// Owned snapshot for post-response finalization.
#[derive(Clone)]
pub(crate) struct FinalizeContext {
    session_key: SessionKey,
    session_key_str: String,
    content: String,
    local_vault_values: Vec<String>,
    estimated_tokens: usize,
}

impl FinalizeContext {
    /// Minimal constructor — used by abort path and as a base for `from_request`.
    fn new(session_key: &SessionKey, content: &str) -> Self {
        Self {
            session_key: session_key.clone(),
            session_key_str: session_key.to_string(),
            content: content.to_string(),
            local_vault_values: vec![],
            estimated_tokens: 0,
        }
    }

    fn from_request(req: &history::builder::ChatRequest) -> Self {
        let session_key = SessionKey::parse(&req.session_key)
            .unwrap_or_else(|| SessionKey::new(gasket_types::ChannelType::Cli, &req.session_key));
        let mut ctx = Self::new(&session_key, &req.user_content);
        ctx.local_vault_values = req.vault_values.clone();
        ctx.estimated_tokens = req.estimated_tokens;
        ctx
    }
}

/// Async checkpoint callback implementation for AgentSession.
///
/// Bridges the kernel's checkpoint hook to the session's compactor,
/// eliminating the need for `block_in_place` + `block_on` hacks.
pub(crate) struct SessionCheckpointCallback {
    session_key: SessionKey,
    compactor: Arc<ContextCompactor>,
    event_store: Arc<dyn gasket_storage::EventStoreTrait>,
}

impl SessionCheckpointCallback {
    pub(crate) fn new(
        session_key: SessionKey,
        compactor: Arc<ContextCompactor>,
        event_store: Arc<dyn gasket_storage::EventStoreTrait>,
    ) -> Self {
        Self {
            session_key,
            compactor,
            event_store,
        }
    }
}

#[async_trait]
impl crate::kernel::CheckpointCallback for SessionCheckpointCallback {
    async fn get_checkpoint(&self, msg_len: usize) -> Option<String> {
        // Only check after a minimum number of messages to avoid
        // checkpoint noise at the start of a conversation.
        if msg_len < 3 {
            return None;
        }
        let max_seq = match self.event_store.get_max_sequence(&self.session_key).await {
            Ok(seq) => seq,
            Err(e) => {
                tracing::debug!("Checkpoint: get_max_sequence failed: {}", e);
                return None;
            }
        };
        match self.compactor.checkpoint(&self.session_key, max_seq).await {
            Ok(Some(summary)) => {
                tracing::info!(
                    "Checkpoint injected for {} at seq {} ({} chars)",
                    self.session_key,
                    max_seq,
                    summary.len()
                );
                Some(summary)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("Checkpoint generation failed: {}", e);
                None
            }
        }
    }

    async fn save_ask_checkpoint(
        &self,
        messages: &[gasket_providers::ChatMessage],
        pending_question: &str,
    ) -> Result<(), String> {
        self.compactor
            .save_ask_checkpoint(&self.session_key, messages, pending_question)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Stateful session management — wraps the kernel, adds session lifecycle.
///
/// Owns session-level state (events, prompts, compaction, cost tracking) and
/// handles per-turn request orchestration via `process_turn` — a single
/// async function that inlines preprocess → execute → postprocess.
/// No trait objects, no cross-file state搬运, no indirection.
#[allow(unused_variables)]
pub struct AgentSession {
    runtime_ctx: RuntimeContext,
    /// Mutable model name — supports runtime switching via `/model <id>`.
    /// Read on every request via `model()`, written by `switch_model()`.
    active_model: parking_lot::Mutex<String>,
    context_builder: history::builder::ContextBuilder,
    compactor: Option<Arc<ContextCompactor>>,
    /// Pricing config for cost tracking. None when cost tracking is disabled.
    pricing: Option<ModelPricing>,
    /// Response finalizer — invoked after each kernel execution to persist
    /// events, trigger compaction, and calculate cost.
    finalizer: ResponseFinalizer,
    /// TaskTracker so that `graceful_shutdown` can await in-flight turns.
    pending_done: tokio_util::task::TaskTracker,
    /// Pending-ask registry shared with tools through `RuntimeContext`.
    pending_asks: Arc<PendingAskRegistryImpl>,
    /// RAII guard for the background embedding indexer.
    /// Held alive for the session lifetime; `Drop` cancels the background task.
    #[cfg(feature = "embedding")]
    embedding_indexer: Option<gasket_embedding::EmbeddingIndexer>,
}

impl AgentSession {
    /// Create a new session with default services.
    pub async fn new(
        provider: Arc<dyn gasket_providers::LlmProvider>,
        workspace: PathBuf,
        config: AgentConfig,
        tools: Arc<ToolRegistry>,
    ) -> Result<Self, AgentError> {
        let store = Arc::new(gasket_storage::JsonStore::new(gasket_storage::config_dir()));
        Self::with_store(provider, workspace, config, tools, store).await
    }

    /// Create a session with a custom JsonStore.
    pub async fn with_store(
        provider: Arc<dyn gasket_providers::LlmProvider>,
        workspace: PathBuf,
        config: AgentConfig,
        tools: Arc<ToolRegistry>,
        store: Arc<gasket_storage::JsonStore>,
    ) -> Result<Self, AgentError> {
        builder::build_session(provider, workspace, config, tools, store).await
    }

    /// Create a session with embedding recall support.
    #[cfg(feature = "embedding")]
    pub async fn with_store_and_embedding(
        provider: Arc<dyn gasket_providers::LlmProvider>,
        workspace: PathBuf,
        config: AgentConfig,
        tools: Arc<ToolRegistry>,
        store: Arc<gasket_storage::JsonStore>,
        embedding: builder::EmbeddingContext,
    ) -> Result<Self, AgentError> {
        builder::build_session_with_embedding(
            provider,
            workspace,
            config,
            tools,
            store,
            embedding,
        )
        .await
    }

    /// Access the pending-ask registry (for wiring into the subagent spawner).
    pub fn pending_asks(&self) -> gasket_types::pending_ask::DynPendingAskRegistry {
        self.pending_asks.clone() as gasket_types::pending_ask::DynPendingAskRegistry
    }

    /// Set the subagent spawner.
    pub fn with_spawner(mut self, spawner: Arc<dyn SubagentSpawner>) -> Self {
        self.runtime_ctx.refs.spawner = Some(spawner);
        self
    }

    /// Set the token tracker.
    pub fn with_token_tracker(mut self, tracker: Arc<crate::token_tracker::TokenTracker>) -> Self {
        self.runtime_ctx.refs.token_tracker = Some(tracker);
        self
    }

    /// Attach cost-tracking with the given pricing config.
    pub fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    /// Access the tool registry.
    pub fn tools(&self) -> Arc<ToolRegistry> {
        self.runtime_ctx.tools.clone()
    }

    /// Get the active model name.
    pub fn model(&self) -> String {
        self.active_model.lock().clone()
    }

    /// Get the hook registry.
    pub fn hooks(&self) -> &Arc<HookRegistry> {
        self.context_builder.hooks()
    }

    /// Clear session for the given key.
    pub async fn clear_session(&self, session_key: &SessionKey) {
        match self
            .context_builder
            .event_store()
            .clear_session(session_key)
            .await
        {
            Ok(_) => info!("Session '{}' cleared", session_key),
            Err(e) => warn!("Failed to clear session '{}': {}", session_key, e),
        }
    }

    /// List recent sessions ordered by last activity.
    ///
    /// Queries the session store for all sessions with at least one event.
    pub async fn list_sessions(&self) -> Vec<gasket_types::SessionSummary> {
        match self
            .context_builder
            .session_store()
            .scan_active_sessions()
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(key_str, count, updated_at)| {
                    let key = SessionKey::parse(&key_str)?;
                    let last_active = chrono::DateTime::parse_from_rfc3339(&updated_at)
                        .ok()
                        .map(|dt| dt.with_timezone(&chrono::Utc));
                    Some(gasket_types::SessionSummary {
                        key,
                        message_count: count as usize,
                        last_active,
                    })
                })
                .collect(),
            Err(e) => {
                warn!("Failed to list sessions: {}", e);
                Vec::new()
            }
        }
    }

    /// Switch the active model for the session.
    ///
    /// Updates the model used in subsequent LLM calls. Returns previous and
    /// current model IDs on success.
    pub async fn switch_model(&self, new: &str) -> Result<gasket_types::ModelSwitchInfo, String> {
        let mut guard = self.active_model.lock();
        let previous = guard.clone();
        *guard = new.to_string();
        drop(guard);
        Ok(gasket_types::ModelSwitchInfo {
            previous,
            current: new.to_string(),
        })
    }

    /// Force-trigger context compaction.
    pub fn force_compact(&self, session_key: &SessionKey, vault_values: &[String]) -> bool {
        self.compactor
            .as_ref()
            .is_some_and(|c| c.force_compact(session_key, vault_values))
    }

    /// Force-trigger context compaction and await completion.
    pub async fn force_compact_and_wait(
        &self,
        session_key: &SessionKey,
        vault_values: &[String],
    ) -> Result<(), AgentError> {
        match self.compactor.as_ref() {
            Some(c) => c
                .force_compact_and_wait(session_key, vault_values)
                .await
                .map_err(|e| AgentError::SessionError(e.to_string())),
            None => Err(AgentError::SessionError(
                "No compactor available".to_string(),
            )),
        }
    }

    /// Check if context compaction is currently in progress.
    pub fn is_compacting(&self) -> bool {
        self.compactor.as_ref().is_some_and(|c| c.is_compressing())
    }

    /// Get context usage statistics.
    pub async fn get_context_stats(
        &self,
        session_key: &SessionKey,
    ) -> Option<crate::session::compactor::UsageStats> {
        match self.compactor.as_ref() {
            Some(c) => c.get_usage_stats(session_key).await.ok(),
            None => None,
        }
    }

    /// Get watermark information.
    pub async fn get_watermark_info(
        &self,
        session_key: &SessionKey,
    ) -> Option<crate::session::compactor::WatermarkInfo> {
        match self.compactor.as_ref() {
            Some(c) => c.get_watermark_info(session_key).await.ok(),
            None => None,
        }
    }

    /// Gracefully shut down the session, awaiting all in-flight finalization tasks.
    ///
    /// Call this before dropping the session or shutting down the gateway to ensure
    /// all pending `finalize_response` calls complete. This prevents data loss where
    /// an assistant message has been generated but not yet persisted to the EventStore.
    pub async fn graceful_shutdown(&self) {
        self.pending_done.close();
        if !self.pending_done.is_empty() {
            info!(
                "Graceful shutdown: awaiting {} pending finalization task(s)",
                self.pending_done.len()
            );
        }
        self.pending_done.wait().await;
    }

    /// Inbound entry: try to deliver to a pending ask first, otherwise run
    /// the streaming pipeline.
    pub async fn handle_inbound(
        &self,
        content: &str,
        session_key: &SessionKey,
        tool_filter: Option<Vec<String>>,
    ) -> Result<HandleOutcome, AgentError> {
        let synthetic = gasket_types::events::InboundMessage {
            channel: session_key.channel.clone(),
            sender_id: session_key.chat_id.clone(),
            chat_id: session_key.chat_id.clone(),
            content: content.to_string(),
            media: None,
            metadata: None,
            timestamp: chrono::Utc::now(),
            trace_id: None,
        };
        if self
            .pending_asks
            .try_fulfill(session_key, synthetic)
            .is_ok()
        {
            return Ok(HandleOutcome::Consumed);
        }
        let (events, result) = self
            .process_direct_streaming_with_channel(content, session_key, tool_filter)
            .await?;
        Ok(HandleOutcome::Replied { events, result })
    }

    /// Process a single turn: preprocess → execute → postprocess, inlined.
    ///
    /// This is the single linear async function that replaces the old
    /// `RequestPipeline` three-stage split. No cross-file state搬运,
    /// no PipelineContext temporary struct, no trait objects.
    pub async fn process_direct_streaming_with_channel(
        &self,
        content: &str,
        session_key: &SessionKey,
        tool_filter: Option<Vec<String>>,
    ) -> Result<
        (
            tokio::sync::mpsc::Receiver<ChatEvent>,
            tokio::task::JoinHandle<Result<AgentResponse, AgentError>>,
        ),
        AgentError,
    > {
        // ── Stage 1: PreProcess — build request context ──────────────
        let active_model = self.model();
        let build_outcome = self.context_builder.build(content, session_key).await?;
        let request = match build_outcome {
            BuildOutcome::Aborted(msg) => {
                return Ok(early_abort_response(msg, active_model));
            }
            BuildOutcome::Ready(req) => req,
        };

        let fctx = FinalizeContext::from_request(&request);
        let messages = request.messages;
        let mut runtime_ctx = self.runtime_ctx.clone();
        runtime_ctx.refs.session_key = Some(session_key.clone());

        if let Some(compactor) = self.compactor.as_ref() {
            runtime_ctx.checkpoint_callback =
                Some(Arc::new(SessionCheckpointCallback::new(
                    fctx.session_key.clone(),
                    compactor.clone(),
                    self.context_builder.event_store().clone(),
                )));
        }

        // ── Wire channels ───────────────────────────────────────────
        let (kernel_tx, kernel_rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);
        let (chat_tx, chat_rx) = tokio::sync::mpsc::channel(64);

        let outbound_tx = bridge_outbound_to_chat(chat_tx.clone());
        runtime_ctx.refs.outbound_tx = Some(outbound_tx.clone());
        runtime_ctx.config.tool_filter = tool_filter;

        // Inject the synthesis callback at the session layer — the kernel
        // itself stays oblivious to specific channel implementations.
        let synth_session_key = runtime_ctx.refs.session_key.clone().unwrap_or_else(|| {
            gasket_types::SessionKey::new(gasket_types::events::ChannelType::Cli, "default")
        });
        runtime_ctx.refs.synthesis_callback = Some(Arc::new(
            crate::kernel::synthesis::WebSocketSynthesizer::new(
                runtime_ctx.provider.clone(),
                runtime_ctx.provider.default_model().to_string(),
                outbound_tx,
                synth_session_key,
            ),
        ));

        // Reset any previous aggregator left from the prior turn.
        let cancel = runtime_ctx
            .refs
            .aggregator_cancel
            .get_or_insert_with(gasket_types::AggregatorCancel::new);
        cancel.cancel_current();

        // ── Stage 2 + 3: Execute + PostProcess — spawn tracked task ──
        let finalizer = self.finalizer.clone();
        let fctx_for_task = fctx.clone();
        let model_for_task = active_model.clone();
        let chat_tx_err = chat_tx.clone();

        let result_handle = self.pending_done.spawn(async move {
            // Forward kernel StreamEvents to ChatEvent channel
            let stream_future = StreamExt::for_each(
                tokio_stream::wrappers::ReceiverStream::new(kernel_rx)
                    .filter_map(|event| futures_util::future::ready(event.to_chat_event())),
                |chat| {
                    let chat_tx = chat_tx.clone();
                    async move {
                        let _ = chat_tx.send(chat).await;
                    }
                },
            );

            // Execute kernel
            let exec_future = async move {
                match kernel::execute_streaming(&runtime_ctx, messages, kernel_tx).await {
                    Ok(r) => Ok::<_, AgentError>(r),
                    Err(crate::kernel::KernelError::MaxIterations(n)) => Ok(ExecutionResult {
                        content: format!("Maximum iterations ({}) reached.", n),
                        reasoning_content: None,
                        tools_used: vec![],
                        token_usage: None,
                    }),
                    Err(e) => Err(e.into()),
                }
            };

            let (result, _) = tokio::join!(exec_future, stream_future);

            if let Err(ref e) = result {
                let _ = chat_tx_err
                    .send(ChatEvent::error(format!("Agent error: {}", e)))
                    .await;
                let _ = chat_tx_err.send(ChatEvent::done()).await;
            }

            // PostProcess: finalize response
            let result = result?;
            let response = finalizer.finalize(result, &fctx_for_task, &model_for_task).await;
            Ok(response)
        });

        Ok((chat_rx, result_handle))
    }
}

// ── Free-function helpers for streaming entry point ─────────────────────────

/// Construct the response pair for the early-abort path (BeforeRequest hook
/// aborted the pipeline). No kernel is invoked; just emits the abort message.
fn early_abort_response(
    msg: String,
    model: String,
) -> (
    tokio::sync::mpsc::Receiver<ChatEvent>,
    tokio::task::JoinHandle<Result<AgentResponse, AgentError>>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let handle = tokio::spawn(async move {
        let _ = tx.send(ChatEvent::done()).await;
        Ok(AgentResponse {
            content: msg,
            reasoning_content: None,
            tools_used: vec![],
            model: Some(model),
            token_usage: None,
            cost: 0.0,
        })
    });
    (rx, handle)
}

/// Spawn a bridge task: every `OutboundMessage::Stream` payload is forwarded
/// as a `ChatEvent` onto `chat_tx`. Returns the sender for tools to use.
fn bridge_outbound_to_chat(
    chat_tx: tokio::sync::mpsc::Sender<ChatEvent>,
) -> tokio::sync::mpsc::Sender<gasket_types::events::OutboundMessage> {
    let (outbound_tx, mut outbound_rx) =
        tokio::sync::mpsc::channel::<gasket_types::events::OutboundMessage>(64);
    tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if let gasket_types::events::OutboundPayload::Stream(chat_event) = msg.payload {
                let _ = chat_tx.send(chat_event).await;
            }
        }
    });
    outbound_tx
}
