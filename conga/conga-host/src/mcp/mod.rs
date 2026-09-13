//! MCP (Model Context Protocol) client — connect external MCP tool servers.
//!
//! Legacy-era stdio client (protocol version `2025-06-18`): spawns an MCP
//! server as a subprocess, runs the `initialize` handshake, discovers tools
//! via `tools/list`, and wraps each as a [`ToolDefinition`]. Tool invocation
//! sends `tools/call`, matching the response by JSON-RPC `id` while skipping
//! server-sent notifications.
//!
//! Configuration: `~/.conga/mcp.json` (or `$CONGA_MCP_CONFIG`), Claude-Desktop
//! style `{"mcpServers": {name: {command, args, env}}}`. Parallel to
//! [`crate::external_tool::ExternalToolBridge`]; both produce `Vec<ToolDefinition>`.
//!
//! ## Serialization constraint (both transports)
//!
//! Calls to ONE server are fully serialized (stdio: one connection-level
//! mutex held across request+response; HTTP: a per-client id mutex). This
//! is a deliberate simplification — MCP servers are frequently
//! single-threaded subprocesses — NOT an accident. Parallel fan-out across
//! *different* servers is unaffected.
//!
//! Split by concern: [`config`] (`mcp.json`), `types` (shared JSON-RPC/tool
//! wire types), `stdio` (subprocess transport), `http` (Streamable HTTP
//! transport).

mod config;
mod http;
mod stdio;
mod types;

use std::time::Duration;

use conga::ToolDefinition;

pub use config::{load_config, load_config_from, McpServerConfig};
pub use http::McpHttpClient;
pub use stdio::McpBridge;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("call timeout after {0:?}")]
    Timeout(Duration),
    #[error("server returned error {code}: {message}")]
    ServerError { code: i64, message: String },
}

/// Spawn every configured MCP server; collect tools. Per-server failures are
/// logged and skipped (matching `load_external_tools`'s tolerance).
///
/// Dispatches by transport: `url` → Streamable HTTP, `command` → stdio.
pub async fn load_all_mcp() -> Vec<ToolDefinition> {
    let configs = match load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("mcp config load failed: {e}");
            return Vec::new();
        }
    };
    let timeout = mcp_call_timeout();
    let mut tools = Vec::new();
    for (name, cfg) in configs {
        let defs: Vec<ToolDefinition> = if cfg.is_http() {
            let url = cfg.url.as_ref().expect("is_http guarantees url");
            match McpHttpClient::connect(&name, url, &cfg.headers, timeout).await {
                Ok((_, defs)) => defs,
                Err(e) => {
                    tracing::warn!("mcp {name} load failed: {e}");
                    continue;
                }
            }
        } else {
            let command = cfg.command.as_deref().unwrap_or("");
            match McpBridge::spawn(&name, command, &cfg.args, &cfg.env, timeout).await {
                Ok((_, defs)) => defs,
                Err(e) => {
                    tracing::warn!("mcp {name} load failed: {e}");
                    continue;
                }
            }
        };
        if !defs.is_empty() {
            tracing::info!("mcp {name}: {} tools", defs.len());
        }
        tools.extend(defs);
    }
    tools
}

/// Read the MCP call timeout from `CONGA_MCP_CALL_TIMEOUT_S` (default 60s).
fn mcp_call_timeout() -> Duration {
    std::env::var("CONGA_MCP_CALL_TIMEOUT_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// Test-only helper shared by the stdio and HTTP transport test modules.
#[cfg(test)]
pub(crate) mod test_util {
    use std::sync::Arc;

    pub(crate) fn tool_call_ctx_for_test(id: &str, args: serde_json::Value) -> conga::ToolCallCtx {
        conga::ToolCallCtx {
            tool_call_id: id.into(),
            args,
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        }
    }
}
