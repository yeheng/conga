//! Clear session history tool — wipe all events and summaries for a session.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::info;

use super::{simple_schema, Tool, ToolContext, ToolError, ToolResult};
use gasket_storage::{EventStoreTrait, SessionStoreTrait};
use gasket_types::SessionKey;

/// Tool for clearing all conversation history (events + summaries) for a session.
pub struct ClearSessionTool {
    event_store: Arc<dyn EventStoreTrait>,
    session_store: Arc<dyn SessionStoreTrait>,
}

impl ClearSessionTool {
    /// Create a new clear-session tool backed by the given stores.
    pub fn new(
        event_store: Arc<dyn EventStoreTrait>,
        session_store: Arc<dyn SessionStoreTrait>,
    ) -> Self {
        Self {
            event_store,
            session_store,
        }
    }
}

#[derive(Deserialize)]
struct ClearArgs {
    /// Optional session key to clear (format: "channel:chat_id").
    /// If omitted, the current session is cleared.
    session_key: Option<String>,
}

#[async_trait]
impl Tool for ClearSessionTool {
    fn name(&self) -> &str {
        "clear_session_history"
    }

    fn description(&self) -> &str {
        "Clear all conversation history (events and summaries) for a session. \
         This is destructive and cannot be undone. \
         If no session_key is provided, the current session is cleared."
    }

    fn parameters(&self) -> Value {
        simple_schema(&[(
            "session_key",
            "string",
            false,
            "Optional session key to clear (format: 'channel:chat_id'). \
                 Defaults to the current session if omitted.",
        )])
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> ToolResult {
        let args: ClearArgs = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        let session_key: SessionKey = match args.session_key {
            Some(ref s) => SessionKey::try_parse(s)
                .map_err(|e| ToolError::InvalidArguments(format!("Invalid session_key: {}", e)))?,
            None => ctx.session_key.clone(),
        };

        let key_str = session_key.to_string();
        info!("Clearing session history for {}", key_str);

        // Clear events and session row via EventStore
        self.event_store
            .clear_session(&session_key)
            .await
            .map_err(|e| {
                ToolError::ExecutionError(format!("Failed to clear session events: {}", e))
            })?;

        // Clear summary via SessionStore
        let summary_deleted = self
            .session_store
            .delete_summary(&session_key)
            .await
            .map_err(|e| {
                ToolError::ExecutionError(format!("Failed to delete session summary: {}", e))
            })?;

        Ok(format!(
            "Cleared session `{}`. All events and the session record have been removed. Summary deleted: {}.",
            key_str, summary_deleted
        ))
    }
}
