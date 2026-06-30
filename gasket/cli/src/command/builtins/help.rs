use std::sync::Arc;

use futures::FutureExt;

use crate::command::dispatcher::HelpSnapshot;
use crate::command::host::CommandHost;
use crate::command::types::{Command, CommandKind, CommandResult, HelpEntry, HelpSource};
use gasket_types::SessionKey;

pub fn help(snapshot: Arc<HelpSnapshot>) -> Command {
    Command {
        name: "help".into(),
        description: "Show available commands".into(),
        aliases: vec!["?".into()],
        kind: CommandKind::Builtin(Arc::new(
            move |_args: &str, _host: Arc<dyn CommandHost>, _session_key: &SessionKey| {
                let snap = snapshot.clone();
                async move {
                    let entries: &[HelpEntry] = match snap.get() {
                        Some(v) => v,
                        None => {
                            return CommandResult::Error("help snapshot not initialised".into())
                        }
                    };
                    CommandResult::Print(render_help(entries))
                }
                .boxed()
            },
        )),
    }
}

fn render_help(entries: &[HelpEntry]) -> String {
    let (builtin, user): (Vec<&HelpEntry>, Vec<&HelpEntry>) = entries
        .iter()
        .partition(|e| matches!(e.source, HelpSource::Builtin));

    let mut out = String::new();
    out.push_str("Built-in commands:\n");
    for e in &builtin {
        out.push_str(&format_row(e));
    }
    if !user.is_empty() {
        out.push_str("\nUser commands  (~/.gasket/commands):\n");
        for e in &user {
            out.push_str(&format_row(e));
        }
    }
    out
}

fn format_row(e: &HelpEntry) -> String {
    let alias_suffix = if e.aliases.is_empty() {
        String::new()
    } else {
        let aliases: Vec<String> = e.aliases.iter().map(|a| format!("/{a}")).collect();
        format!("  (aliases: {})", aliases.join(", "))
    };
    format!("  /{:<11} {}{}\n", e.name, e.description, alias_suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::builtins::{clear, exit};
    use crate::command::dispatcher::{shared_help_snapshot, DispatcherBuilder};
    use crate::command::host::CommandHost;
    use crate::command::types::RouteOutcome;
    use async_trait::async_trait;
    use gasket_types::{ChannelType, ModelSwitchInfo, SessionKey, SessionSummary};
    use std::sync::Arc;

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
    async fn help_lists_registered_commands() {
        let snap = shared_help_snapshot();
        let d = DispatcherBuilder::new()
            .host(Arc::new(H))
            .help_snapshot(snap.clone())
            .register_builtin(exit())
            .register_builtin(clear())
            .register_builtin(help(snap.clone()))
            .build()
            .await
            .unwrap();

        let key = SessionKey::new(ChannelType::Cli, "test");
        match d.route("/help", &key).await {
            RouteOutcome::Handled(CommandResult::Print(text)) => {
                assert!(text.contains("/clear"), "missing /clear: {text}");
                assert!(text.contains("/exit"), "missing /exit: {text}");
                assert!(text.contains("/help"), "missing /help: {text}");
                assert!(text.contains("Clear the terminal screen"));
                assert!(text.contains("Built-in commands:"));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }
}
