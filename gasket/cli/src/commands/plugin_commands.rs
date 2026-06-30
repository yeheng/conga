//! Bridge: expose registered tools as slash commands.
//!
//! Any tool in `ToolRegistry` (including plugins) becomes callable as
//! `/toolname [args]` without going through the LLM.

use std::sync::Arc;

use futures_util::FutureExt;
use crate::command::dispatcher::DispatcherBuilder;
use crate::command::types::{Command, CommandKind, CommandResult};
use gasket_engine::broker::{BrokerPayload, MemoryBroker, Topic};
use gasket_engine::tools::{SubagentSpawner, Tool, ToolContext, ToolRegistry};
use gasket_types::events::OutboundMessage;
use gasket_types::SessionKey;

/// Parse raw command-line args into JSON for tool invocation.
///
/// - Empty args          → `{}`
/// - JSON (`{...}`)      → parsed directly
/// - Plain text          → `{ <first_string_prop>: text }` or `{ "input": text }`
pub fn parse_tool_args(args: &str, parameters: &serde_json::Value) -> serde_json::Value {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }

    // Attempt JSON parse first
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(val) = serde_json::from_str(trimmed) {
            return val;
        }
    }

    // Fallback: map plain text to the most relevant string property.
    // Prefer required string fields, then fields named "task" or "input",
    // then the first string property in the schema.
    if let Some(props) = parameters.get("properties").and_then(|p| p.as_object()) {
        let required: Vec<&str> = parameters
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        // 1. First required string field
        for key in &required {
            if let Some(schema) = props.get(*key) {
                if schema.get("type").and_then(|t| t.as_str()) == Some("string") {
                    return serde_json::json!({ *key: trimmed });
                }
            }
        }

        // 2. Well-known primary fields
        for key in &["task", "input"] {
            if let Some(schema) = props.get(*key) {
                if schema.get("type").and_then(|t| t.as_str()) == Some("string") {
                    return serde_json::json!({ *key: trimmed });
                }
            }
        }

        // 3. First string property as ultimate fallback
        for (key, schema) in props {
            if schema.get("type").and_then(|t| t.as_str()) == Some("string") {
                return serde_json::json!({ key: trimmed });
            }
        }
    }

    // Ultimate fallback
    serde_json::json!({ "input": trimmed })
}

/// Turn a single `Tool` into a `Command` that the dispatcher understands.
fn tool_to_command(
    tool: &dyn Tool,
    tool_registry: Arc<ToolRegistry>,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    broker: Option<Arc<MemoryBroker>>,
) -> Command {
    let name = tool.name().to_string();
    let description = tool.description().to_string();
    let parameters = tool.parameters();

    Command {
        name: name.clone(),
        description,
        aliases: vec![],
        kind: CommandKind::Builtin(Arc::new(
            move |args: &str, _host: Arc<dyn crate::command::CommandHost>, key: &SessionKey| {
                let tool_registry = tool_registry.clone();
                let name = name.clone();
                let parameters = parameters.clone();
                let spawner = spawner.clone();
                let broker = broker.clone();
                let parsed = parse_tool_args(args, &parameters);
                let _session_key = key.clone();

                async move {
                    // Create a channel so the plugin can send intermediate messages
                    let (outbound_tx, mut outbound_rx) =
                        tokio::sync::mpsc::channel::<OutboundMessage>(32);

                    // Background task: forward plugin messages directly via broker.
                    // Use try_publish so we never block the plugin if the outbound
                    // dispatcher is not running (e.g. CLI mode without WebSocket).
                    let forward_handle = tokio::spawn(async move {
                        while let Some(msg) = outbound_rx.recv().await {
                            if let Some(ref broker) = broker {
                                let envelope = gasket_engine::broker::Envelope::new(
                                    Topic::Outbound,
                                    BrokerPayload::Outbound(msg),
                                );
                                if let Err(e) = broker.try_publish(envelope) {
                                    tracing::warn!("Plugin message forward dropped: {}", e);
                                }
                            }
                        }
                    });

                    let mut ctx = ToolContext::default();
                    if let Some(s) = spawner {
                        ctx = ctx.spawner(s);
                    }
                    ctx = ctx.outbound_tx(outbound_tx);
                    ctx = ctx.session_key(key.clone());

                    let result = match tool_registry.execute(&name, parsed, &ctx).await {
                        Ok(output) => CommandResult::Print(output),
                        Err(e) => CommandResult::Error(format!("Tool error: {e}")),
                    };

                    // Wait a moment for any trailing messages, then drop the sender
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    drop(ctx);
                    let _ = forward_handle.await;
                    result
                }
                .boxed()
            },
        )),
    }
}

/// Register every tool in `tool_registry` as a slash command on the builder.
pub fn register_tool_commands(
    mut builder: DispatcherBuilder,
    tool_registry: Arc<ToolRegistry>,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    broker: Option<Arc<MemoryBroker>>,
) -> DispatcherBuilder {
    for name in tool_registry.list() {
        if let Some(tool) = tool_registry.get(name) {
            let cmd = tool_to_command(tool, tool_registry.clone(), spawner.clone(), broker.clone());
            builder = builder.register_builtin(cmd);
        }
    }
    builder
}
