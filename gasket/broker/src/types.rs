//! Core message types for the broker.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gasket_types::events::{InboundMessage, OutboundMessage};

/// Strongly-typed topic enum. Rejects stringly-typed routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Topic {
    #[default]
    Inbound,
    Outbound,
    SystemEvent,
    ToolCall(String),
    LlmRequest,
    Stream(String),
    CronTrigger,
    Heartbeat,
    Custom(String),
}

impl Topic {
    /// Validate and create a ToolCall topic.
    /// Rejects empty tool names.
    pub fn tool_call(name: impl Into<String>) -> Result<Self, &'static str> {
        let name = name.into();
        if name.is_empty() {
            return Err("ToolCall topic name cannot be empty");
        }
        Ok(Topic::ToolCall(name))
    }

    /// Validate and create a Custom topic.
    /// Rejects empty custom names.
    pub fn custom(name: impl Into<String>) -> Result<Self, &'static str> {
        let name = name.into();
        if name.is_empty() {
            return Err("Custom topic name cannot be empty");
        }
        Ok(Topic::Custom(name))
    }

    pub fn delivery_mode(&self) -> DeliveryMode {
        match self {
            Topic::SystemEvent => DeliveryMode::Broadcast,
            _ => DeliveryMode::PointToPoint,
        }
    }
}

/// Compile-time decision per topic — not a runtime guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Message consumed by exactly one subscriber (work-stealing)
    PointToPoint,
    /// Message delivered to all subscribers
    Broadcast,
}

/// Zero-cost in-process payload. Eliminates serde_json from the hot path.
#[derive(Debug, Clone, PartialEq)]
pub enum BrokerPayload {
    Inbound(InboundMessage),
    Outbound(OutboundMessage),
    /// Subagent started executing a task.
    SubagentStarted {
        id: String,
        task: String,
        model: Option<String>,
    },
    /// Subagent finished executing a task.
    SubagentCompleted {
        id: String,
        task: String,
        model: Option<String>,
        success: bool,
        tool_count: usize,
    },
}

/// Pure data envelope — no callbacks, no channels, fully Clone-safe.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub id: uuid::Uuid,
    pub timestamp: u64,
    pub topic: Topic,
    pub payload: Arc<BrokerPayload>,
}

impl Envelope {
    /// Quick construction — auto-generates ID and timestamp.
    pub fn new(topic: Topic, payload: BrokerPayload) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            topic,
            payload: Arc::new(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gasket_types::events::ChannelType;

    fn dummy_inbound(content: &str) -> InboundMessage {
        InboundMessage {
            channel: ChannelType::Cli,
            sender_id: "test".into(),
            chat_id: "test".into(),
            content: content.into(),
            media: None,
            metadata: None,
            timestamp: Utc::now(),
            trace_id: None,
        }
    }

    #[test]
    fn topic_default_is_inbound() {
        assert_eq!(Topic::default(), Topic::Inbound);
    }

    #[test]
    fn topic_tool_call_valid() {
        assert!(Topic::tool_call("search").is_ok());
        assert_eq!(
            Topic::tool_call("search").unwrap(),
            Topic::ToolCall("search".into())
        );
    }

    #[test]
    fn topic_tool_call_rejects_empty() {
        assert!(Topic::tool_call("").is_err());
    }

    #[test]
    fn topic_custom_valid() {
        assert!(Topic::custom("my_topic").is_ok());
    }

    #[test]
    fn topic_custom_rejects_empty() {
        assert!(Topic::custom("").is_err());
    }

    #[test]
    fn delivery_mode_system_event_is_broadcast() {
        assert_eq!(Topic::SystemEvent.delivery_mode(), DeliveryMode::Broadcast);
    }

    #[test]
    fn delivery_mode_inbound_is_p2p() {
        assert_eq!(Topic::Inbound.delivery_mode(), DeliveryMode::PointToPoint);
    }

    #[test]
    fn delivery_mode_outbound_is_p2p() {
        assert_eq!(Topic::Outbound.delivery_mode(), DeliveryMode::PointToPoint);
    }

    #[test]
    fn envelope_new_generates_id_and_timestamp() {
        let env = Envelope::new(
            Topic::Inbound,
            BrokerPayload::Inbound(dummy_inbound("hello")),
        );
        assert!(!env.id.is_nil());
        assert!(env.timestamp > 0);
        assert_eq!(env.topic, Topic::Inbound);
        match env.payload.as_ref() {
            BrokerPayload::Inbound(msg) => assert_eq!(msg.content, "hello"),
            _ => panic!("expected Inbound payload"),
        }
    }

    #[test]
    fn envelope_is_clone_safe() {
        let env = Envelope::new(
            Topic::Outbound,
            BrokerPayload::Outbound(OutboundMessage::new(ChannelType::Cli, "chat1", "hello")),
        );
        let cloned = env.clone();
        assert_eq!(env.id, cloned.id);
        assert!(matches!(
            cloned.payload.as_ref(),
            BrokerPayload::Outbound(msg) if msg.content() == "hello"
        ));
    }
}
