//! Error types for the agent core.
//!
//! Populated in stage 3b.

use thiserror::Error;

/// Top-level error returned by `run_agent_loop`.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("corrupt transcript: {0}")]
    Transcript(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid session id: {0}")]
    InvalidSessionId(String),
    #[error("a turn is already running on this host")]
    TurnInProgress,
    #[error("context transform failed: {0}")]
    ContextTransform(String),
}

/// Error returned by a tool's `execute` closure.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("{0}")]
    Message(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

impl From<ToolError> for AgentError {
    fn from(e: ToolError) -> Self {
        AgentError::Tool(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain tool message becomes `AgentError::Tool` carrying the message
    /// verbatim - the tool error kind is flattened into the string.
    #[test]
    fn tool_error_message_converts_to_agent_error() {
        let err = AgentError::from(ToolError::Message("boom".into()));
        match err {
            AgentError::Tool(msg) => assert_eq!(msg, "boom"),
            other => panic!("expected AgentError::Tool, got {other:?}"),
        }
        assert_eq!(
            AgentError::Tool("boom".into()).to_string(),
            "tool error: boom"
        );
    }

    /// The io/serde variants keep their kind in the flattened message, so a
    /// caller can still tell an io failure from a serialization failure.
    #[test]
    fn tool_error_io_and_serde_keep_their_kind() {
        let io = AgentError::from(ToolError::Io(std::io::Error::other("disk full")));
        match io {
            AgentError::Tool(msg) => assert!(msg.starts_with("io error:"), "{msg}"),
            other => panic!("expected AgentError::Tool, got {other:?}"),
        }

        let json_err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let serde = AgentError::from(ToolError::Serde(json_err));
        match serde {
            AgentError::Tool(msg) => assert!(msg.starts_with("serialization error:"), "{msg}"),
            other => panic!("expected AgentError::Tool, got {other:?}"),
        }
    }

    /// An empty tool message still converts (no panic, no default text).
    #[test]
    fn empty_tool_error_message_converts_to_empty_payload() {
        let err = AgentError::from(ToolError::Message(String::new()));
        match err {
            AgentError::Tool(msg) => assert_eq!(msg, ""),
            other => panic!("expected AgentError::Tool, got {other:?}"),
        }
    }

    /// The error Display strings name the failure kind for host logs.
    #[test]
    fn agent_error_display_names_the_kind() {
        assert_eq!(
            AgentError::Provider("p".into()).to_string(),
            "provider error: p"
        );
        assert_eq!(
            AgentError::ToolNotFound("t".into()).to_string(),
            "tool not found: t"
        );
        assert_eq!(
            AgentError::Transcript("c".into()).to_string(),
            "corrupt transcript: c"
        );
        assert_eq!(
            AgentError::InvalidSessionId("s".into()).to_string(),
            "invalid session id: s"
        );
        assert_eq!(
            AgentError::TurnInProgress.to_string(),
            "a turn is already running on this host"
        );
        assert_eq!(
            AgentError::ContextTransform("x".into()).to_string(),
            "context transform failed: x"
        );
    }
}
