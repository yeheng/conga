use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crate::command::builtins::{clear, exit, help, model, new as builtin_new, sessions};
use crate::command::dispatcher::shared_help_snapshot;
use crate::command::{CommandHost, CommandResult, DispatcherBuilder, RouteOutcome};
use gasket_types::{ChannelType, ModelSwitchInfo, SessionKey, SessionSummary};

struct H {
    cleared: Mutex<Vec<SessionKey>>,
    current: Mutex<String>,
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
        self.current.lock().unwrap().clone()
    }
    async fn switch_model(&self, _key: &SessionKey, new: &str) -> Result<ModelSwitchInfo, String> {
        let mut g = self.current.lock().unwrap();
        let prev = g.clone();
        *g = new.to_string();
        Ok(ModelSwitchInfo {
            previous: prev,
            current: new.into(),
        })
    }
}

#[tokio::test]
async fn full_dispatcher_routing_matrix() {
    let dir = tempfile::TempDir::new().unwrap();
    tokio::fs::write(
        dir.path().join("translate.md"),
        "---\n\
name: translate\n\
description: Translate to Mandarin\n\
aliases: [tr]\n\
---\n\
\n\
Translate to Mandarin: {{user_input}}\n",
    )
    .await
    .unwrap();

    let host = Arc::new(H {
        cleared: Mutex::new(vec![]),
        current: Mutex::new("openai/gpt-4.1".into()),
    });
    let snap = shared_help_snapshot();
    let key = SessionKey::new(ChannelType::Cli, "interactive");

    let d = DispatcherBuilder::new()
        .host(host.clone())
        .help_snapshot(snap.clone())
        .user_dir(dir.path().to_path_buf())
        .register_builtin(exit())
        .register_builtin(clear())
        .register_builtin(help(snap.clone()))
        .register_builtin(builtin_new())
        .register_builtin(sessions())
        .register_builtin(model())
        .build()
        .await
        .unwrap();

    // Built-in match
    assert_eq!(
        d.route("/exit", &key).await,
        RouteOutcome::Handled(CommandResult::Quit)
    );

    // Alias
    assert_eq!(
        d.route("/q", &key).await,
        RouteOutcome::Handled(CommandResult::Quit)
    );

    // YAML rewrite
    assert_eq!(
        d.route("/translate Hello", &key).await,
        RouteOutcome::Rewrite {
            prompt: "Translate to Mandarin: Hello\n".into(),
            tool_filter: None,
        }
    );

    // Alias on YAML command
    assert_eq!(
        d.route("/tr World", &key).await,
        RouteOutcome::Rewrite {
            prompt: "Translate to Mandarin: World\n".into(),
            tool_filter: None,
        }
    );

    // Unknown command
    match d.route("/whatisthis", &key).await {
        RouteOutcome::Handled(CommandResult::Error(msg)) => {
            assert!(msg.contains("/whatisthis"));
        }
        other => panic!("{:?}", other),
    }

    // Passthrough
    assert_eq!(
        d.route("plain text", &key).await,
        RouteOutcome::Passthrough("plain text".into())
    );

    // /help lists built-ins and the user command
    match d.route("/help", &key).await {
        RouteOutcome::Handled(CommandResult::Print(text)) => {
            for needle in [
                "/exit",
                "/help",
                "/new",
                "/sessions",
                "/model",
                "/clear",
                "/translate",
            ] {
                assert!(text.contains(needle), "expected {needle} in:\n{text}");
            }
        }
        other => panic!("{:?}", other),
    }

    // /new triggers host.clear_session
    let _ = d.route("/new", &key).await;
    assert_eq!(host.cleared.lock().unwrap().len(), 1);

    // /model with no args shows current
    match d.route("/model", &key).await {
        RouteOutcome::Handled(CommandResult::Print(s)) => {
            assert!(s.contains("openai/gpt-4.1"));
        }
        other => panic!("{:?}", other),
    }

    // /model <id> switches
    match d.route("/model anthropic/claude-4.5-sonnet", &key).await {
        RouteOutcome::Handled(CommandResult::Print(s)) => {
            assert!(s.contains("openai/gpt-4.1"));
            assert!(s.contains("anthropic/claude-4.5-sonnet"));
        }
        other => panic!("{:?}", other),
    }
}
