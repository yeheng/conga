//! Watermark-based background context compactor.
//!
//! # Design: Sequence High-Water Mark
//!
//! Compaction uses a single integer cursor (`covered_upto_sequence`) stored in
//! the `session_summaries` table. This replaces the previous `covered_event_ids:
//! Vec<Uuid>` tracking — one integer instead of hundreds of UUIDs.
//!
//! # Read Path
//!
//! ```text
//! SELECT content, covered_upto_sequence FROM session_summaries WHERE session_key = ?
//! → summary text + watermark
//! SELECT * FROM session_events WHERE session_key = ? AND sequence > watermark
//! → recent (uncompacted) events
//! Assemble: [System Prompt] + [Summary] + [Recent Events]
//! ```
//!
//! # Write Path (Background)
//!
//! ```text
//! 1. should_compact() → guard + threshold check
//! 2. try_acquire_lock() → CAS guard
//! 3. spawn_compaction_task() → tokio::spawn
//!    └─ run_compaction()
//!       ├─ load target_sequence, existing_summary, events from DB
//!       ├─ build_context_text() → assemble LLM input
//!       ├─ summarize_with_llm() → call provider
//!       └─ persist_and_gc() → redact, save summary, garbage-collect
//! ```
//!
//! # Crash Safety
//!
//! If the process crashes during compaction, nothing is lost: the summary
//! watermark wasn't updated, so the next startup loads slightly more history
//! and triggers compaction again. This is **natural idempotency**.

pub mod checkpoint;
pub mod pipeline;
pub mod stats;

pub use checkpoint::CheckpointConfig;
pub use pipeline::{build_context_text, persist_and_gc, run_compaction, summarize_with_llm};
pub use stats::{UsageStats, WatermarkInfo};

/// Cooldown after LLM failure: skip compaction for 60s to avoid hammering a failing API.
const COMPACTION_COOLDOWN_SECS: u64 = 60;

/// Watermark-based context compactor.
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use gasket_providers::LlmProvider;
use gasket_storage::{EventStoreTrait, SessionStoreTrait};
use gasket_types::SessionKey;

/// Default prompt for LLM summarization (used when no custom prompt is configured).
pub const DEFAULT_SUMMARIZATION_PROMPT: &str =
    "Summarize the following conversation briefly, keeping key facts, decisions, and outcomes.";

/// Alias for backward compatibility.
pub const SUMMARIZATION_PROMPT: &str = DEFAULT_SUMMARIZATION_PROMPT;

/// Prefix for injected summary system messages.
pub const SUMMARY_PREFIX: &str = "[Conversation Summary]\n";

pub const SUMMARY_SUFFIX: &str = "\n[End of Summary]";

/// Prefix for recalled history injection.
pub const RECALL_PREFIX: &str = "[回忆]";

// ---------------------------------------------------------------------------
// CompactionListener — notification interface for embedding cleanup
// ---------------------------------------------------------------------------

/// Listener notified when compaction deletes events.
///
/// Implementors (e.g. `EmbeddingIndexer`) use this to clean up associated
/// embeddings when events are garbage-collected by the compactor.
pub trait CompactionListener: Send + Sync {
    /// Called with the IDs of events about to be (or already) deleted.
    fn on_events_deleted(&self, event_ids: &[String]);
}

// ---------------------------------------------------------------------------
// CompactorState — unified state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum CompactorState {
    Idle,
    Compressing { pending: bool },
    Cooldown(Instant),
}

// ---------------------------------------------------------------------------
// ContextCompactor — public API
// ---------------------------------------------------------------------------

/// Watermark-based context compactor.
///
/// Called via `tokio::spawn` after each agent response when the token budget
/// is exceeded. Uses an `AtomicBool` guard to prevent concurrent compaction
/// for the same session.
///
/// The compactor is self-contained: given a `target_sequence`, it loads all
/// data it needs from the database, calls the LLM, persists the result, and
/// garbage-collects old events. No in-memory state is required from the caller.
pub struct ContextCompactor {
    /// LLM provider for generating summaries.
    provider: Arc<dyn LlmProvider>,
    /// Event store for loading events and garbage collection.
    event_store: Arc<dyn EventStoreTrait>,
    /// SQLite store for summary persistence (session_summaries table).
    session_store: Arc<dyn SessionStoreTrait>,
    /// Model to use for summarization.
    model: String,
    /// Token budget for context window.
    token_budget: usize,
    /// Compaction threshold multiplier (default 1.2).
    compaction_threshold: f32,
    /// Custom summarization prompt.
    summarization_prompt: String,
    /// Cooldown after LLM failure in seconds (default: 60).
    cooldown_secs: u64,
    /// Unified state machine: Idle | Compressing { pending } | Cooldown(Instant).
    state: Arc<parking_lot::Mutex<CompactorState>>,
    /// Optional checkpoint configuration for proactive working-memory snapshots.
    checkpoint_config: Option<CheckpointConfig>,
    /// Listeners notified when events are deleted during compaction.
    listeners: Vec<Arc<dyn CompactionListener>>,
    /// Optional task tracker for graceful shutdown. When set, compaction tasks
    /// are spawned through the tracker so they can be awaited during shutdown.
    task_tracker: Option<tokio_util::task::TaskTracker>,
}

impl ContextCompactor {
    /// Default compaction threshold multiplier.
    pub const DEFAULT_COMPACTION_THRESHOLD: f32 = 1.2;

    /// Create a new compactor.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        event_store: Arc<dyn EventStoreTrait>,
        session_store: Arc<dyn SessionStoreTrait>,
        model: String,
        token_budget: usize,
    ) -> Self {
        Self {
            provider,
            event_store,
            session_store,
            model,
            token_budget,
            compaction_threshold: Self::DEFAULT_COMPACTION_THRESHOLD,
            summarization_prompt: DEFAULT_SUMMARIZATION_PROMPT.to_string(),
            cooldown_secs: COMPACTION_COOLDOWN_SECS,
            state: Arc::new(parking_lot::Mutex::new(CompactorState::Idle)),
            checkpoint_config: None,
            listeners: Vec::new(),
            task_tracker: None,
        }
    }

    /// Set a custom summarization prompt (overrides built-in default).
    pub fn with_summarization_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.summarization_prompt = prompt.into();
        self
    }

    /// Enable proactive checkpointing.
    pub fn with_checkpoint_config(mut self, config: CheckpointConfig) -> Self {
        self.checkpoint_config = Some(config);
        self
    }

    /// Set a custom compaction threshold multiplier.
    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.compaction_threshold = threshold;
        self
    }

    /// Set cooldown after LLM failure in seconds.
    pub fn with_cooldown_secs(mut self, secs: u64) -> Self {
        self.cooldown_secs = secs;
        self
    }

    /// Add a compaction listener to be notified when events are deleted.
    pub fn add_listener(&mut self, listener: Arc<dyn CompactionListener>) {
        self.listeners.push(listener);
    }

    /// Attach a task tracker so compaction tasks are tracked for graceful shutdown.
    pub fn with_task_tracker(mut self, tracker: tokio_util::task::TaskTracker) -> Self {
        self.task_tracker = Some(tracker);
        self
    }

    /// Check whether compaction is currently in progress.
    pub fn is_compressing(&self) -> bool {
        matches!(*self.state.lock(), CompactorState::Compressing { .. })
    }

    /// Load the current summary and its watermark for a session.
    ///
    /// Returns `(summary_text, covered_upto_sequence)`.
    /// If no summary exists, returns `("", 0)`.
    pub async fn load_summary_with_watermark(&self, session_key: &SessionKey) -> (String, i64) {
        stats::load_summary_with_watermark(self.session_store.as_ref(), session_key).await
    }

    // -----------------------------------------------------------------------
    // Inspection APIs
    // -----------------------------------------------------------------------

    /// Get context usage statistics for a session.
    pub async fn get_usage_stats(&self, session_key: &SessionKey) -> Result<UsageStats> {
        stats::get_usage_stats(
            self.event_store.as_ref(),
            self.session_store.as_ref(),
            session_key,
            self.token_budget,
            self.compaction_threshold,
            self.is_compressing(),
        )
        .await
    }

    /// Get watermark and sequence information for a session.
    pub async fn get_watermark_info(&self, session_key: &SessionKey) -> Result<WatermarkInfo> {
        stats::get_watermark_info(
            self.event_store.as_ref(),
            self.session_store.as_ref(),
            session_key,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Public entry points
    // -----------------------------------------------------------------------

    /// Force-trigger background compaction, bypassing the threshold check.
    ///
    /// Unlike `try_compact`, this always triggers compaction (unless already
    /// in progress). Useful for manual context management via the `context` tool.
    ///
    /// Returns `true` if compaction was triggered, `false` if already in progress.
    pub fn force_compact(&self, session_key: &SessionKey, vault_values: &[String]) -> bool {
        let sk = session_key.to_string();
        {
            let mut state = self.state.lock();
            if matches!(*state, CompactorState::Compressing { .. }) {
                debug!("Force compaction skipped: already in progress for {}", sk);
                return false;
            }
            *state = CompactorState::Compressing { pending: false };
        }
        let guard = CompactionGuard::new(self.state.clone());
        info!("Force compaction triggered for {}", sk);
        self.spawn_compaction_task(session_key, vault_values, guard);
        true
    }

    /// Force-trigger compaction and await its completion.
    ///
    /// Returns `Ok(())` if compaction ran successfully, `Err` if already in progress
    /// or if compaction failed.
    pub async fn force_compact_and_wait(
        &self,
        session_key: &SessionKey,
        vault_values: &[String],
    ) -> Result<()> {
        let sk = session_key.to_string();
        {
            let mut state = self.state.lock();
            if matches!(*state, CompactorState::Compressing { .. }) {
                bail!("Compaction already in progress for {}", sk);
            }
            *state = CompactorState::Compressing { pending: false };
        }
        info!("Force compaction (blocking) started for {}", sk);
        let _guard = CompactionGuard::new(self.state.clone());

        if let Err(e) = self
            .session_store
            .mark_compaction_started(session_key)
            .await
        {
            warn!("Failed to mark compaction started for {}: {}", sk, e);
        }

        let listeners: Vec<Arc<dyn CompactionListener>> = self.listeners.clone();

        let result = pipeline::run_compaction(
            self.event_store.as_ref(),
            self.session_store.as_ref(),
            &*self.provider,
            &self.model,
            &self.summarization_prompt,
            session_key,
            vault_values,
            &listeners,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Compaction failed for {}: {}", sk, e));

        if let Err(e) = self
            .session_store
            .mark_compaction_finished(session_key)
            .await
        {
            warn!("Failed to mark compaction finished for {}: {}", sk, e);
        }

        result
    }

    /// Generate a proactive checkpoint for the current session state.
    ///
    /// Called every N sequence increments. Returns `Some(summary)` if a
    /// checkpoint was generated, `None` if skipped.
    ///
    /// `current_max_sequence` must be fetched from `EventStore` — never pass
    /// a transient turn counter.
    pub async fn checkpoint(
        &self,
        session_key: &SessionKey,
        current_max_sequence: i64,
    ) -> Result<Option<String>> {
        let config = match &self.checkpoint_config {
            Some(c) => c,
            None => return Ok(None),
        };

        checkpoint::checkpoint(
            &*self.provider,
            &self.model,
            config,
            self.event_store.as_ref(),
            self.session_store.as_ref(),
            session_key,
            current_max_sequence,
        )
        .await
    }

    /// Save an ask-user semantic checkpoint for working-memory recovery.
    ///
    /// Called when the agent issues an `ask_user` tool call so the session
    /// can be resumed after a long timeout.
    pub async fn save_ask_checkpoint(
        &self,
        session_key: &SessionKey,
        messages: &[gasket_providers::ChatMessage],
        pending_question: &str,
    ) -> Result<()> {
        checkpoint::save_ask_checkpoint(
            &*self.provider,
            &self.model,
            self.session_store.as_ref(),
            session_key,
            messages,
            pending_question,
        )
        .await
    }

    /// Try to trigger background compaction.
    ///
    /// This is the main entry point, called from `finalize_response()`.
    /// Decomposed into three steps: [should_compact] → [try_acquire_lock] →
    /// [spawn_compaction_task].
    ///
    /// # Returns
    ///
    /// `true` if compaction was triggered, `false` if skipped (already
    /// compressing or below threshold).
    pub fn try_compact(
        &self,
        session_key: &SessionKey,
        current_tokens: usize,
        vault_values: &[String],
    ) -> bool {
        let sk = session_key.to_string();
        let threshold = (self.token_budget as f32 * self.compaction_threshold) as usize;
        if current_tokens < threshold {
            debug!(
                "Skipping compaction for {}: {} tokens < threshold {} (budget={}, mult={})",
                sk, current_tokens, threshold, self.token_budget, self.compaction_threshold
            );
            return false;
        }

        let started = {
            let mut state = self.state.lock();
            match *state {
                CompactorState::Compressing { ref mut pending } => {
                    *pending = true;
                    debug!(
                        "Compaction deferred for {}: already in progress, marked pending",
                        sk
                    );
                    false
                }
                CompactorState::Cooldown(last_fail) => {
                    if last_fail.elapsed().as_secs() < self.cooldown_secs {
                        debug!(
                            "Compaction in cooldown for {}: {}s since last failure",
                            sk,
                            last_fail.elapsed().as_secs()
                        );
                        false
                    } else {
                        *state = CompactorState::Compressing { pending: false };
                        true
                    }
                }
                CompactorState::Idle => {
                    *state = CompactorState::Compressing { pending: false };
                    true
                }
            }
        };

        if started {
            let guard = CompactionGuard::new(self.state.clone());
            self.spawn_compaction_task(session_key, vault_values, guard);
        }
        started
    }

    // -----------------------------------------------------------------------
    // Task spawning
    // -----------------------------------------------------------------------

    /// Clone all fields and spawn the background compaction task.
    fn spawn_compaction_task(
        &self,
        session_key: &SessionKey,
        vault_values: &[String],
        guard: CompactionGuard,
    ) {
        let event_store = self.event_store.clone();
        let session_store = self.session_store.clone();
        let provider = self.provider.clone();
        let model = self.model.clone();
        let summarization_prompt = self.summarization_prompt.clone();
        let sk = session_key.clone();
        let vault = vault_values.to_vec();
        let state = self.state.clone();
        // Clone Arc references to listeners so the background task can notify them.
        let listeners: Vec<Arc<dyn CompactionListener>> = self.listeners.clone();

        let spawn = |fut| {
            if let Some(ref tracker) = self.task_tracker {
                tracker.spawn(fut)
            } else {
                tokio::spawn(fut)
            }
        };

        spawn(async move {
            let _guard = guard;
            debug!("Background compaction started for {}", sk);

            if let Err(e) = session_store.mark_compaction_started(&sk).await {
                warn!("Failed to mark compaction started for {}: {}", sk, e);
            }

            let mut follow_up = false;
            let run = async {
                if let Err(e) = pipeline::run_compaction(
                    event_store.as_ref(),
                    session_store.as_ref(),
                    &*provider,
                    &model,
                    &summarization_prompt,
                    &sk,
                    &vault,
                    &listeners,
                )
                .await
                {
                    warn!("Compaction failed for {}: {}", sk, e);
                    *state.lock() = CompactorState::Cooldown(Instant::now());
                } else {
                    // Check if a follow-up was requested while we were running.
                    follow_up =
                        matches!(*state.lock(), CompactorState::Compressing { pending: true });
                }
            };

            run.await;

            // If pending was set while we were running, re-trigger.
            if follow_up {
                info!(
                    "Pending compaction detected for {} — re-triggering immediately",
                    sk
                );
                *state.lock() = CompactorState::Compressing { pending: false };
                if let Err(e) = pipeline::run_compaction(
                    event_store.as_ref(),
                    session_store.as_ref(),
                    &*provider,
                    &model,
                    &summarization_prompt,
                    &sk,
                    &vault,
                    &listeners,
                )
                .await
                {
                    warn!("Follow-up compaction failed for {}: {}", sk, e);
                    *state.lock() = CompactorState::Cooldown(Instant::now());
                }
            }

            if let Err(e) = session_store.mark_compaction_finished(&sk).await {
                warn!("Failed to mark compaction finished for {}: {}", sk, e);
            }
        });
    }
}

/// RAII guard that resets the compactor state to Idle on drop, ensuring panic safety.
struct CompactionGuard {
    state: Arc<parking_lot::Mutex<CompactorState>>,
}

impl CompactionGuard {
    fn new(state: Arc<parking_lot::Mutex<CompactorState>>) -> Self {
        Self { state }
    }
}

impl Drop for CompactionGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        // Only reset to Idle if still in Compressing state.
        // Preserve Cooldown — it was set because the LLM call failed
        // and we want to avoid hammering a failing API.
        if matches!(*state, CompactorState::Compressing { .. }) {
            *state = CompactorState::Idle;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarization_prompt_not_empty() {
        assert!(!SUMMARIZATION_PROMPT.is_empty());
    }

    #[test]
    fn test_checkpoint_config_default() {
        let config = CheckpointConfig::default();
        assert_eq!(config.interval_turns, 7);
        assert!(config.prompt.contains("Current goal"));
    }

    #[test]
    fn test_summary_prefix_format() {
        assert!(SUMMARY_PREFIX.starts_with('['));
        assert!(SUMMARY_PREFIX.ends_with('\n'));
        assert!(!SUMMARY_SUFFIX.is_empty());
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_atomic_bool_guard() {
        let flag = Arc::new(AtomicBool::new(false));

        assert!(flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());

        assert!(flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err());

        flag.store(false, Ordering::Release);
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_should_compact_below_threshold() {
        let token_budget: usize = 1000;
        let threshold_mult: f32 = 1.2;
        let overflow_threshold = (token_budget as f32 * threshold_mult) as usize;

        assert!(900 < overflow_threshold);
        assert!(1200 >= overflow_threshold);
    }

    #[test]
    fn test_build_context_text_empty() {
        let text = pipeline::build_context_text(None, &[]);
        assert!(text.is_empty());
    }

    #[test]
    fn test_build_context_text_with_summary() {
        let text = pipeline::build_context_text(Some("previous summary"), &[]);
        assert!(text.contains("Previous summary:"));
        assert!(text.contains("previous summary"));
    }

    #[test]
    fn test_build_context_text_with_events() {
        use gasket_types::EventType;
        use uuid::Uuid;

        let event = gasket_types::SessionEvent {
            id: Uuid::new_v4(),
            session_key: "test".to_string(),
            event_type: EventType::UserMessage,
            content: "hello".to_string(),
            sequence: 1,
            created_at: chrono::Utc::now(),
            metadata: Default::default(),
        };

        let text = pipeline::build_context_text(None, &[event]);
        assert!(text.contains("hello"));
        assert!(!text.contains("Previous summary"));
    }

    // ── Crash Recovery Integration Test ──────────────────────────────────

    struct MockProvider {
        response: parking_lot::Mutex<String>,
        fail: AtomicBool,
    }

    impl MockProvider {
        fn new(summary: &str) -> Self {
            Self {
                response: parking_lot::Mutex::new(summary.to_string()),
                fail: AtomicBool::new(false),
            }
        }

        fn set_fail(&self, fail: bool) {
            self.fail.store(fail, Ordering::Release);
        }
    }

    #[async_trait::async_trait]
    impl gasket_providers::LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        async fn chat(
            &self,
            _request: gasket_providers::ChatRequest,
        ) -> std::result::Result<gasket_providers::ChatResponse, gasket_providers::ProviderError>
        {
            if self.fail.load(Ordering::Acquire) {
                return Err(gasket_providers::ProviderError::ApiError {
                    status_code: 500,
                    message: "simulated crash".into(),
                });
            }
            let content = self.response.lock().clone();
            Ok(gasket_providers::ChatResponse::text(content))
        }
    }

    async fn setup_compaction_db() -> (
        sqlx::SqlitePool,
        gasket_storage::EventStore,
        gasket_storage::SessionStore,
    ) {
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new().connect(":memory:").await.unwrap();

        sqlx::query(
            r#"
            CREATE TABLE sessions_v2 (
                key TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_consolidated_event TEXT,
                total_events INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                channel TEXT NOT NULL DEFAULT '',
                chat_id TEXT NOT NULL DEFAULT ''
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE session_events (
                id TEXT PRIMARY KEY,
                session_key TEXT NOT NULL,
                channel TEXT NOT NULL DEFAULT '',
                chat_id TEXT NOT NULL DEFAULT '',
                event_type TEXT NOT NULL,
                content TEXT NOT NULL,
                tools_used TEXT DEFAULT '[]',
                token_usage TEXT,
                token_len INTEGER NOT NULL DEFAULT 0,
                event_data TEXT,
                extra TEXT DEFAULT '{}',
                created_at TEXT NOT NULL,
                sequence INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("CREATE INDEX idx_events_channel_chat ON session_events(channel, chat_id)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            "CREATE INDEX idx_events_channel_chat_sequence ON session_events(channel, chat_id, sequence)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("CREATE INDEX idx_sessions_v2_channel_chat ON sessions_v2(channel, chat_id)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS session_summaries (
                session_key TEXT PRIMARY KEY,
                content TEXT NOT NULL DEFAULT '',
                covered_upto_sequence INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                compaction_in_progress INTEGER NOT NULL DEFAULT 0,
                compaction_started_at TEXT
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        (
            pool.clone(),
            gasket_storage::EventStore::new(pool.clone()),
            gasket_storage::SessionStore::new(pool),
        )
    }

    async fn append_messages(
        event_store: &gasket_storage::EventStore,
        session_key: &SessionKey,
        messages: &[&str],
    ) {
        for msg in messages {
            let event = gasket_types::SessionEvent {
                id: uuid::Uuid::now_v7(),
                session_key: session_key.to_string(),
                event_type: gasket_types::EventType::UserMessage,
                content: msg.to_string(),
                metadata: gasket_types::EventMetadata::default(),
                created_at: chrono::Utc::now(),
                sequence: 0,
            };
            event_store.append_event(&event).await.unwrap();
        }
    }

    /// Verify crash recovery: if compaction fails mid-flight, the watermark
    /// invariant holds and no data is lost.
    #[tokio::test]
    async fn test_crash_recovery_watermark() {
        let (_pool, event_store, session_store) = setup_compaction_db().await;
        let session_key = SessionKey::new(gasket_types::ChannelType::Cli, "crash-test");
        let provider = Arc::new(MockProvider::new("Summary of the conversation."));
        let model = "mock-model";
        let prompt = DEFAULT_SUMMARIZATION_PROMPT;

        // Phase 1: Create initial events (seq 0-5)
        append_messages(
            &event_store,
            &session_key,
            &["msg0", "msg1", "msg2", "msg3", "msg4", "msg5"],
        )
        .await;
        assert_eq!(event_store.get_max_sequence(&session_key).await.unwrap(), 5);

        // Phase 2: Successful compaction → watermark = 5
        pipeline::run_compaction(
            &event_store,
            &session_store,
            &*provider,
            model,
            prompt,
            &session_key,
            &[],
            &[],
        )
        .await
        .expect("first compaction should succeed");

        let (_, wm) = session_store
            .load_summary(&session_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wm, 5, "watermark should be 5 after first compaction");

        // GC should have removed old events
        let remaining = event_store
            .get_events_after_sequence(&session_key, 0)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 0, "events up to watermark should be GC'd");

        // Phase 3: Add 4 more events (seq 6-9)
        append_messages(
            &event_store,
            &session_key,
            &["msg6", "msg7", "msg8", "msg9"],
        )
        .await;
        assert_eq!(event_store.get_max_sequence(&session_key).await.unwrap(), 9);

        // Verify new events are readable from old watermark
        let uncompacted = event_store
            .get_events_after_sequence(&session_key, wm)
            .await
            .unwrap();
        assert_eq!(uncompacted.len(), 4);

        // Phase 4: Simulate crash — LLM fails mid-compaction
        provider.set_fail(true);
        let result = pipeline::run_compaction(
            &event_store,
            &session_store,
            &*provider,
            model,
            prompt,
            &session_key,
            &[],
            &[],
        )
        .await;
        assert!(result.is_err(), "compaction should fail when LLM crashes");
        provider.set_fail(false);

        // Phase 5: Verify watermark invariant — NO data loss
        let (_, wm_after_crash) = session_store
            .load_summary(&session_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            wm_after_crash, 5,
            "watermark must NOT advance after failed compaction"
        );

        let events_after_crash = event_store
            .get_events_after_sequence(&session_key, wm_after_crash)
            .await
            .unwrap();
        assert_eq!(
            events_after_crash.len(),
            4,
            "no data loss: all 4 new events still accessible"
        );
        assert_eq!(events_after_crash[0].content, "msg6");
        assert_eq!(events_after_crash[3].content, "msg9");

        // Phase 6: Retry compaction successfully
        pipeline::run_compaction(
            &event_store,
            &session_store,
            &*provider,
            model,
            prompt,
            &session_key,
            &[],
            &[],
        )
        .await
        .expect("retry compaction should succeed");

        // Phase 7: Full recovery verified
        let (summary, final_wm) = session_store
            .load_summary(&session_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_wm, 9, "watermark should advance to 9");
        assert!(!summary.is_empty());

        let events_after_final = event_store
            .get_events_after_sequence(&session_key, final_wm)
            .await
            .unwrap();
        assert_eq!(
            events_after_final.len(),
            0,
            "all events compacted — nothing uncompacted remains"
        );
    }
}
