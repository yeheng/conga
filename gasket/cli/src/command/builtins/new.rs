use std::sync::Arc;

use futures::FutureExt;
use gasket_types::SessionKey;

use crate::command::host::CommandHost;
use crate::command::types::{Command, CommandKind, CommandResult};

pub fn new() -> Command {
    Command {
        name: "new".into(),
        description: "Start a new conversation".into(),
        aliases: vec![],
        kind: CommandKind::Builtin(Arc::new(
            |_args: &str, host: Arc<dyn CommandHost>, session_key: &SessionKey| {
                async move {
                    host.clear_session(session_key).await;
                    CommandResult::Print(format!("✓ Session cleared ({})", session_key))
                }
                .boxed()
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use gasket_types::{ChannelType, ModelSwitchInfo, SessionKey, SessionSummary};
    use std::sync::Mutex;

    use crate::command::dispatcher::DispatcherBuilder;
    use crate::command::types::RouteOutcome;

    struct H {
        cleared: Mutex<Vec<SessionKey>>,
    }
    #[async_trait]
    impl CommandHost for H {
        async fn clear_session(&self, k: &SessionKey) {
            self.cleared.lock().unwrap().push(k.clone());
        }
        async fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }
        async fn current_model(&self, _key: &SessionKey) -> String {
            "m".into()
        }
        async fn switch_model(
            &self,
            _key: &SessionKey,
            _: &str,
        ) -> Result<ModelSwitchInfo, String> {
            Ok(ModelSwitchInfo {
                previous: "m".into(),
                current: "m".into(),
            })
        }
    }

    #[tokio::test]
    async fn new_calls_clear_session_once_with_correct_key() {
        let key = SessionKey::new(ChannelType::Cli, "interactive");
        let host = Arc::new(H {
            cleared: Mutex::new(vec![]),
        });
        let d = DispatcherBuilder::new()
            .host(host.clone())
            .register_builtin(new())
            .build()
            .await
            .unwrap();

        let outcome = d.route("/new", &key).await;

        match outcome {
            RouteOutcome::Handled(CommandResult::Print(msg)) => {
                assert!(msg.contains("Session cleared"));
            }
            other => panic!("{:?}", other),
        }
        let calls = host.cleared.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], key);
    }
}
