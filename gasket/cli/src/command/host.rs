//! Bridge trait between the dispatcher and the host application.
//!
//! The `command` module does not depend on `gasket-engine`. Built-in handlers
//! reach engine capabilities (clear session, list sessions, switch model)
//! through this trait. The CLI provides its own implementation.

use async_trait::async_trait;
use gasket_types::{ModelSwitchInfo, SessionKey, SessionSummary};

#[async_trait]
pub trait CommandHost: Send + Sync {
    /// Clear the conversation history for the given session.
    async fn clear_session(&self, key: &SessionKey);

    /// Recent sessions visible to this host, newest first.
    async fn list_sessions(&self) -> Vec<SessionSummary>;

    /// The currently active model id for the given session (e.g. "openai/gpt-4.1").
    async fn current_model(&self, key: &SessionKey) -> String;

    /// Switch the active model for the given session. Returns previous and current ids on success.
    async fn switch_model(&self, key: &SessionKey, new: &str) -> Result<ModelSwitchInfo, String>;

    /// Send a message to a specific channel/chat. Default returns an error.
    #[allow(dead_code)]
    async fn send_message(
        &self,
        _channel: &str,
        _chat_id: &str,
        _content: &str,
    ) -> Result<(), String> {
        Err("send_message not implemented for this host".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_types::ChannelType;
    use std::sync::Mutex;

    pub struct StubHost {
        pub current: Mutex<String>,
        pub cleared: Mutex<Vec<SessionKey>>,
    }

    #[async_trait]
    impl CommandHost for StubHost {
        async fn clear_session(&self, key: &SessionKey) {
            self.cleared.lock().unwrap().push(key.clone());
        }
        async fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }
        async fn current_model(&self, _key: &SessionKey) -> String {
            self.current.lock().unwrap().clone()
        }
        async fn switch_model(
            &self,
            _key: &SessionKey,
            new: &str,
        ) -> Result<ModelSwitchInfo, String> {
            let mut g = self.current.lock().unwrap();
            let previous = g.clone();
            *g = new.to_string();
            Ok(ModelSwitchInfo {
                previous,
                current: new.to_string(),
            })
        }
    }

    #[tokio::test]
    async fn stub_host_round_trip() {
        let host = StubHost {
            current: Mutex::new("a".into()),
            cleared: Mutex::new(vec![]),
        };
        let key = SessionKey::new(ChannelType::Cli, "x");
        let info = host.switch_model(&key, "b").await.unwrap();
        assert_eq!(info.previous, "a");
        assert_eq!(info.current, "b");
        assert_eq!(host.current_model(&key).await, "b");
        host.clear_session(&key).await;
        assert_eq!(host.cleared.lock().unwrap().len(), 1);
    }
}
