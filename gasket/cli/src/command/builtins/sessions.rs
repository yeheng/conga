use std::sync::Arc;

use futures::FutureExt;
use gasket_types::{SessionKey, SessionSummary};

use crate::command::host::CommandHost;
use crate::command::types::{Command, CommandKind, CommandResult};

pub fn sessions() -> Command {
    Command {
        name: "sessions".into(),
        description: "List recent sessions".into(),
        aliases: vec!["ls".into()],
        kind: CommandKind::Builtin(Arc::new(
            |_args: &str, host: Arc<dyn CommandHost>, _session_key: &SessionKey| {
                async move {
                    let rows = host.list_sessions().await;
                    CommandResult::Print(render(&rows))
                }
                .boxed()
            },
        )),
    }
}

fn render(rows: &[SessionSummary]) -> String {
    if rows.is_empty() {
        return "No sessions yet.".into();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<30} {:>9}   {}\n",
        "SESSION KEY", "MESSAGES", "LAST ACTIVE"
    ));
    for r in rows {
        let last = match r.last_active {
            Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
            None => "—".into(),
        };
        out.push_str(&format!(
            "{:<30} {:>9}   {}\n",
            r.key.to_string(),
            r.message_count,
            last
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use gasket_types::{ChannelType, ModelSwitchInfo, SessionKey, SessionSummary};
    use std::sync::Arc;

    use crate::command::dispatcher::DispatcherBuilder;
    use crate::command::types::RouteOutcome;

    struct H {
        rows: Vec<SessionSummary>,
    }
    #[async_trait]
    impl CommandHost for H {
        async fn clear_session(&self, _: &SessionKey) {}
        async fn list_sessions(&self) -> Vec<SessionSummary> {
            self.rows.clone()
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
    async fn empty_list_yields_friendly_message() {
        let d = DispatcherBuilder::new()
            .host(Arc::new(H { rows: vec![] }))
            .register_builtin(sessions())
            .build()
            .await
            .unwrap();
        let key = SessionKey::new(ChannelType::Cli, "test");
        match d.route("/sessions", &key).await {
            RouteOutcome::Handled(CommandResult::Print(s)) => {
                assert!(s.starts_with("No sessions"));
            }
            other => panic!("{:?}", other),
        }
    }

    #[tokio::test]
    async fn renders_table() {
        let row = SessionSummary {
            key: SessionKey::new(ChannelType::Cli, "interactive"),
            message_count: 42,
            last_active: Some(Utc.with_ymd_and_hms(2026, 5, 3, 12, 0, 0).unwrap()),
        };
        let d = DispatcherBuilder::new()
            .host(Arc::new(H { rows: vec![row] }))
            .register_builtin(sessions())
            .build()
            .await
            .unwrap();
        let key = SessionKey::new(ChannelType::Cli, "test");
        match d.route("/sessions", &key).await {
            RouteOutcome::Handled(CommandResult::Print(s)) => {
                assert!(s.contains("SESSION KEY"));
                assert!(s.contains("42"));
                assert!(s.contains("interactive"));
                assert!(s.contains("2026-05-03"));
            }
            other => panic!("{:?}", other),
        }
    }
}
