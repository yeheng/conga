//! Core data types for the dispatcher.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::command::host::CommandHost;
use gasket_types::SessionKey;

/// A registered command, either a built-in Rust handler or a user YAML entry.
pub struct Command {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub kind: CommandKind,
}

pub enum CommandKind {
    Builtin(BuiltinHandler),
    Yaml {
        prompt_template: String,
        allowed_tools: Option<Vec<String>>,
    },
}

pub type BuiltinHandler = Arc<
    dyn for<'a> Fn(&'a str, Arc<dyn CommandHost>, &'a SessionKey) -> BoxFuture<'a, CommandResult>
        + Send
        + Sync,
>;

/// Top-level result of `Dispatcher::route`.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteOutcome {
    Handled(CommandResult),
    Rewrite {
        prompt: String,
        tool_filter: Option<Vec<String>>,
    },
    Passthrough(String),
}

/// What a built-in handler asks the caller to do after it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandResult {
    Print(String),
    Quit,
    Error(String),
}

/// One row in the `/help` output.
#[derive(Debug, Clone, PartialEq)]
pub struct HelpEntry {
    pub name: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub source: HelpSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HelpSource {
    Builtin,
    User,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_outcome_variants_construct() {
        let _h = RouteOutcome::Handled(CommandResult::Quit);
        let _r = RouteOutcome::Rewrite {
            prompt: "x".into(),
            tool_filter: None,
        };
        let _p = RouteOutcome::Passthrough("y".into());
    }
}
