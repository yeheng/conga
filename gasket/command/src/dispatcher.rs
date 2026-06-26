//! Slash-command dispatcher.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::error::BuildError;
use crate::host::CommandHost;
use crate::parser::{parse, ParsedInput};
use crate::template::render;
use crate::types::{Command, CommandKind, CommandResult, HelpEntry, HelpSource, RouteOutcome};
use crate::yaml_loader::load_user_commands;
use gasket_types::SessionKey;

/// Lazily-filled snapshot of registered commands. The builder writes once
/// after all commands are known; the `/help` builtin reads from it.
pub type HelpSnapshot = OnceLock<Vec<HelpEntry>>;

/// Construct a fresh, empty help snapshot to share between the builder and
/// the `/help` builtin.
pub fn shared_help_snapshot() -> Arc<HelpSnapshot> {
    Arc::new(OnceLock::new())
}

pub struct Dispatcher {
    pub(crate) commands: HashMap<String, Arc<Command>>,
    pub(crate) aliases: HashMap<String, String>,
    pub(crate) host: Arc<dyn CommandHost>,
}

impl Dispatcher {
    pub async fn route(&self, line: &str, session_key: &SessionKey) -> RouteOutcome {
        match parse(line) {
            ParsedInput::NotCommand => RouteOutcome::Passthrough(line.to_string()),
            ParsedInput::Command { name, args } => self.dispatch(name, args, session_key).await,
        }
    }

    async fn dispatch(&self, name: &str, args: &str, session_key: &SessionKey) -> RouteOutcome {
        let canonical = self.aliases.get(name).map(String::as_str).unwrap_or(name);

        let Some(cmd) = self.commands.get(canonical) else {
            return RouteOutcome::Handled(CommandResult::Error(format!(
                "unknown command: /{name}    (type /help to see commands)"
            )));
        };

        match &cmd.kind {
            CommandKind::Builtin(handler) => {
                RouteOutcome::Handled(handler(args, self.host.clone(), session_key).await)
            }
            CommandKind::Yaml {
                prompt_template,
                allowed_tools,
            } => RouteOutcome::Rewrite {
                prompt: render(prompt_template, args),
                tool_filter: allowed_tools.clone(),
            },
        }
    }

    pub fn list_commands(&self) -> Vec<&Command> {
        let mut v: Vec<&Command> = self.commands.values().map(|a| a.as_ref()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

pub struct DispatcherBuilder {
    builtins: Vec<Command>,
    user_yaml_dir: Option<PathBuf>,
    host: Option<Arc<dyn CommandHost>>,
    help_snapshot: Option<Arc<HelpSnapshot>>,
}

impl Dispatcher {
    pub fn builder() -> DispatcherBuilder {
        DispatcherBuilder::new()
    }
}

impl DispatcherBuilder {
    pub fn new() -> Self {
        Self {
            builtins: Vec::new(),
            user_yaml_dir: None,
            host: None,
            help_snapshot: None,
        }
    }

    pub fn register_builtin(mut self, cmd: Command) -> Self {
        self.builtins.push(cmd);
        self
    }

    pub fn user_dir(mut self, p: PathBuf) -> Self {
        self.user_yaml_dir = Some(p);
        self
    }

    pub fn host(mut self, h: Arc<dyn CommandHost>) -> Self {
        self.host = Some(h);
        self
    }

    pub fn help_snapshot(mut self, slot: Arc<HelpSnapshot>) -> Self {
        self.help_snapshot = Some(slot);
        self
    }

    pub async fn build(self) -> Result<Dispatcher, BuildError> {
        let host = self.host.ok_or(BuildError::MissingHost)?;

        let mut commands: HashMap<String, Arc<Command>> = HashMap::new();
        let mut aliases: HashMap<String, String> = HashMap::new();

        // 1. Built-ins first; duplicates are programmer bugs and fail the build.
        for cmd in self.builtins {
            if commands.contains_key(&cmd.name) {
                return Err(BuildError::DuplicateBuiltin(cmd.name.clone()));
            }
            for a in &cmd.aliases {
                aliases.insert(a.clone(), cmd.name.clone());
            }
            commands.insert(cmd.name.clone(), Arc::new(cmd));
        }

        // 2. User commands; collisions warn-and-drop, never fatal.
        if let Some(dir) = self.user_yaml_dir {
            for cmd in load_user_commands(&dir).await {
                if commands.contains_key(&cmd.name) || aliases.contains_key(&cmd.name) {
                    tracing::warn!(
                        name = cmd.name,
                        "user command name collides with a built-in or earlier user command; dropping"
                    );
                    continue;
                }
                let mut conflicting_alias = false;
                for a in &cmd.aliases {
                    if commands.contains_key(a) || aliases.contains_key(a) {
                        tracing::warn!(
                            name = cmd.name,
                            alias = a,
                            "user command alias collides with an earlier registration; dropping"
                        );
                        conflicting_alias = true;
                        break;
                    }
                }
                if conflicting_alias {
                    continue;
                }
                let arc = Arc::new(cmd);
                for a in &arc.aliases {
                    aliases.insert(a.clone(), arc.name.clone());
                }
                commands.insert(arc.name.clone(), arc);
            }
        }

        // 3. If the caller wants a help snapshot, fill it now.
        if let Some(slot) = self.help_snapshot.clone() {
            let mut entries: Vec<HelpEntry> = commands
                .values()
                .map(|c| HelpEntry {
                    name: c.name.clone(),
                    description: c.description.clone(),
                    aliases: c.aliases.clone(),
                    source: match &c.kind {
                        CommandKind::Builtin(_) => HelpSource::Builtin,
                        CommandKind::Yaml { .. } => HelpSource::User,
                    },
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let _ = slot.set(entries);
        }

        Ok(Dispatcher {
            commands,
            aliases,
            host,
        })
    }
}

impl Default for DispatcherBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BuiltinHandler;
    use async_trait::async_trait;
    use futures::FutureExt;
    use gasket_types::{ChannelType, ModelSwitchInfo, SessionKey, SessionSummary};
    use std::sync::Mutex;

    pub struct MockCommandHost {
        pub clear_calls: Mutex<Vec<SessionKey>>,
    }

    impl MockCommandHost {
        pub fn new() -> Self {
            Self {
                clear_calls: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl CommandHost for MockCommandHost {
        async fn clear_session(&self, key: &SessionKey) {
            self.clear_calls.lock().unwrap().push(key.clone());
        }
        async fn list_sessions(&self) -> Vec<SessionSummary> {
            vec![]
        }
        async fn current_model(&self, _key: &SessionKey) -> String {
            "test-model".into()
        }
        async fn switch_model(
            &self,
            _key: &SessionKey,
            new: &str,
        ) -> Result<ModelSwitchInfo, String> {
            Ok(ModelSwitchInfo {
                previous: "test-model".into(),
                current: new.into(),
            })
        }
    }

    fn echo_handler() -> BuiltinHandler {
        Arc::new(
            |args: &str, _host: Arc<dyn CommandHost>, _key: &SessionKey| {
                let s = format!("echo: {}", args);
                async move { CommandResult::Print(s) }.boxed()
            },
        )
    }

    fn make_dispatcher_with(commands: Vec<Command>) -> Dispatcher {
        let mut map: HashMap<String, Arc<Command>> = HashMap::new();
        for c in commands {
            map.insert(c.name.clone(), Arc::new(c));
        }
        Dispatcher {
            commands: map,
            aliases: HashMap::new(),
            host: Arc::new(MockCommandHost::new()),
        }
    }

    #[tokio::test]
    async fn builtin_match_invokes_handler() {
        let cmd = Command {
            name: "echo".into(),
            description: "echoes args".into(),
            aliases: vec![],
            kind: CommandKind::Builtin(echo_handler()),
        };
        let d = make_dispatcher_with(vec![cmd]);
        let key = SessionKey::new(ChannelType::Cli, "test");

        let outcome = d.route("/echo hello world", &key).await;

        assert_eq!(
            outcome,
            RouteOutcome::Handled(CommandResult::Print("echo: hello world".into()))
        );
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let d = make_dispatcher_with(vec![]);
        let key = SessionKey::new(ChannelType::Cli, "test");
        let outcome = d.route("/whatisthis", &key).await;
        match outcome {
            RouteOutcome::Handled(CommandResult::Error(msg)) => {
                assert!(msg.contains("/whatisthis"), "msg = {msg}");
                assert!(msg.contains("/help"), "should hint /help; msg = {msg}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn non_command_text_passes_through_verbatim() {
        let d = make_dispatcher_with(vec![]);
        let key = SessionKey::new(ChannelType::Cli, "test");
        let outcome = d.route("hello world", &key).await;
        assert_eq!(outcome, RouteOutcome::Passthrough("hello world".into()));
    }

    #[tokio::test]
    async fn alias_resolves_to_canonical() {
        let cmd = Command {
            name: "exit".into(),
            description: "exit".into(),
            aliases: vec!["q".into(), "quit".into()],
            kind: CommandKind::Builtin(Arc::new(|_, _, _| async { CommandResult::Quit }.boxed())),
        };
        let mut map: HashMap<String, Arc<Command>> = HashMap::new();
        let arc = Arc::new(cmd);
        map.insert(arc.name.clone(), arc.clone());
        let mut aliases = HashMap::new();
        aliases.insert("q".into(), "exit".into());
        aliases.insert("quit".into(), "exit".into());
        let d = Dispatcher {
            commands: map,
            aliases,
            host: Arc::new(MockCommandHost::new()),
        };
        let key = SessionKey::new(ChannelType::Cli, "test");

        assert_eq!(
            d.route("/q", &key).await,
            RouteOutcome::Handled(CommandResult::Quit)
        );
        assert_eq!(
            d.route("/quit", &key).await,
            RouteOutcome::Handled(CommandResult::Quit)
        );
    }

    #[tokio::test]
    async fn yaml_kind_produces_rewrite_with_filter() {
        let cmd = Command {
            name: "translate".into(),
            description: "translate".into(),
            aliases: vec![],
            kind: CommandKind::Yaml {
                prompt_template: "Translate to Mandarin: {{user_input}}".into(),
                allowed_tools: Some(vec!["example_tool".into()]),
            },
        };
        let d = make_dispatcher_with(vec![cmd]);
        let key = SessionKey::new(ChannelType::Cli, "test");

        let outcome = d.route("/translate Hello world", &key).await;

        assert_eq!(
            outcome,
            RouteOutcome::Rewrite {
                prompt: "Translate to Mandarin: Hello world".into(),
                tool_filter: Some(vec!["example_tool".into()]),
            }
        );
    }

    #[tokio::test]
    async fn yaml_with_no_tool_filter_passes_none() {
        let cmd = Command {
            name: "review".into(),
            description: "review".into(),
            aliases: vec![],
            kind: CommandKind::Yaml {
                prompt_template: "Review:\n{{user_input}}".into(),
                allowed_tools: None,
            },
        };
        let d = make_dispatcher_with(vec![cmd]);
        let key = SessionKey::new(ChannelType::Cli, "test");

        let outcome = d.route("/review my code", &key).await;

        assert_eq!(
            outcome,
            RouteOutcome::Rewrite {
                prompt: "Review:\nmy code".into(),
                tool_filter: None,
            }
        );
    }

    use crate::error::BuildError;
    use tempfile::TempDir;

    fn make_builtin(name: &str, alias: &[&str]) -> Command {
        Command {
            name: name.into(),
            description: format!("desc-{name}"),
            aliases: alias.iter().map(|s| s.to_string()).collect(),
            kind: CommandKind::Builtin(Arc::new(|_, _, _| {
                async { CommandResult::Print("ok".into()) }.boxed()
            })),
        }
    }

    #[tokio::test]
    async fn build_fails_without_host() {
        let res = DispatcherBuilder::new()
            .register_builtin(make_builtin("help", &[]))
            .build()
            .await;
        assert!(matches!(res, Err(BuildError::MissingHost)));
    }

    #[tokio::test]
    async fn build_fails_on_duplicate_builtin() {
        let res = DispatcherBuilder::new()
            .host(Arc::new(MockCommandHost::new()))
            .register_builtin(make_builtin("help", &[]))
            .register_builtin(make_builtin("help", &[]))
            .build()
            .await;
        assert!(matches!(res, Err(BuildError::DuplicateBuiltin(_))));
    }

    #[tokio::test]
    async fn user_yaml_colliding_with_builtin_is_dropped() {
        let dir = TempDir::new().unwrap();
        let yaml_help = "---\nname: help\ndescription: bogus help\n---\nbody\n";
        tokio::fs::write(dir.path().join("help.md"), yaml_help)
            .await
            .unwrap();

        let d = DispatcherBuilder::new()
            .host(Arc::new(MockCommandHost::new()))
            .user_dir(dir.path().to_path_buf())
            .register_builtin(make_builtin("help", &[]))
            .build()
            .await
            .unwrap();

        let help = d.commands.get("help").unwrap();
        assert_eq!(help.description, "desc-help");
    }

    #[tokio::test]
    async fn two_user_yamls_with_same_name_first_wins() {
        let dir = TempDir::new().unwrap();
        let body_a = "---\nname: foo\ndescription: from-a\n---\nbody-a\n";
        let body_z = "---\nname: foo\ndescription: from-z\n---\nbody-z\n";
        tokio::fs::write(dir.path().join("a.md"), body_a)
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("z.md"), body_z)
            .await
            .unwrap();

        let d = DispatcherBuilder::new()
            .host(Arc::new(MockCommandHost::new()))
            .user_dir(dir.path().to_path_buf())
            .build()
            .await
            .unwrap();

        let foo = d.commands.get("foo").unwrap();
        assert_eq!(foo.description, "from-a");
    }

    #[tokio::test]
    async fn build_smoke_with_host_and_one_builtin() {
        let d = DispatcherBuilder::new()
            .host(Arc::new(MockCommandHost::new()))
            .register_builtin(make_builtin("ping", &[]))
            .build()
            .await
            .unwrap();
        assert!(d.commands.contains_key("ping"));
    }
}
