//! Bridge from `crate::command::CommandHost` to `AgentSession`.

use std::sync::Arc;

use async_trait::async_trait;
use crate::command::CommandHost;
use gasket_engine::session::AgentSession;
use gasket_types::events::OutboundMessage;
use gasket_types::{ModelSwitchInfo, SessionKey, SessionSummary};

#[allow(dead_code)]
pub struct CliCommandHost {
    pub agent: Arc<AgentSession>,
    pub outbound_tx: Option<Arc<tokio::sync::mpsc::Sender<OutboundMessage>>>,
}

#[allow(dead_code)]
impl CliCommandHost {
    pub fn new(
        agent: Arc<AgentSession>,
        outbound_tx: Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
    ) -> Self {
        Self {
            agent,
            outbound_tx: outbound_tx.map(Arc::new),
        }
    }
}

#[async_trait]
impl CommandHost for CliCommandHost {
    async fn clear_session(&self, key: &SessionKey) {
        self.agent.clear_session(key).await;
    }

    async fn list_sessions(&self) -> Vec<SessionSummary> {
        self.agent.list_sessions().await
    }

    async fn current_model(&self, _key: &SessionKey) -> String {
        self.agent.model().to_string()
    }

    async fn switch_model(&self, _key: &SessionKey, new: &str) -> Result<ModelSwitchInfo, String> {
        self.agent.switch_model(new).await
    }

    async fn send_message(
        &self,
        channel: &str,
        chat_id: &str,
        content: &str,
    ) -> Result<(), String> {
        let outbound_tx = self
            .outbound_tx
            .as_ref()
            .ok_or("Outbound channel not available")?;
        let channel_type: gasket_types::events::ChannelType = channel.into();
        let outbound = OutboundMessage::new(channel_type, chat_id, content.to_string());
        outbound_tx
            .send(outbound)
            .await
            .map_err(|e| format!("Outbound send failed: {e}"))?;
        Ok(())
    }
}
