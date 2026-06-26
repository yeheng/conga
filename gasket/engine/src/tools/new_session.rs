//! New session tool — generate a fresh session key and clear history.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::info;
use uuid::Uuid;

use super::{simple_schema, Tool, ToolContext, ToolError, ToolResult};
use gasket_storage::{EventStoreTrait, SessionStoreTrait};
use gasket_types::SessionKey;

/// Tool for starting a new session: clears history and generates a fresh session key.
pub struct NewSessionTool {
    event_store: Arc<dyn EventStoreTrait>,
    session_store: Arc<dyn SessionStoreTrait>,
}

impl NewSessionTool {
    /// Create a new-session tool backed by the given stores.
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

#[async_trait]
impl Tool for NewSessionTool {
    fn name(&self) -> &str {
        "new_session"
    }

    fn description(&self) -> &str {
        "Start a new session with a fresh session key. \
         Clears all conversation history (events and summaries) for the current session, \
         then generates a new unique session key. \
         After this tool returns, the agent should treat subsequent messages as a fresh conversation."
    }

    fn parameters(&self) -> Value {
        simple_schema(&[])
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> ToolResult {
        let old_key = ctx.session_key.clone();
        let old_key_str = old_key.to_string();

        info!("Starting new session, clearing old session {}", old_key_str);

        // Clear events via EventStore
        self.event_store
            .clear_session(&old_key)
            .await
            .map_err(|e| {
                ToolError::ExecutionError(format!("Failed to clear session events: {}", e))
            })?;

        // Clear summary via SessionStore
        self.session_store
            .delete_summary(&old_key)
            .await
            .map_err(|e| {
                ToolError::ExecutionError(format!("Failed to delete session summary: {}", e))
            })?;

        // Generate a new session key with the same channel but a random chat_id
        let new_chat_id = format!("session-{}", Uuid::new_v4());
        let new_key = SessionKey::new(old_key.channel.clone(), &new_chat_id);
        let new_key_str = new_key.to_string();

        info!("New session key generated: {}", new_key_str);

        Ok(format!(
            "New session started.\n\
             Old session `{}` has been cleared (all events and summaries removed).\n\
             New session key: `{}`\n\
             \
             Please use this new session key for all subsequent operations. \
             The conversation context has been reset.",
            old_key_str, new_key_str
        ))
    }
}
