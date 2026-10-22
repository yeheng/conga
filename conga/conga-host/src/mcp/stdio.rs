//! stdio transport: spawn an MCP server as a subprocess, JSON-RPC over
//! newline-delimited stdin/stdout.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use conga::ToolDefinition;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::types::{mcp_tool_definition, CallResult, McpTool, ToolsListResult};
use super::{McpError, PROTOCOL_VERSION};

/// A request we send (has `id` + `method` + optional `params`).
async fn write_request(
    stdin: &mut ChildStdin,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Result<(), McpError> {
    let msg = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params
    });
    let line = serde_json::to_string(&msg)?;
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

/// A notification we send (no `id`).
async fn write_notification(stdin: &mut ChildStdin, method: &str) -> Result<(), McpError> {
    let msg = serde_json::json!({"jsonrpc": "2.0", "method": method});
    let line = serde_json::to_string(&msg)?;
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

/// One line read from the server's stdout — a response, error, or notification.
#[derive(Debug)]
enum IncomingMessage {
    /// A successful response for our request `id`.
    Response { result: serde_json::Value },
    /// An error response for our request `id`.
    Error { code: i64, message: String },
    /// Anything else: notification (no id), or a response/error whose id
    /// doesn't match what we're waiting for. Skipped by the caller.
    Other,
}

fn parse_incoming(line: &str, expected_id: u64) -> IncomingMessage {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return IncomingMessage::Other,
    };
    // Notification: no "id" field.
    let Some(id_val) = v.get("id") else {
        return IncomingMessage::Other;
    };
    let id = id_val.as_u64().unwrap_or(u64::MAX);
    if id != expected_id {
        return IncomingMessage::Other;
    }
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string();
        IncomingMessage::Error { code, message }
    } else if v.get("result").is_some() {
        IncomingMessage::Response {
            result: v["result"].clone(),
        }
    } else {
        IncomingMessage::Other
    }
}

struct McpBridgeInner {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

/// One long-lived MCP server process. Each tool's `execute` closure holds an
/// `Arc<McpBridge>`; dropping all tools drops the bridge → `kill_on_drop`
/// reaps the subprocess.
pub struct McpBridge {
    inner: Mutex<McpBridgeInner>,
    timeout: Duration,
    server_name: String,
}

impl McpBridge {
    /// Spawn a server, run the handshake, discover tools.
    pub async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), McpError> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("no stdout".into()))?;

        let inner = McpBridgeInner {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };

        let bridge = Arc::new(Self {
            inner: Mutex::new(inner), // temporarily moves; we re-extract below
            timeout,
            server_name: name.to_string(),
        });

        // Handshake — IDs 1 (initialize) and 2 (tools/list).
        {
            let mut guard = bridge.inner.lock().await;
            // 1. initialize
            write_request(
                &mut guard.stdin,
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientInfo": {
                        "name": "conga",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {},
                }),
            )
            .await?;
            let _init_result = bridge.recv_response(&mut guard, 1).await?;

            // 2. initialized notification
            write_notification(&mut guard.stdin, "notifications/initialized").await?;

            // 3. tools/list
            write_request(&mut guard.stdin, 2, "tools/list", serde_json::json!({})).await?;
        }

        let tools_list: ToolsListResult = bridge
            .call_typed(2, |result| {
                serde_json::from_value(result)
                    .map_err(|e| McpError::Protocol(format!("tools/list parse error: {e}")))
            })
            .await?;

        let tools = tools_list
            .list
            .into_iter()
            .map(|t| bridge.tool_definition(t))
            .collect();
        Ok((bridge, tools))
    }

    /// Send a request and wait for its response, deserializing the result.
    async fn call_typed<T, F>(&self, id: u64, map: F) -> Result<T, McpError>
    where
        F: FnOnce(serde_json::Value) -> Result<T, McpError>,
    {
        let mut guard = self.inner.lock().await;
        let result = self.recv_response(&mut guard, id).await?;
        map(result)
    }

    /// Read lines until we find the response for `id`, skipping notifications
    /// and unmatched messages. Borrows `inner` via `guard` so we hold the lock
    /// for the entire read window.
    async fn recv_response(
        &self,
        guard: &mut McpBridgeInner,
        id: u64,
    ) -> Result<serde_json::Value, McpError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let mut line = String::new();
            let read_fut = guard.stdout.read_line(&mut line);
            let n = match tokio::time::timeout_at(deadline, read_fut).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(McpError::Io(e)),
                Err(_) => return Err(McpError::Timeout(self.timeout)),
            };
            if n == 0 {
                return Err(McpError::Protocol("server closed stdout".into()));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match parse_incoming(line, id) {
                IncomingMessage::Response { result, .. } => return Ok(result),
                IncomingMessage::Error { code, message, .. } => {
                    return Err(McpError::ServerError { code, message });
                }
                IncomingMessage::Other => continue,
            }
        }
    }

    /// Invoke an MCP tool by its original (un-prefixed) name.
    async fn call(
        &self,
        original_name: &str,
        args: &serde_json::Value,
    ) -> Result<CallResult, McpError> {
        let mut guard = self.inner.lock().await;
        let id = guard.next_id;
        guard.next_id += 1;
        write_request(
            &mut guard.stdin,
            id,
            "tools/call",
            serde_json::json!({"name": original_name, "arguments": args}),
        )
        .await?;
        let result = self.recv_response(&mut guard, id).await?;
        serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("tools/call parse error: {e}")))
    }

    /// Wrap one MCP tool as a conga [`ToolDefinition`] — via the shared
    /// [`mcp_tool_definition`] wrapper (same naming/risk/dispatch as the
    /// HTTP transport).
    fn tool_definition(self: &Arc<Self>, t: McpTool) -> ToolDefinition {
        let bridge = Arc::clone(self);
        mcp_tool_definition(
            &self.server_name,
            t,
            Arc::new(move |name, args| {
                let bridge = Arc::clone(&bridge);
                Box::pin(async move { bridge.call(&name, &args).await })
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::{ContentBlock, RiskLevel};

    #[test]
    fn parse_incoming_matches_response_by_id() {
        let line = r#"{"jsonrpc":"2.0","id":5,"result":{"ok":true}}"#;
        match parse_incoming(line, 5) {
            IncomingMessage::Response { result, .. } => {
                assert_eq!(result["ok"], true);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_incoming_skips_unmatched_id() {
        let line = r#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
        assert!(matches!(parse_incoming(line, 5), IncomingMessage::Other));
    }

    #[test]
    fn parse_incoming_treats_notification_as_other() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#;
        assert!(matches!(parse_incoming(line, 5), IncomingMessage::Other));
    }

    #[test]
    fn parse_incoming_parses_error() {
        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message"|"not found"}}"#;
        // malformed JSON (| instead of :) → Other
        assert!(matches!(parse_incoming(line, 7), IncomingMessage::Other));

        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"not found"}}"#;
        match parse_incoming(line, 7) {
            IncomingMessage::Error { code, message, .. } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "not found");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn parse_incoming_defaults_missing_error_fields() {
        // An error object without code/message → the documented defaults.
        let line = r#"{"jsonrpc":"2.0","id":7,"error":{}}"#;
        match parse_incoming(line, 7) {
            IncomingMessage::Error { code, message, .. } => {
                assert_eq!(code, -1);
                assert_eq!(message, "unknown error");
            }
            _ => panic!("expected Error"),
        }

        // Neither result nor error → Other (nothing usable).
        let line = r#"{"jsonrpc":"2.0","id":7}"#;
        assert!(matches!(parse_incoming(line, 7), IncomingMessage::Other));
    }

    // ── Integration test (Python mock server) ────────────────────

    /// Write a python3 MCP server that answers `tools/call`:
    /// - `echo`  → the arguments echoed back as text
    /// - `boom`  → a JSON-RPC error
    /// - `weird` → a result that is not a valid `CallResult`
    /// Returns the script path; the temp dir is kept so it outlives the
    /// call.
    fn fixture_call_server_script() -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_call.py");
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
    if method != "tools/call":
        continue
    name = msg.get("params", {}).get("name", "")
    if name == "boom":
        send({"jsonrpc": "2.0", "id": mid, "error": {
            "code": -32602, "message": "invalid params"}})
    elif name == "weird":
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": "oops"}})
    else:
        args = msg.get("params", {}).get("arguments", {})
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": args.get("text", "")}],
            "isError": False
        }})
"#,
        )
        .unwrap();
        let kept = dir.keep();
        kept.join("mcp_call.py").to_string_lossy().into_owned()
    }

    /// A bridge whose "server" is `cat`: every line written to its stdin is
    /// echoed back on its stdout.
    async fn echo_bridge(timeout: Duration) -> Arc<McpBridge> {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Arc::new(McpBridge {
            inner: Mutex::new(McpBridgeInner {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
            }),
            timeout,
            server_name: "cat".to_string(),
        })
    }

    /// A bridge whose server is the python fixture above.
    async fn call_bridge(script: &str, timeout: Duration) -> Arc<McpBridge> {
        let mut child = Command::new("python3")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Arc::new(McpBridge {
            inner: Mutex::new(McpBridgeInner {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
            }),
            timeout,
            server_name: "fixture".to_string(),
        })
    }

    // ── `write_request` / `write_notification` ────────────────────

    #[tokio::test]
    async fn write_request_writes_a_jsonrpc_frame_with_id_and_params() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        write_request(
            &mut stdin,
            7,
            "tools/list",
            serde_json::json!({"cursor": "x"}),
        )
        .await
        .expect("write request");

        let mut line = String::new();
        stdout.read_line(&mut line).await.unwrap();
        assert!(line.ends_with('\n'), "a frame must be newline terminated");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "tools/list");
        assert_eq!(v["params"], serde_json::json!({"cursor": "x"}));

        drop(stdin);
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn write_request_fails_when_the_server_process_is_gone() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // The child is gone: its end of the pipe is closed, so the write
        // fails instead of silently succeeding.
        let _ = child.wait().await;

        let err = write_request(&mut stdin, 1, "initialize", serde_json::json!({}))
            .await
            .expect_err("writing to a dead process must fail");
        assert!(
            matches!(err, McpError::Io(_)),
            "expected an io error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn write_notification_writes_a_frame_without_an_id() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        write_notification(&mut stdin, "notifications/initialized")
            .await
            .expect("write notification");

        let mut line = String::new();
        stdout.read_line(&mut line).await.unwrap();
        assert!(line.ends_with('\n'), "a frame must be newline terminated");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/initialized");
        assert!(v.get("id").is_none(), "a notification carries no id: {v}");

        drop(stdin);
        let _ = child.wait().await;
    }

    // ── `recv_response` ───────────────────────────────────────────

    #[tokio::test]
    async fn recv_response_skips_noise_until_the_matching_response() {
        let bridge = echo_bridge(Duration::from_secs(5)).await;
        let mut guard = bridge.inner.lock().await;
        {
            let stdin = &mut guard.stdin;
            stdin.write_all(b"not json at all\n").await.unwrap();
            stdin.write_all(b"\n").await.unwrap();
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n")
                .await
                .unwrap();
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"wrong\":true}}\n")
                .await
                .unwrap();
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"ok\":true}}\n")
                .await
                .unwrap();
            stdin.flush().await.unwrap();
        }

        let result = bridge
            .recv_response(&mut guard, 5)
            .await
            .expect("matching response");
        assert_eq!(result["ok"], true);
    }

    #[tokio::test]
    async fn recv_response_reports_a_jsonrpc_error_frame() {
        let bridge = echo_bridge(Duration::from_secs(5)).await;
        let mut guard = bridge.inner.lock().await;
        {
            let stdin = &mut guard.stdin;
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"error\":{\"code\":-32601,\"message\":\"not found\"}}\n")
                .await
                .unwrap();
            stdin.flush().await.unwrap();
        }

        let err = bridge
            .recv_response(&mut guard, 5)
            .await
            .expect_err("error frame");
        match err {
            McpError::ServerError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "not found");
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recv_response_fails_when_the_server_closes_stdout() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let bridge = Arc::new(McpBridge {
            inner: Mutex::new(McpBridgeInner {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
            }),
            timeout: Duration::from_secs(5),
            server_name: "dead".to_string(),
        });
        let mut guard = bridge.inner.lock().await;

        let err = bridge
            .recv_response(&mut guard, 1)
            .await
            .expect_err("a closed stdout is an error");
        match err {
            McpError::Protocol(msg) => assert!(msg.contains("server closed stdout"), "{msg}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recv_response_times_out_when_the_server_stays_silent() {
        let bridge = echo_bridge(Duration::from_millis(50)).await;
        let mut guard = bridge.inner.lock().await;

        let err = bridge
            .recv_response(&mut guard, 1)
            .await
            .expect_err("a silent server must time out");
        match err {
            McpError::Timeout(d) => assert_eq!(d, Duration::from_millis(50)),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    // ── `call_typed` ──────────────────────────────────────────────

    #[tokio::test]
    async fn call_typed_maps_the_matching_response() {
        let bridge = echo_bridge(Duration::from_secs(5)).await;
        {
            let mut guard = bridge.inner.lock().await;
            guard
                .stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"n\":3}}\n")
                .await
                .unwrap();
            guard.stdin.flush().await.unwrap();
        }

        let n: u32 = bridge
            .call_typed(5, |v| Ok(v["n"].as_u64().unwrap() as u32))
            .await
            .expect("call_typed");
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn call_typed_propagates_the_mapper_error() {
        let bridge = echo_bridge(Duration::from_secs(5)).await;
        {
            let mut guard = bridge.inner.lock().await;
            guard
                .stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n")
                .await
                .unwrap();
            guard.stdin.flush().await.unwrap();
        }

        let err = bridge
            .call_typed::<serde_json::Value, _>(5, |_v| Err(McpError::Protocol("bad shape".into())))
            .await
            .expect_err("the mapper error must surface");
        match err {
            McpError::Protocol(msg) => assert_eq!(msg, "bad shape"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_typed_times_out_on_an_unmatched_id() {
        let bridge = echo_bridge(Duration::from_millis(50)).await;
        {
            let mut guard = bridge.inner.lock().await;
            guard
                .stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n")
                .await
                .unwrap();
            guard.stdin.flush().await.unwrap();
        }

        let err = bridge
            .call_typed(5, |v| Ok(v))
            .await
            .expect_err("an unmatched id must not be accepted");
        match err {
            McpError::Timeout(d) => assert_eq!(d, Duration::from_millis(50)),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    // ── `call` ────────────────────────────────────────────────────

    #[tokio::test]
    async fn call_sends_tools_call_and_parses_the_call_result() {
        let script = fixture_call_server_script();
        let bridge = call_bridge(&script, Duration::from_secs(10)).await;

        let result = bridge
            .call("echo", &serde_json::json!({"text": "hello"}))
            .await
            .expect("tools/call");

        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        let blocks = super::super::types::content_to_blocks(&result.content);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[tokio::test]
    async fn call_reports_a_jsonrpc_error_from_the_server() {
        let script = fixture_call_server_script();
        let bridge = call_bridge(&script, Duration::from_secs(10)).await;

        let err = bridge
            .call("boom", &serde_json::json!({}))
            .await
            .expect_err("the server error must surface");
        match err {
            McpError::ServerError { code, message } => {
                assert_eq!(code, -32602);
                assert_eq!(message, "invalid params");
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_rejects_a_result_that_is_not_a_call_result() {
        let script = fixture_call_server_script();
        let bridge = call_bridge(&script, Duration::from_secs(10)).await;

        let err = bridge
            .call("weird", &serde_json::json!({}))
            .await
            .expect_err("a malformed result must surface");
        match err {
            McpError::Protocol(msg) => assert!(msg.contains("tools/call parse error"), "{msg}"),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_times_out_when_the_server_never_answers() {
        // `cat` echoes the request back; the echo is not a response, so the
        // call must run into its deadline.
        let bridge = echo_bridge(Duration::from_millis(50)).await;

        let err = bridge
            .call("echo", &serde_json::json!({}))
            .await
            .expect_err("a silent server must time out");
        match err {
            McpError::Timeout(d) => assert_eq!(d, Duration::from_millis(50)),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    // ── `tool_definition` ─────────────────────────────────────────

    #[tokio::test]
    async fn tool_definition_wraps_the_tool_and_dispatches_a_call() {
        let script = fixture_call_server_script();
        let bridge = call_bridge(&script, Duration::from_secs(10)).await;

        let def = bridge.tool_definition(McpTool {
            name: "echo".into(),
            title: None,
            description: "echo back the arguments".into(),
            annotations: serde_json::json!({}),
            input_schema: serde_json::json!({"type": "object"}),
        });

        assert_eq!(def.name, "mcp__fixture__echo");
        assert_eq!(def.label, "fixture/echo");
        assert_eq!(def.description, "echo back the arguments");
        assert_eq!(def.parameters, serde_json::json!({"type": "object"}));
        assert_eq!(def.risk, RiskLevel::High);

        let result = (def.execute)(super::super::test_util::tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hi"}),
        ))
        .await
        .expect("execute");
        assert!(!result.is_error);
        assert!(matches!(
            &result.content[0],
            ContentBlock::Text { text } if text == "hi"
        ));
    }

    fn fixture_mcp_server_script() -> String {
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

    #[tokio::test]
    async fn mcp_handshake_list_and_call() {
        let script = fixture_mcp_server_script();
        let env = HashMap::new();
        let (bridge, tools) =
            McpBridge::spawn("test", "python3", &[script], &env, Duration::from_secs(10))
                .await
                .expect("spawn");

        // tools/list → 1 tool, prefixed
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__test__echo");
        assert_eq!(tools[0].label, "test/echo");
        assert_eq!(tools[0].risk, RiskLevel::High);

        // tools/call → echo back
        let result = (tools[0].execute)(super::super::test_util::tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hello mcp"}),
        ))
        .await
        .unwrap();
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello mcp"),
            _ => panic!("expected text content"),
        }
        drop(bridge);
    }

    // ── Smoke test: real GitHub MCP server (needs token + network) ──────

    /// End-to-end against the real `@modelcontextprotocol/server-github`.
    /// Ignored by default — run with:
    ///   GITHUB_PERSONAL_ACCESS_TOKEN=ghp_xxx \
    ///     cargo test -p conga-host -- --ignored mcp_smoke_github
    #[tokio::test]
    #[ignore]
    async fn mcp_smoke_github() {
        let token = std::env::var("GITHUB_PERSONAL_ACCESS_TOKEN")
            .expect("set GITHUB_PERSONAL_ACCESS_TOKEN to run this smoke test");
        let mut env = HashMap::new();
        env.insert("GITHUB_PERSONAL_ACCESS_TOKEN".into(), token);

        let (bridge, tools) = McpBridge::spawn(
            "github",
            "npx",
            &["-y".into(), "@modelcontextprotocol/server-github".into()],
            &env,
            Duration::from_secs(30),
        )
        .await
        .expect("spawn github mcp");

        // GitHub MCP exposes dozens of tools; we just need > 0.
        assert!(!tools.is_empty(), "expected tools from github server");
        eprintln!("(github mcp: {} tools discovered)", tools.len());

        // Verify naming convention on the first tool.
        assert!(
            tools[0].name.starts_with("mcp__github__"),
            "tool name not prefixed: {}",
            tools[0].name
        );

        // Call search_repositories — read-only, no specific repo needed,
        // verifies the token works and the full tools/call path.
        let search = tools
            .iter()
            .find(|t| t.name == "mcp__github__search_repositories")
            .expect("search_repositories tool not found");

        let result = (search.execute)(super::super::test_util::tool_call_ctx_for_test(
            "smoke",
            serde_json::json!({"query": "conga"}),
        ))
        .await
        .expect("search_repositories call");
        assert!(
            !result.is_error,
            "search_repositories returned error: {:?}",
            result.content
        );
        eprintln!(
            "(search_repositories ok, {} content blocks)",
            result.content.len()
        );
        drop(bridge);
    }
}
