use std::sync::Arc;

use futures::FutureExt;

use crate::command::host::CommandHost;
use crate::command::types::{Command, CommandKind, CommandResult};
use gasket_types::SessionKey;

const ANSI_CLEAR: &str = "\x1B[2J\x1B[H";

pub fn clear() -> Command {
    Command {
        name: "clear".into(),
        description: "Clear the terminal screen".into(),
        aliases: vec![],
        kind: CommandKind::Builtin(Arc::new(
            |_args: &str, _host: Arc<dyn CommandHost>, _session_key: &SessionKey| {
                async { CommandResult::Print(ANSI_CLEAR.to_string()) }.boxed()
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use gasket_types::{ChannelType, ModelSwitchInfo, SessionKey, SessionSummary};
    use std::sync::Arc;

    use crate::command::dispatcher::DispatcherBuilder;
    use crate::command::types::RouteOutcome;

    struct H;

    #[async_trait]
    impl CommandHost for H {
        async fn clear_session(&self, _: &SessionKey) {}
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
    async fn clear_emits_ansi_sequence() {
        let d = DispatcherBuilder::new()
            .host(Arc::new(H))
            .register_builtin(clear())
            .build()
            .await
            .unwrap();
        let key = SessionKey::new(ChannelType::Cli, "test");
        match d.route("/clear", &key).await {
            RouteOutcome::Handled(CommandResult::Print(s)) => {
                assert_eq!(s, "\x1B[2J\x1B[H");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }
}
