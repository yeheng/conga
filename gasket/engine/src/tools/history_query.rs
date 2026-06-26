//! History query tool for searching conversation records.
//!
//! Provides a `query_history` tool that searches the event store
//! (JSON-backed) without relying on an external `sqlite3` CLI binary.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument};

use super::{Tool, ToolContext, ToolError, ToolResult};
use gasket_storage::EventStoreTrait;

/// Query conversation history from the local event store.
///
/// This tool queries recent user/assistant events via the event-store trait,
/// filtering by keyword substring (case-insensitive). No external binary or
/// SQL is involved.
pub struct HistoryQueryTool {
    event_store: Arc<dyn EventStoreTrait>,
}

impl HistoryQueryTool {
    /// Create a new history query tool backed by an event store.
    pub fn new(event_store: Arc<dyn EventStoreTrait>) -> Self {
        Self { event_store }
    }
}

// ── Argument Parsing ───────────────────────────────────────────

#[derive(Deserialize)]
struct QueryArgs {
    /// Optional keywords to search for in message content (case-insensitive).
    keywords: Option<String>,

    /// Maximum number of messages to return (default: 20).
    limit: Option<usize>,
}

#[async_trait]
impl Tool for HistoryQueryTool {
    fn name(&self) -> &str {
        "query_history"
    }

    fn description(&self) -> &str {
        "Query conversation history from the local store. \
         Supports keyword search across all sessions. \
         Works regardless of sandbox policies."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "keywords": {
                    "type": "string",
                    "description": "Optional keywords to search for in message content (case-insensitive)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of messages to return",
                    "default": 20
                }
            },
            "required": []
        })
    }

    #[instrument(name = "tool.query_history", skip_all)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> ToolResult {
        let args: QueryArgs = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let limit = args.limit.unwrap_or(20).min(100);

        debug!(
            "History query tool invoked: keywords={:?}, limit={}",
            args.keywords, limit
        );

        // Load all recent user/assistant events (limit == 0 returns all).
        let all = self
            .event_store
            .get_recent_events(0)
            .await
            .map_err(|e| ToolError::ExecutionError(e.to_string()))?;

        // Optional keyword filter (case-insensitive substring).
        let keyword = args.keywords.map(|k| k.to_lowercase());
        let mut hits: Vec<_> = all
            .into_iter()
            .filter(|e| {
                matches!(
                    e.event_type,
                    gasket_types::EventType::UserMessage
                        | gasket_types::EventType::AssistantMessage
                )
            })
            .filter(|e| match keyword.as_deref() {
                Some(kw) => e.content.to_lowercase().contains(kw),
                None => true,
            })
            .collect();

        // Newest first (by created_at, then sequence as tiebreaker).
        hits.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.sequence.cmp(&a.sequence))
        });
        hits.truncate(limit);

        if hits.is_empty() {
            return Ok("No history found.".to_string());
        }

        let mut lines = vec![format!("Conversation history ({} messages):", hits.len())];

        for e in &hits {
            let role = match e.event_type {
                gasket_types::EventType::UserMessage => "user",
                gasket_types::EventType::AssistantMessage => "assistant",
                _ => "unknown",
            };
            let timestamp = e.created_at.to_rfc3339();
            let preview = if e.content.chars().count() > 400 {
                format!("{}...", e.content.chars().take(400).collect::<String>())
            } else {
                e.content.clone()
            };
            lines.push(format!("\n[{}] {}:\n{}", timestamp, role, preview));
        }

        Ok(lines.join(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_storage::JsonStore;

    #[tokio::test]
    async fn test_history_query_by_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonStore::new(dir.path().to_path_buf()));
        let mk = |content: &str| gasket_types::SessionEvent {
            id: uuid::Uuid::now_v7(),
            session_key: "cli:test".into(),
            event_type: gasket_types::EventType::UserMessage,
            content: content.to_string(),
            metadata: gasket_types::EventMetadata::default(),
            created_at: chrono::Utc::now(),
            sequence: 0,
        };
        store.append(&mk("Hello world")).await.unwrap();
        store.append(&mk("Another message")).await.unwrap();

        let tool = HistoryQueryTool::new(store.clone() as Arc<dyn EventStoreTrait>);
        let args = serde_json::json!({
            "keywords": "Hello",
            "limit": 10,
        });
        let result = tool.execute(args, &ToolContext::default()).await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.contains("Hello world"));
        assert!(text.contains("user"));
    }

    #[tokio::test]
    async fn test_history_query_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonStore::new(dir.path().to_path_buf()));
        let mk = |content: &str| gasket_types::SessionEvent {
            id: uuid::Uuid::now_v7(),
            session_key: "cli:test".into(),
            event_type: gasket_types::EventType::UserMessage,
            content: content.to_string(),
            metadata: gasket_types::EventMetadata::default(),
            created_at: chrono::Utc::now(),
            sequence: 0,
        };
        store.append(&mk("Hello world")).await.unwrap();

        let tool = HistoryQueryTool::new(store.clone() as Arc<dyn EventStoreTrait>);
        let args = serde_json::json!({
            "keywords": "nonexistent",
        });
        let result = tool.execute(args, &ToolContext::default()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No history found"));
    }
}
