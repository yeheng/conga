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

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use conga::ToolDefinition;

pub use config::{load_config, load_config_from, McpServerConfig};
pub use http::McpHttpClient;
pub use stdio::McpBridge;

use types::McpTool;

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
            match acquire_stdio_server(&name, command, &cfg.args, &cfg.env, timeout).await {
                Ok((bridge, mcp_tools)) => mcp_tools
                    .into_iter()
                    .map(|t| bridge.tool_definition(t))
                    .collect(),
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

/// A cached stdio server: the bridge (weakly) and the tools it advertised.
struct CachedServer {
    bridge: std::sync::Weak<McpBridge>,
    tools: Vec<McpTool>,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// Process-wide cache of the stdio MCP servers this process has spawned.
///
/// The cache holds WEAK references on purpose: a cached server stays alive
/// exactly as long as some tool definition still references it. The
/// consequence is what we want — the subprocess is NOT re-spawned per
/// connection (the gateway builds a session per WebSocket, so ten tabs
/// used to mean ten servers), and `kill_on_drop` still reaps it when the
/// last user goes away, because nothing in the cache keeps it alive.
static STDIO_SERVERS: LazyLock<parking_lot::Mutex<HashMap<String, CachedServer>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Reuse the live server for `name` if there is one for the SAME command,
/// else spawn a fresh one.
async fn acquire_stdio_server(
    name: &str,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    timeout: Duration,
) -> Result<(Arc<McpBridge>, Vec<McpTool>), McpError> {
    if let Some((bridge, tools)) = reuse_stdio_server(name, command, args, env) {
        tracing::info!("mcp {name}: reusing the live server process");
        return Ok((bridge, tools));
    }
    let (bridge, tools) = McpBridge::spawn(name, command, args, env, timeout).await?;
    let mut cache = STDIO_SERVERS.lock();
    cache.insert(
        name.to_string(),
        CachedServer {
            bridge: Arc::downgrade(&bridge),
            tools: tools.clone(),
            command: command.to_string(),
            args: args.to_vec(),
            env: env.clone(),
        },
    );
    Ok((bridge, tools))
}

/// The cached server for `name`, if it is still alive and was started with
/// the same command line.
fn reuse_stdio_server(
    name: &str,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Option<(Arc<McpBridge>, Vec<McpTool>)> {
    let cache = STDIO_SERVERS.lock();
    let entry = cache.get(name)?;
    if entry.command != command || entry.args != args || entry.env != *env {
        // The configuration changed — the running server is stale.
        return None;
    }
    let bridge = entry.bridge.upgrade()?;
    Some((bridge, entry.tools.clone()))
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

    /// Serializes the tests that mutate the process-global MCP env vars
    /// (`CONGA_MCP_CONFIG`, `CONGA_MCP_CALL_TIMEOUT_S`): they are read by
    /// `load_config` / `mcp_call_timeout`, which have no injectable lookup.
    pub(crate) static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// Write a tiny local MCP server (python3) that answers the
    /// `initialize` / `tools/list` / `tools/call` handshake with one tool
    /// named `echo`, and return the script path. The temp dir is kept so
    /// the script outlives this call.
    pub(crate) fn fixture_echo_server_script() -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_echo.py");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "echo-server", "version": "1.0.0"}
        }})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{
            "name": "echo",
            "description": "echo back the arguments",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }
        }]}})
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        text = args.get("text", "")
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": text}],
            "isError": False
        }})
"#,
        )
        .unwrap();
        let kept = dir.keep();
        kept.join("mcp_echo.py").to_string_lossy().into_owned()
    }

    /// Write a tiny local MCP server (python3) that APPENDS ITS PID to
    /// `spawn_log` when it starts — so a test can count how many server
    /// processes were actually spawned.
    pub(crate) fn fixture_pid_logging_server_script(spawn_log: &std::path::Path) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_pid.py");
        std::fs::write(
            &path,
            format!(
                r#"#!/usr/bin/env python3
import sys, json, os

with open({log:?}, "a") as f:
    f.write(str(os.getpid()) + "\n")

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")

    if method == "initialize":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{
            "protocolVersion": "2025-06-18",
            "capabilities": {{"tools": {{"listChanged": False}}}},
            "serverInfo": {{"name": "pid-server", "version": "1.0.0"}}
        }}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({{"jsonrpc": "2.0", "id": mid, "result": {{"tools": [{{
            "name": "echo",
            "description": "echo back the arguments",
            "inputSchema": {{
                "type": "object",
                "properties": {{"text": {{"type": "string"}}}},
                "required": ["text"]
            }}
        }}]}}}})
    elif method == "tools/call":
        args = msg.get("params", {{}}).get("arguments", {{}})
        text = args.get("text", "")
        send({{"jsonrpc": "2.0", "id": mid, "result": {{
            "content": [{{"type": "text", "text": text}}],
            "isError": False
        }}}})
"#,
                log = spawn_log.to_string_lossy()
            ),
        )
        .unwrap();
        let kept = dir.keep();
        kept.join("mcp_pid.py").to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{
        fixture_echo_server_script, fixture_pid_logging_server_script, ENV_LOCK,
    };
    use super::*;

    /// Scoped, process-global MCP env (`CONGA_MCP_CONFIG` /
    /// `CONGA_MCP_CALL_TIMEOUT_S`); restored on drop.
    struct McpEnv {
        saved_config: Option<String>,
        saved_timeout: Option<String>,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    impl McpEnv {
        fn apply(config: Option<&std::path::Path>, timeout: Option<&str>) {
            match config {
                Some(p) => std::env::set_var("CONGA_MCP_CONFIG", p),
                None => std::env::remove_var("CONGA_MCP_CONFIG"),
            }
            match timeout {
                Some(v) => std::env::set_var("CONGA_MCP_CALL_TIMEOUT_S", v),
                None => std::env::remove_var("CONGA_MCP_CALL_TIMEOUT_S"),
            }
        }

        /// Sync variant — for plain `#[test]`s.
        fn set(config: Option<&std::path::Path>, timeout: Option<&str>) -> Self {
            let guard = ENV_LOCK.blocking_lock();
            let saved_config = std::env::var("CONGA_MCP_CONFIG").ok();
            let saved_timeout = std::env::var("CONGA_MCP_CALL_TIMEOUT_S").ok();
            Self::apply(config, timeout);
            Self {
                saved_config,
                saved_timeout,
                _guard: guard,
            }
        }

        /// Async variant — for `#[tokio::test]`s (`blocking_lock` panics
        /// inside a runtime).
        async fn set_async(config: Option<&std::path::Path>, timeout: Option<&str>) -> Self {
            let guard = ENV_LOCK.lock().await;
            let saved_config = std::env::var("CONGA_MCP_CONFIG").ok();
            let saved_timeout = std::env::var("CONGA_MCP_CALL_TIMEOUT_S").ok();
            Self::apply(config, timeout);
            Self {
                saved_config,
                saved_timeout,
                _guard: guard,
            }
        }
    }

    impl Drop for McpEnv {
        fn drop(&mut self) {
            match self.saved_config.take() {
                Some(v) => std::env::set_var("CONGA_MCP_CONFIG", v),
                None => std::env::remove_var("CONGA_MCP_CONFIG"),
            }
            match self.saved_timeout.take() {
                Some(v) => std::env::set_var("CONGA_MCP_CALL_TIMEOUT_S", v),
                None => std::env::remove_var("CONGA_MCP_CALL_TIMEOUT_S"),
            }
        }
    }

    /// Write an `mcp.json` with the given body and return its path.
    fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("mcp.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[tokio::test]
    async fn load_all_mcp_returns_empty_when_config_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        let _env = McpEnv::set_async(Some(&missing), Some("5")).await;
        let tools = load_all_mcp().await;
        assert!(tools.is_empty(), "a missing config file yields no tools");
    }

    #[tokio::test]
    async fn load_all_mcp_returns_empty_on_invalid_config_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "{ this is not json");
        let _env = McpEnv::set_async(Some(&path), Some("5")).await;
        let tools = load_all_mcp().await;
        assert!(tools.is_empty(), "a broken config file yields no tools");
    }

    #[tokio::test]
    async fn load_all_mcp_skips_server_that_cannot_be_spawned() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(
            tmp.path(),
            r#"{"mcpServers":{"broken":{"command":"/nonexistent/mcp-server","args":[]}}}"#,
        );
        let _env = McpEnv::set_async(Some(&path), Some("5")).await;
        let tools = load_all_mcp().await;
        assert!(
            tools.is_empty(),
            "a server that fails to start must be skipped, not fatal"
        );
    }

    #[tokio::test]
    async fn load_all_mcp_collects_tools_from_a_stdio_server() {
        let script = fixture_echo_server_script();
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(
            tmp.path(),
            &format!(r#"{{"mcpServers":{{"echo":{{"command":"python3","args":["{script}"]}}}}}}"#),
        );
        let _env = McpEnv::set_async(Some(&path), Some("20")).await;
        let tools = load_all_mcp().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__echo__echo");
    }

    /// Two sessions in the same process must SHARE one server process.
    /// The gateway builds a session per WebSocket connection, so without
    /// the cache every browser tab would spawn its own MCP server.
    #[tokio::test]
    async fn load_all_mcp_reuses_the_live_server_process() {
        let tmp = tempfile::tempdir().unwrap();
        let spawn_log = tmp.path().join("spawns.log");
        let script = fixture_pid_logging_server_script(&spawn_log);
        let path = write_config(
            tmp.path(),
            &format!(
                r#"{{"mcpServers":{{"reuse-probe":{{"command":"python3","args":["{script}"]}}}}}}"#
            ),
        );
        let _env = McpEnv::set_async(Some(&path), Some("20")).await;

        let first = load_all_mcp().await;
        assert_eq!(first.len(), 1);
        let spawns = || {
            std::fs::read_to_string(&spawn_log)
                .unwrap_or_default()
                .lines()
                .count()
        };
        assert_eq!(spawns(), 1, "the first load spawns one server");

        // The first load's tools still reference the server, so the second
        // load must reuse that process instead of spawning another one.
        let second = load_all_mcp().await;
        assert_eq!(second.len(), 1);
        assert_eq!(
            spawns(),
            1,
            "the second load must reuse the live server process"
        );
        assert_eq!(first[0].name, second[0].name);

        drop(first);
        drop(second);
    }

    /// A changed command line invalidates the cached server.
    #[tokio::test]
    async fn load_all_mcp_respawns_when_the_command_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let spawn_log = tmp.path().join("spawns.log");
        let script = fixture_pid_logging_server_script(&spawn_log);
        let path = write_config(
            tmp.path(),
            &format!(
                r#"{{"mcpServers":{{"reuse-probe":{{"command":"python3","args":["{script}"]}}}}}}"#
            ),
        );
        // The env guard holds a process-wide lock, so it must be released
        // before the second one is taken. The tools outlive the guard.
        let first = {
            let _env = McpEnv::set_async(Some(&path), Some("20")).await;
            load_all_mcp().await
        };
        assert_eq!(first.len(), 1);

        // Same server name, different command line -> the cached server is
        // stale and must not be reused.
        let other = fixture_pid_logging_server_script(&spawn_log);
        let path2 = write_config(
            tmp.path(),
            &format!(
                r#"{{"mcpServers":{{"reuse-probe":{{"command":"python3","args":["{other}"]}}}}}}"#
            ),
        );
        {
            let _env = McpEnv::set_async(Some(&path2), Some("20")).await;
            let second = load_all_mcp().await;
            assert_eq!(second.len(), 1);
        }

        let spawns = std::fs::read_to_string(&spawn_log)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(spawns, 2, "a changed command line must respawn the server");

        drop(first);
    }

    #[test]
    fn mcp_call_timeout_defaults_to_60_seconds() {
        let _env = McpEnv::set(None, None);
        assert_eq!(mcp_call_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn mcp_call_timeout_reads_the_env_override() {
        let _env = McpEnv::set(None, Some("5"));
        assert_eq!(mcp_call_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn mcp_call_timeout_invalid_env_value_falls_back_to_the_default() {
        let _env = McpEnv::set(None, Some("not-a-number"));
        assert_eq!(mcp_call_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn mcp_call_timeout_empty_env_value_falls_back_to_the_default() {
        let _env = McpEnv::set(None, Some(""));
        assert_eq!(mcp_call_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn mcp_call_timeout_zero_env_value_is_a_zero_duration() {
        let _env = McpEnv::set(None, Some("0"));
        assert_eq!(mcp_call_timeout(), Duration::from_secs(0));
    }
}
