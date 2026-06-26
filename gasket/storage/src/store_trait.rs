//! Storage traits — the real, backend-agnostic interface.
//!
//! Engine / embedding / tools program against these traits, never against a
//! concrete backend. Two backends implement them: SQLite (legacy, removed in
//! Phase 3) and JSON ([`crate::JsonStore`]).
//!
//! The previous `EventStoreTrait` exposed only 4 methods while consumers called
//! ~15 inherent methods on the concrete `EventStore` struct directly — a
//! decorative trait with no swap boundary. This module fixes that by defining
//! the full surface the rest of the codebase actually needs.

use async_trait::async_trait;
use gasket_types::{SessionEvent, SessionKey};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Storage error type shared across all backends.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid UUID: {0}")]
    InvalidUuid(String),

    #[error("Invalid event type: {0}")]
    InvalidEventType(String),

    #[error("Other: {0}")]
    Other(String),
}

/// Adapt legacy `anyhow::Result` returns (e.g. `SessionStore`) into `StoreError`.
impl From<anyhow::Error> for StoreError {
    fn from(e: anyhow::Error) -> Self {
        StoreError::Other(e.to_string())
    }
}

/// Event log operations — the full surface engine / embedding / tools require.
///
/// Implementations must preserve the watermark invariant: a session's event log
/// physically holds only events with `sequence > summary watermark`; everything
/// at or below the watermark has been garbage-collected after compaction.
#[async_trait]
pub trait EventStoreTrait: Send + Sync {
    /// Append an event and return its assigned (monotonic) sequence number.
    async fn append(&self, event: &SessionEvent) -> Result<i64, StoreError>;

    /// Load the full ordered history for a session (ascending sequence).
    async fn get_session_history(&self, key: &SessionKey) -> Result<Vec<SessionEvent>, StoreError>;

    /// Load events with `sequence > after` (watermark-based recovery).
    async fn get_events_after_sequence(
        &self,
        key: &SessionKey,
        after: i64,
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Load events with `sequence <= target`, excluding summary events
    /// (compaction input).
    async fn get_events_up_to_sequence(
        &self,
        key: &SessionKey,
        target: i64,
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Current high-water sequence number for a session (0 if empty).
    async fn get_max_sequence(&self, key: &SessionKey) -> Result<i64, StoreError>;

    /// IDs of events with `sequence <= up_to` (used before GC to notify
    /// embedding listeners of removals).
    async fn get_event_ids_up_to(
        &self,
        key: &SessionKey,
        up_to: i64,
    ) -> Result<Vec<String>, StoreError>;

    /// Garbage-collect events with `sequence <= target`. Returns count deleted.
    async fn delete_events_upto(&self, key: &SessionKey, target: i64) -> Result<u64, StoreError>;

    /// Load specific events by ID, scoped to a session.
    async fn get_events_by_ids(
        &self,
        key: &SessionKey,
        ids: &[Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Load specific events by ID across all sessions (cross-session recall).
    async fn get_events_by_ids_global(
        &self,
        ids: &[Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Recent user/assistant events across all sessions (`limit == 0` = all).
    /// Used for embedding backfill.
    async fn get_recent_events(&self, limit: usize) -> Result<Vec<SessionEvent>, StoreError>;

    /// Keyword search within a session's user/assistant events (substring match).
    async fn search_session_events(
        &self,
        key: &SessionKey,
        keyword: &str,
        limit: i64,
    ) -> Result<Vec<SessionEvent>, StoreError>;

    /// Most recent summary event for a session, if any.
    async fn get_latest_summary(&self, key: &SessionKey)
        -> Result<Option<SessionEvent>, StoreError>;

    /// Delete all events + metadata for a session.
    async fn clear_session(&self, key: &SessionKey) -> Result<(), StoreError>;

    /// All session keys for a channel.
    async fn get_sessions_by_channel(&self, channel: &str) -> Result<Vec<String>, StoreError>;

    /// Subscribe to newly appended events via broadcast channel.
    fn subscribe(&self) -> broadcast::Receiver<SessionEvent>;
}

/// Session summary / checkpoint / compaction-state operations.
#[async_trait]
pub trait SessionStoreTrait: Send + Sync {
    /// Upsert a session summary with its sequence watermark.
    async fn save_summary(
        &self,
        key: &SessionKey,
        content: &str,
        watermark: i64,
    ) -> Result<(), StoreError>;

    /// Load a session summary and its watermark (`None` if absent).
    async fn load_summary(&self, key: &SessionKey) -> Result<Option<(String, i64)>, StoreError>;

    /// Delete a session summary. Returns `true` if one existed.
    async fn delete_summary(&self, key: &SessionKey) -> Result<bool, StoreError>;

    /// Load `(summary, checkpoint_summary, watermark)`.
    async fn load_summary_with_checkpoint(
        &self,
        key: &SessionKey,
    ) -> Result<(String, String, i64), StoreError>;

    /// Save a checkpoint summary at a target sequence.
    async fn save_checkpoint(
        &self,
        key: &str,
        target: i64,
        summary: &str,
    ) -> Result<(), StoreError>;

    /// Most recent checkpoint at or before `target`.
    async fn load_checkpoint(
        &self,
        key: &str,
        target: i64,
    ) -> Result<Option<(String, i64)>, StoreError>;

    /// Scan all sessions with at least one event: `(session_key, total_events, updated_at)`.
    async fn scan_active_sessions(&self) -> Result<Vec<(String, i64, String)>, StoreError>;

    /// Sessions needing a maintenance task: `(session_key, total_events, watermark)`.
    async fn get_sessions_needing_evolution(
        &self,
        task: &str,
        threshold: i64,
    ) -> Result<Vec<(String, i64, i64)>, StoreError>;

    /// Mark compaction started for a session.
    async fn mark_compaction_started(&self, key: &SessionKey) -> Result<(), StoreError>;

    /// Mark compaction finished for a session.
    async fn mark_compaction_finished(&self, key: &SessionKey) -> Result<(), StoreError>;

    /// Whether compaction is marked in-progress for a session.
    async fn is_compaction_in_progress(&self, key: &SessionKey) -> Result<bool, StoreError>;
}
