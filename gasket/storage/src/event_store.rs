//! Event store for event sourcing architecture.

use crate::processor::count_tokens;
use crate::store_trait::{EventStoreTrait, StoreError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gasket_types::{EventMetadata, EventType, SessionEvent, SessionKey, TokenUsage};

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

fn event_type_tag(et: &EventType) -> &'static str {
    match et {
        EventType::UserMessage => "user_message",
        EventType::AssistantMessage => "assistant_message",
        EventType::ToolCall { .. } => "tool_call",
        EventType::ToolResult { .. } => "tool_result",
        EventType::Summary { .. } => "summary",
    }
}

#[derive(Clone)]
pub struct EventStore {
    pool: SqlitePool,
    tx: broadcast::Sender<SessionEvent>,
}

impl EventStore {
    pub fn new(pool: SqlitePool) -> Self {
        let (tx, _) = broadcast::channel(64);
        info!("EventStore created");
        Self { pool, tx }
    }

    /// Create an EventStore with an existing broadcast sender.
    /// Used to share the same event broadcast channel across multiple EventStore instances.
    pub fn with_pool_and_sender(pool: SqlitePool, tx: broadcast::Sender<SessionEvent>) -> Self {
        Self { pool, tx }
    }

    /// Clone the broadcast sender so another EventStore instance can share the same channel.
    pub fn sender(&self) -> broadcast::Sender<SessionEvent> {
        self.tx.clone()
    }

    /// Parse a session key string into (channel, chat_id).
    /// Falls back to ChannelType::Cli if no channel prefix.
    fn parse_session_key_str(session_key: &str) -> (String, String) {
        let key = SessionKey::parse(session_key)
            .unwrap_or_else(|| SessionKey::new(gasket_types::ChannelType::Cli, session_key));
        (key.channel.to_string(), key.chat_id)
    }

    /// Append an event with atomic sequence generation.
    ///
    /// Sequence is derived from `sessions_v2.total_events` inside the same
    /// transaction, eliminating the read-then-write race that existed when
    /// `generate_sequence` ran outside the transaction.
    async fn append_event_internal(&self, event: &SessionEvent) -> Result<i64, StoreError> {
        let event_type_tag = event_type_tag(&event.event_type);
        let event_data_json = match &event.event_type {
            EventType::UserMessage | EventType::AssistantMessage => None,
            _ => Some(serde_json::to_string(&event.event_type)?),
        };
        let tools_used = serde_json::to_string(&event.metadata.tools_used)?;
        let token_usage = event
            .metadata
            .token_usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let extra = serde_json::to_string(&event.metadata.extra)?;

        let (channel, chat_id) = Self::parse_session_key_str(&event.session_key);

        let mut tx = self.pool.begin().await?;

        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT OR IGNORE INTO sessions_v2 (key, channel, chat_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&event.session_key)
        .bind(&channel)
        .bind(&chat_id)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;

        // Atomic sequence: read total_events inside the transaction, use as sequence.
        // SQLite WAL serializes writes within a single transaction.
        let sequence: i64 = sqlx::query_scalar(
            "SELECT COALESCE(total_events, 0) FROM sessions_v2 WHERE channel = ? AND chat_id = ?",
        )
        .bind(&channel)
        .bind(&chat_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);

        let token_len = if event.metadata.content_token_len > 0 {
            event.metadata.content_token_len as i64
        } else {
            count_tokens(&event.content) as i64
        };

        sqlx::query(
            r#"
            INSERT INTO session_events
            (id, session_key, channel, chat_id, event_type, content,
             tools_used, token_usage, token_len, event_data, extra, created_at, sequence)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(event.id.to_string())
        .bind(&event.session_key)
        .bind(&channel)
        .bind(&chat_id)
        .bind(event_type_tag)
        .bind(&event.content)
        .bind(&tools_used)
        .bind(token_usage.as_deref())
        .bind(token_len)
        .bind(event_data_json.as_deref())
        .bind(&extra)
        .bind(event.created_at.to_rfc3339())
        .bind(sequence)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE sessions_v2 SET updated_at = ?, total_events = total_events + 1 WHERE channel = ? AND chat_id = ?",
        )
        .bind(&now)
        .bind(&channel)
        .bind(&chat_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        let _ = self.tx.send(event.clone());

        debug!(
            "Appended event: type={}, session={}, seq={}",
            event_type_tag, event.session_key, sequence
        );

        Ok(sequence)
    }

    pub async fn append_event(&self, event: &SessionEvent) -> Result<(), StoreError> {
        self.append_event_internal(event).await?;
        Ok(())
    }

    pub async fn get_session_history(
        &self,
        session_key: &SessionKey,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let rows = sqlx::query_as::<_, EventRow>(
            r#"
            SELECT * FROM session_events
            WHERE channel = ? AND chat_id = ?
            ORDER BY sequence ASC
            "#,
        )
        .bind(&channel)
        .bind(chat_id)
        .fetch_all(&self.pool)
        .await?;

        debug!("Loaded {} events for session {}", rows.len(), session_key);
        rows.into_iter().map(|r| r.try_into()).collect()
    }

    /// Load events with sequence > after_sequence for a session (watermark-based query).
    ///
    /// This is the core read-path method for the watermark-based compaction design.
    /// Returns only events not yet covered by the summary's high-water mark,
    /// using the composite index on (session_key, sequence) for efficient lookup.
    pub async fn get_events_after_sequence(
        &self,
        session_key: &SessionKey,
        after_sequence: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let rows = sqlx::query_as::<_, EventRow>(
            r#"
            SELECT * FROM session_events
            WHERE channel = ? AND chat_id = ? AND sequence > ?
            ORDER BY sequence ASC
            "#,
        )
        .bind(&channel)
        .bind(chat_id)
        .bind(after_sequence)
        .fetch_all(&self.pool)
        .await?;

        debug!(
            "Loaded {} events after seq {} for {}",
            rows.len(),
            after_sequence,
            session_key
        );
        rows.into_iter().map(|r| r.try_into()).collect()
    }

    /// Load events with sequence <= target_sequence for compaction input.
    ///
    /// Returns events that are about to be summarized, excluding summary events
    /// to avoid circular references. Used by the compactor to gather input
    /// for LLM summarization.
    pub async fn get_events_up_to_sequence(
        &self,
        session_key: &SessionKey,
        target_sequence: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let rows = sqlx::query_as::<_, EventRow>(
            r#"
            SELECT * FROM session_events
            WHERE channel = ? AND chat_id = ? AND sequence <= ? AND event_type != 'summary'
            ORDER BY sequence ASC
            "#,
        )
        .bind(&channel)
        .bind(chat_id)
        .bind(target_sequence)
        .fetch_all(&self.pool)
        .await?;

        debug!(
            "Loaded {} events up to seq {} for {} (excl. summaries)",
            rows.len(),
            target_sequence,
            session_key
        );
        rows.into_iter().map(|r| r.try_into()).collect()
    }

    pub async fn get_events_by_ids(
        &self,
        session_key: &SessionKey,
        event_ids: &[Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError> {
        if event_ids.is_empty() {
            return Ok(vec![]);
        }

        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let placeholders: String = event_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT * FROM session_events WHERE channel = ? AND chat_id = ? AND id IN ({}) ORDER BY sequence ASC",
            placeholders
        );

        let mut sql_query = sqlx::query_as::<_, EventRow>(&query);
        sql_query = sql_query.bind(&channel).bind(chat_id);
        for id in event_ids {
            sql_query = sql_query.bind(id.to_string());
        }

        let rows = sql_query.fetch_all(&self.pool).await?;
        rows.into_iter().map(|r| r.try_into()).collect()
    }

    /// Load events by IDs across all sessions (no session scoping).
    ///
    /// Unlike `get_events_by_ids`, this does not filter by session key,
    /// making it suitable for cross-session recall.
    pub async fn get_events_by_ids_global(
        &self,
        ids: &[Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT * FROM session_events WHERE id IN ({}) ORDER BY session_key ASC, sequence ASC",
            placeholders
        );
        let mut q = sqlx::query_as::<_, EventRow>(&query);
        for id in ids {
            q = q.bind(id.to_string());
        }
        let rows = q.fetch_all(&self.pool).await?;
        rows.into_iter().map(|r| r.try_into()).collect()
    }

    /// Return event IDs that will be deleted by `delete_events_upto`.
    ///
    /// Used by CompactionListener to know which embeddings to clean up
    /// before the actual deletion occurs.
    pub async fn get_event_ids_up_to(
        &self,
        session_key: &SessionKey,
        up_to_seq: i64,
    ) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM session_events WHERE channel = ? AND chat_id = ? AND sequence <= ?",
        )
        .bind(session_key.channel.to_string())
        .bind(&session_key.chat_id)
        .bind(up_to_seq)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    pub async fn clear_session(&self, session_key: &SessionKey) -> Result<(), StoreError> {
        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM session_events WHERE channel = ? AND chat_id = ?")
            .bind(&channel)
            .bind(chat_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions_v2 WHERE channel = ? AND chat_id = ?")
            .bind(&channel)
            .bind(chat_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        info!("Cleared session {}", session_key);
        Ok(())
    }

    /// Get the maximum sequence number for a session.
    ///
    /// Returns 0 if the session has no events. Used by the compaction
    /// pipeline to determine the current high-water mark.
    pub async fn get_max_sequence(&self, session_key: &SessionKey) -> Result<i64, StoreError> {
        let max_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM session_events WHERE channel = ? AND chat_id = ?",
        )
        .bind(session_key.channel.to_string())
        .bind(&session_key.chat_id)
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0);
        Ok(max_seq)
    }

    /// Garbage-collect events that have been summarized.
    ///
    /// Deletes all events with `sequence <= target_sequence` for the given session.
    /// This is called after a successful compaction — the summary's
    /// `covered_upto_sequence` watermark guarantees these events are covered.
    pub async fn delete_events_upto(
        &self,
        session_key: &SessionKey,
        target_sequence: i64,
    ) -> Result<u64, StoreError> {
        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let result = sqlx::query(
            "DELETE FROM session_events WHERE channel = ? AND chat_id = ? AND sequence <= ?",
        )
        .bind(&channel)
        .bind(chat_id)
        .bind(target_sequence)
        .execute(&self.pool)
        .await?;
        let deleted = result.rows_affected();
        info!(
            "GC'd {} events up to seq {} for {}",
            deleted, target_sequence, session_key
        );
        Ok(deleted)
    }

    /// Get the most recent summary event for a session.
    ///
    /// Returns the latest `EventType::Summary` event, which serves as a
    /// checkpoint for context reconstruction. Used by the compression
    /// pipeline to load the existing summary before generating a new one.
    pub async fn get_latest_summary(
        &self,
        session_key: &SessionKey,
    ) -> Result<Option<SessionEvent>, StoreError> {
        let channel = session_key.channel.to_string();
        let chat_id = &session_key.chat_id;
        let row = sqlx::query_as::<_, EventRow>(
            r#"
            SELECT * FROM session_events
            WHERE channel = ? AND chat_id = ? AND event_type = 'summary'
            ORDER BY sequence DESC
            LIMIT 1
            "#,
        )
        .bind(&channel)
        .bind(chat_id)
        .fetch_optional(&self.pool)
        .await?;

        let found = row.is_some();
        let result = row.map(|r| r.try_into()).transpose();
        debug!("Latest summary for {}: found={}", session_key, found);
        result
    }

    /// Query all sessions for a given channel.
    ///
    /// Returns session keys ordered by updated_at descending.
    pub async fn get_sessions_by_channel(&self, channel: &str) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT key FROM sessions_v2 WHERE channel = ? ORDER BY updated_at DESC",
        )
        .bind(channel)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(k,)| k).collect())
    }

    /// Query all sessions for a given chat_id across channels.
    ///
    /// Returns session keys ordered by updated_at descending.
    pub async fn get_sessions_by_chat_id(&self, chat_id: &str) -> Result<Vec<String>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT key FROM sessions_v2 WHERE chat_id = ? ORDER BY updated_at DESC",
        )
        .bind(chat_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(k,)| k).collect())
    }

    /// Get all user/assistant events across all sessions.
    ///
    /// Used for embedding backfill when the embedding store is empty.
    pub async fn get_all_events(&self) -> Result<Vec<SessionEvent>, StoreError> {
        self.get_recent_events(0).await
    }

    /// Get recent user/assistant events across all sessions.
    ///
    /// `limit = 0` means no limit (all events).
    /// Used for hot-index backfill with a bounded memory footprint.
    pub async fn get_recent_events(&self, limit: usize) -> Result<Vec<SessionEvent>, StoreError> {
        let sql = if limit > 0 {
            r#"
            SELECT * FROM session_events
            WHERE event_type IN ('user_message', 'assistant_message')
            ORDER BY created_at ASC
            LIMIT ?
            "#
        } else {
            r#"
            SELECT * FROM session_events
            WHERE event_type IN ('user_message', 'assistant_message')
            ORDER BY created_at ASC
            "#
        };
        let rows: Vec<EventRow> = if limit > 0 {
            sqlx::query_as(sql)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_as(sql).fetch_all(&self.pool).await?
        };
        let events: Vec<SessionEvent> = rows
            .into_iter()
            .map(|r| r.try_into())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(events)
    }

    /// Search session events by keyword using SQL LIKE.
    ///
    /// Returns matching `user_message` and `assistant_message` events
    /// ordered by creation time (newest first).
    pub async fn search_session_events(
        &self,
        session_key: &SessionKey,
        keyword: &str,
        limit: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let pattern = format!(
            "%{}%",
            keyword
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );

        let rows: Vec<EventRow> = sqlx::query_as(
            r#"
            SELECT id, session_key, channel, chat_id, event_type, content,
                   tools_used, token_usage, token_len, event_data, extra, created_at, sequence
            FROM session_events
            WHERE session_key = ?1
              AND content LIKE ?2 ESCAPE '\'
              AND event_type IN ('user_message', 'assistant_message')
            ORDER BY created_at DESC
            LIMIT ?3
            "#,
        )
        .bind(session_key.to_string())
        .bind(&pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(TryInto::try_into).collect()
    }
}

#[async_trait]
impl EventStoreTrait for EventStore {
    async fn append(&self, event: &SessionEvent) -> Result<i64, StoreError> {
        self.append_event_internal(event).await
    }

    async fn get_session_history(&self, key: &SessionKey) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::get_session_history(self, key).await
    }

    async fn get_events_after_sequence(
        &self,
        key: &SessionKey,
        after: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::get_events_after_sequence(self, key, after).await
    }

    async fn get_events_up_to_sequence(
        &self,
        key: &SessionKey,
        target: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::get_events_up_to_sequence(self, key, target).await
    }

    async fn get_max_sequence(&self, key: &SessionKey) -> Result<i64, StoreError> {
        EventStore::get_max_sequence(self, key).await
    }

    async fn get_event_ids_up_to(
        &self,
        key: &SessionKey,
        up_to: i64,
    ) -> Result<Vec<String>, StoreError> {
        EventStore::get_event_ids_up_to(self, key, up_to).await
    }

    async fn delete_events_upto(&self, key: &SessionKey, target: i64) -> Result<u64, StoreError> {
        EventStore::delete_events_upto(self, key, target).await
    }

    async fn get_events_by_ids(
        &self,
        key: &SessionKey,
        ids: &[Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::get_events_by_ids(self, key, ids).await
    }

    async fn get_events_by_ids_global(
        &self,
        ids: &[Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::get_events_by_ids_global(self, ids).await
    }

    async fn get_recent_events(&self, limit: usize) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::get_recent_events(self, limit).await
    }

    async fn search_session_events(
        &self,
        key: &SessionKey,
        keyword: &str,
        limit: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        EventStore::search_session_events(self, key, keyword, limit).await
    }

    async fn get_latest_summary(
        &self,
        key: &SessionKey,
    ) -> Result<Option<SessionEvent>, StoreError> {
        EventStore::get_latest_summary(self, key).await
    }

    async fn clear_session(&self, key: &SessionKey) -> Result<(), StoreError> {
        EventStore::clear_session(self, key).await
    }

    async fn get_sessions_by_channel(&self, channel: &str) -> Result<Vec<String>, StoreError> {
        EventStore::get_sessions_by_channel(self, channel).await
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.tx.subscribe()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct EventRow {
    id: String,
    session_key: String,
    channel: String,
    chat_id: String,
    event_type: String,
    content: String,
    tools_used: String,
    token_usage: Option<String>,
    token_len: i64,
    event_data: Option<String>,
    extra: String,
    created_at: String,
    sequence: i64,
}

impl TryFrom<EventRow> for SessionEvent {
    type Error = StoreError;

    fn try_from(row: EventRow) -> Result<Self, Self::Error> {
        let event_type = match row.event_type.as_str() {
            "user_message" => EventType::UserMessage,
            "assistant_message" => EventType::AssistantMessage,
            "tool_call" | "tool_result" | "summary" => {
                serde_json::from_str(row.event_data.as_deref().unwrap_or("\"\""))?
            }
            _ => return Err(StoreError::InvalidEventType(row.event_type)),
        };

        let tools_used: Vec<String> = serde_json::from_str(&row.tools_used)?;
        let token_usage: Option<TokenUsage> = row
            .token_usage
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let extra: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&row.extra)?;

        // Reconstruct session_key from channel/chat_id for backward compatibility
        let session_key = if !row.channel.is_empty() && !row.chat_id.is_empty() {
            format!("{}:{}", row.channel, row.chat_id)
        } else {
            row.session_key
        };

        Ok(SessionEvent {
            id: row
                .id
                .parse()
                .map_err(|_| StoreError::InvalidUuid(row.id.clone()))?,
            session_key,
            event_type,
            content: row.content,
            metadata: EventMetadata {
                tools_used,
                token_usage,
                content_token_len: row.token_len as usize,
                extra,
            },
            created_at: DateTime::parse_from_rfc3339(&row.created_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            sequence: row.sequence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gasket_types::{EventMetadata, EventType, SummaryType};
    use sqlx::sqlite::SqlitePoolOptions;
    use uuid::Uuid;

    async fn setup_test_db() -> SqlitePool {
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

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_events_channel_chat ON session_events(channel, chat_id)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_events_channel_chat_sequence ON session_events(channel, chat_id, sequence)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_sessions_v2_channel_chat ON sessions_v2(channel, chat_id)",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn test_append_user_message() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let event = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::UserMessage,
            content: "Hello, world!".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };

        store.append_event(&event).await.unwrap();

        let count: (i32,) = sqlx::query_as("SELECT COUNT(*) FROM session_events")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_append_tool_call() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let event = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::ToolCall {
                tool_name: "read_file".into(),
                arguments: serde_json::json!({"path": "/test.txt"}),
            },
            content: "".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };

        store.append_event(&event).await.unwrap();

        let history = store
            .get_session_history(&SessionKey::parse("test:session").unwrap())
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        match &history[0].event_type {
            EventType::ToolCall {
                tool_name,
                arguments,
            } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(arguments, &serde_json::json!({"path": "/test.txt"}));
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[tokio::test]
    async fn test_append_tool_result() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let event = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::ToolResult {
                tool_call_id: "call_123".into(),
                tool_name: "read_file".into(),
                is_error: false,
            },
            content: "file contents".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };

        store.append_event(&event).await.unwrap();

        let history = store
            .get_session_history(&SessionKey::parse("test:session").unwrap())
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        match &history[0].event_type {
            EventType::ToolResult {
                tool_call_id,
                tool_name,
                is_error,
            } => {
                assert_eq!(tool_call_id, "call_123");
                assert_eq!(tool_name, "read_file");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[tokio::test]
    async fn test_append_summary_event() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let covered_ids = vec![Uuid::now_v7(), Uuid::now_v7()];
        let event = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::Summary {
                summary_type: SummaryType::Topic {
                    topic: "discussion about API".into(),
                },
                covered_event_ids: covered_ids.clone(),
            },
            content: "Summary of the discussion...".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };

        store.append_event(&event).await.unwrap();

        let history = store
            .get_session_history(&SessionKey::parse("test:session").unwrap())
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        match &history[0].event_type {
            EventType::Summary {
                summary_type,
                covered_event_ids,
            } => {
                match summary_type {
                    SummaryType::Topic { topic } => assert_eq!(topic, "discussion about API"),
                    _ => panic!("Expected Topic"),
                }
                assert_eq!(covered_event_ids, &covered_ids);
            }
            _ => panic!("Expected Summary"),
        }
    }

    #[tokio::test]
    async fn test_session_auto_created() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let event = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "auto:session".into(),
            event_type: EventType::UserMessage,
            content: "Test".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };

        store.append_event(&event).await.unwrap();

        let count: (i32,) = sqlx::query_as("SELECT COUNT(*) FROM sessions_v2")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);

        let total_events: (i32,) =
            sqlx::query_as("SELECT total_events FROM sessions_v2 WHERE key = 'auto:session'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(total_events.0, 1);
    }

    #[tokio::test]
    async fn test_get_session_history() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let e1 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::UserMessage,
            content: "Hello".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e1).await.unwrap();

        let e2 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::AssistantMessage,
            content: "Hi!".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e2).await.unwrap();

        let history = store
            .get_session_history(&SessionKey::parse("test:session").unwrap())
            .await
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "Hello");
        assert_eq!(history[1].content, "Hi!");
    }

    #[tokio::test]
    async fn test_get_session_history_empty() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let history = store
            .get_session_history(&SessionKey::parse("nonexistent:session").unwrap())
            .await
            .unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_get_events_by_ids() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let e1 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::UserMessage,
            content: "Event 1".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e1).await.unwrap();

        let e2 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::AssistantMessage,
            content: "Event 2".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e2).await.unwrap();

        let e3 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::UserMessage,
            content: "Event 3".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e3).await.unwrap();

        let key = SessionKey::parse("test:session").unwrap();
        let events = store
            .get_events_by_ids(&key, &[e1.id, e3.id])
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "Event 1");
        assert_eq!(events[1].content, "Event 3");

        let events = store
            .get_events_by_ids(&key, &[Uuid::now_v7()])
            .await
            .unwrap();
        assert!(events.is_empty());

        let events = store.get_events_by_ids(&key, &[]).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn test_clear_session() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool.clone());

        let e1 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::UserMessage,
            content: "Event 1".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e1).await.unwrap();

        let e2 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::AssistantMessage,
            content: "Event 2".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e2).await.unwrap();

        let count: (i32,) = sqlx::query_as("SELECT COUNT(*) FROM session_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2);

        let session_count: (i32,) = sqlx::query_as("SELECT COUNT(*) FROM sessions_v2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(session_count.0, 1);

        store
            .clear_session(&SessionKey::parse("test:session").unwrap())
            .await
            .unwrap();

        let count: (i32,) = sqlx::query_as("SELECT COUNT(*) FROM session_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);

        let session_count: (i32,) = sqlx::query_as("SELECT COUNT(*) FROM sessions_v2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(session_count.0, 0);
    }

    #[tokio::test]
    async fn test_get_events_by_ids_global() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        // Create events in two different sessions
        let e1 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session_a".into(),
            event_type: EventType::UserMessage,
            content: "From session A".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e1).await.unwrap();

        let e2 = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session_b".into(),
            event_type: EventType::AssistantMessage,
            content: "From session B".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e2).await.unwrap();

        // Global query returns both events regardless of session
        let events = store
            .get_events_by_ids_global(&[e1.id, e2.id])
            .await
            .unwrap();
        assert_eq!(events.len(), 2);

        // Also works with just one ID
        let events = store.get_events_by_ids_global(&[e1.id]).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "From session A");

        // Non-existent ID returns empty
        let events = store
            .get_events_by_ids_global(&[Uuid::now_v7()])
            .await
            .unwrap();
        assert!(events.is_empty());

        // Empty input returns empty
        let events = store.get_events_by_ids_global(&[]).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn test_get_event_ids_up_to() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let key = SessionKey::parse("test:session").unwrap();

        // Insert 5 events; append_event_internal assigns sequential sequences
        let mut ids = vec![];
        for i in 0..5 {
            let e = SessionEvent {
                id: Uuid::now_v7(),
                session_key: "test:session".into(),
                event_type: EventType::UserMessage,
                content: format!("Event {i}"),
                metadata: EventMetadata::default(),
                created_at: Utc::now(),
                sequence: 0,
            };
            ids.push(e.id);
            store.append_event(&e).await.unwrap();
        }

        // Query IDs up to sequence 2 (should return events with seq 0, 1, 2)
        let result = store.get_event_ids_up_to(&key, 2).await.unwrap();
        assert_eq!(result.len(), 3);
        for id_str in &result {
            let parsed: Uuid = id_str.parse().unwrap();
            assert!(ids[..3].contains(&parsed));
        }
    }

    #[tokio::test]
    async fn test_get_event_ids_up_to_empty() {
        let pool = setup_test_db().await;
        let store = EventStore::new(pool);

        let key = SessionKey::parse("nonexistent:session").unwrap();

        // No events exist for this session
        let result = store.get_event_ids_up_to(&key, 100).await.unwrap();
        assert!(result.is_empty());

        // Sequence 0 matches nothing even if session exists
        let e = SessionEvent {
            id: Uuid::now_v7(),
            session_key: "test:session".into(),
            event_type: EventType::UserMessage,
            content: "only event".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: 0,
        };
        store.append_event(&e).await.unwrap();

        let key = SessionKey::parse("test:session").unwrap();
        // sequence <= -1 matches nothing (first event has seq 0)
        let result = store.get_event_ids_up_to(&key, -1).await.unwrap();
        assert!(result.is_empty());
    }
}
