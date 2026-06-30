//! Bridge from `crate::command::CommandHost` to `AgentSession`.

use std::sync::Arc;

use async_trait::async_trait;
use crate::command::CommandHost;
use gasket_engine::broker::{BrokerPayload, MemoryBroker, Topic};
use gasket_engine::session::AgentSession;
use gasket_types::{ModelSwitchInfo, SessionKey, SessionSummary};

#[allow(dead_code)]
pub struct CliCommandHost {
    pub agent: Arc<AgentSession>,
    pub broker: Option<Arc<MemoryBroker>>,
}

#[allow(dead_code)]
impl CliCommandHost {
    pub fn new(agent: Arc<AgentSession>, broker: Option<Arc<MemoryBroker>>) -> Self {
        Self { agent, broker }
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
        let broker = self.broker.as_ref().ok_or("Broker not available")?;
        let channel_type: gasket_types::events::ChannelType = channel.into();
        let outbound =
            gasket_types::events::OutboundMessage::new(channel_type, chat_id, content.to_string());
        let envelope = gasket_engine::broker::Envelope::new(
            Topic::Outbound,
            BrokerPayload::Outbound(outbound),
        );
        broker
            .publish(envelope)
            .await
            .map_err(|e| format!("Broker publish failed: {e}"))?;
        Ok(())
    }
}
