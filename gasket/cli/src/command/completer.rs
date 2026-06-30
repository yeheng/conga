//! Reedline tab completion for slash commands.

use reedline::{Completer, Span, Suggestion};

use crate::command::Dispatcher;

pub struct CommandCompleter {
    candidates: Vec<String>,
}

impl CommandCompleter {
    pub fn from_dispatcher(d: &Dispatcher) -> Self {
        let mut candidates: Vec<String> = Vec::new();
        for cmd in d.list_commands() {
            candidates.push(format!("/{}", cmd.name));
            for a in &cmd.aliases {
                candidates.push(format!("/{}", a));
            }
        }
        candidates.sort();
        candidates.dedup();
        Self { candidates }
    }
}

impl Completer for CommandCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        if !line.starts_with('/') {
            return vec![];
        }
        let prefix = &line[..pos];
        self.candidates
            .iter()
            .filter(|c| c.starts_with(prefix))
            .map(|c| Suggestion {
                value: c.clone(),
                description: None,
                style: None,
                extra: None,
                span: Span::new(0, pos),
                append_whitespace: true,
                match_indices: None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::builtins::{clear, exit, sessions};
    use crate::command::dispatcher::DispatcherBuilder;
    use crate::command::host::CommandHost;
    use async_trait::async_trait;
    use gasket_types::{ModelSwitchInfo, SessionKey, SessionSummary};
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

    async fn make() -> Dispatcher {
        DispatcherBuilder::new()
            .host(Arc::new(H))
            .register_builtin(exit())
            .register_builtin(clear())
            .register_builtin(sessions())
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn suggests_matching_prefix() {
        let d = make().await;
        let mut c = CommandCompleter::from_dispatcher(&d);
        let suggestions: Vec<String> = c.complete("/cl", 3).into_iter().map(|s| s.value).collect();
        assert!(suggestions.contains(&"/clear".to_string()));
    }

    #[tokio::test]
    async fn no_suggestions_for_plain_text() {
        let d = make().await;
        let mut c = CommandCompleter::from_dispatcher(&d);
        let suggestions = c.complete("clear", 5);
        assert!(suggestions.is_empty());
    }

    #[tokio::test]
    async fn aliases_are_suggested() {
        let d = make().await;
        let mut c = CommandCompleter::from_dispatcher(&d);
        let suggestions: Vec<String> = c.complete("/q", 2).into_iter().map(|s| s.value).collect();
        // /q is itself an alias for /exit
        assert!(suggestions.iter().any(|s| s == "/q" || s == "/quit"));
    }
}
