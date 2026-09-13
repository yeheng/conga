//! Streamable HTTP transport: JSON-RPC over POST, with optional SSE
//! streaming responses and server-assigned session ids.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use conga::ToolDefinition;
use tokio::sync::Mutex;

use super::types::{
    mcp_tool_definition, parse_jsonrpc_value, CallResult, McpTool, Parsed, ToolsListResult,
};
use super::{McpError, PROTOCOL_VERSION};

/// Proxy for remote MCP traffic: the tool-proxy system first (runtime
/// override > `CONGA_TOOL_PROXY`), then the legacy LLM-proxy env chain for
/// backward compatibility. Direct connection when none is set.
fn pick_mcp_proxy(lookup: &dyn Fn(&str) -> Result<String, std::env::VarError>) -> Option<String> {
    if let Some(p) = crate::proxy::tool_proxy() {
        return Some(p);
    }
    ["CONGA_LLM_PROXY", "HTTPS_PROXY", "https_proxy"]
        .iter()
        .find_map(|k| lookup(k).ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Session header for the Streamable HTTP transport: stateful servers
/// assign one at `initialize` and expect it echoed on every later request;
/// stateless servers never send it.
const SESSION_ID_HEADER: &str = "mcp-session-id";

/// Incremental SSE decoder for the Streamable HTTP transport.
///
/// Feed raw body chunks as they arrive; complete events (terminated by a
/// blank line) come back as payloads with their `data:` lines joined by
/// `\n` per the SSE spec. Handles LF / CRLF / CR line endings — including a
/// CRLF split across two chunks — `:` comments, and the single-space strip
/// after the field colon.
struct SseDecoder {
    /// Bytes of a line not yet terminated (may split mid-UTF-8).
    buf: Vec<u8>,
    /// `data:` lines of the event in progress.
    data: Vec<String>,
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Feed one body chunk; returns every event completed by a blank line.
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n' || b == b'\r') {
            // A CR at the very end of the buffer may be half of a CRLF whose
            // LF lands in the next chunk — wait for it.
            if self.buf[i] == b'\r' && i + 1 == self.buf.len() {
                break;
            }
            let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
            let mut consumed = i + 1;
            if self.buf[i] == b'\r' && self.buf.get(i + 1) == Some(&b'\n') {
                consumed += 1;
            }
            self.buf.drain(..consumed);
            if self.process_line(&line) {
                // Blank line → dispatch: no data lines, no event.
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                }
                self.data.clear();
            }
        }
        events
    }

    /// Stream ended: treat the trailing unterminated line (if any) as
    /// complete, then dispatch the pending event one final time.
    fn flush(&mut self) -> Vec<String> {
        let mut line = &self.buf[..];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = String::from_utf8_lossy(line).into_owned();
        self.buf.clear();
        self.process_line(&line);
        if self.data.is_empty() {
            Vec::new()
        } else {
            vec![std::mem::take(&mut self.data).join("\n")]
        }
    }

    /// Process one complete line into the pending event; `true` on a blank
    /// line (the SSE event-dispatch point).
    fn process_line(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        if line.starts_with(':') {
            return false; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            self.data.push(value.to_string());
        }
        false
    }
}

/// Try one dispatched SSE event payload as the JSON-RPC response for `id`:
/// `None` = not ours (notification or different id), `Some(Ok)` = result,
/// `Some(Err)` = a JSON-RPC error addressed to us.
fn sse_payload(payload: &str, id: u64) -> Option<Result<serde_json::Value, McpError>> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match parse_jsonrpc_value(&v, id) {
        Parsed::Result(result) => Some(Ok(result)),
        Parsed::Error { code, message } => Some(Err(McpError::ServerError { code, message })),
        Parsed::Other => None,
    }
}

/// One long-lived MCP HTTP client. Each tool's `execute` closure holds an
/// `Arc<McpHttpClient>`; calls are POSTs to the server URL, echoing the
/// server-assigned session id (if any) on every request after `initialize`.
pub struct McpHttpClient {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    timeout: Duration,
    server_name: String,
    next_id: tokio::sync::Mutex<u64>,
    /// Session id assigned by the server at `initialize`, if any.
    session_id: Mutex<Option<String>>,
}

impl McpHttpClient {
    /// Connect to a Streamable HTTP MCP server, run the handshake, discover tools.
    pub async fn connect(
        name: &str,
        url: &str,
        headers: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<(Arc<Self>, Vec<ToolDefinition>), McpError> {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(false);
        // Proxy support: tool-proxy system (override > CONGA_TOOL_PROXY)
        // first, then the legacy CONGA_LLM_PROXY / HTTPS_PROXY env chain.
        if let Some(proxy_url) = pick_mcp_proxy(&|k| std::env::var(k)) {
            if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
                builder = builder.proxy(proxy);
            }
        }
        let client = builder
            .build()
            .map_err(|e| McpError::Protocol(format!("http client build error: {e}")))?;

        let http = Arc::new(Self {
            client,
            url: url.to_string(),
            headers: headers.clone(),
            timeout,
            server_name: name.to_string(),
            next_id: tokio::sync::Mutex::new(1),
            session_id: Mutex::new(None),
        });

        // Handshake — same sequence as stdio, just over HTTP POST.
        // 1. initialize
        let _init = http
            .request(
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

        // 2. notifications/initialized (no response expected, but we POST it)
        http.notification("notifications/initialized").await?;

        // 3. tools/list
        let tools_result = http.request(2, "tools/list", serde_json::json!({})).await?;
        let tools_list: ToolsListResult = serde_json::from_value(tools_result)
            .map_err(|e| McpError::Protocol(format!("tools/list parse error: {e}")))?;

        let tools = tools_list
            .list
            .into_iter()
            .map(|t| http.tool_definition(t))
            .collect();
        Ok((http, tools))
    }

    /// POST a JSON-RPC request and parse the response (single JSON or SSE stream).
    async fn request(
        &self,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let deadline = tokio::time::Instant::now() + self.timeout;
        let send_fut = self.post_json(&body);
        let resp = tokio::time::timeout_at(deadline, send_fut)
            .await
            .map_err(|_| McpError::Timeout(self.timeout))??;

        // Stateful servers assign a session id (at initialize) and expect it
        // on every later request; stateless servers never send one.
        self.capture_session_id(resp.headers()).await;

        // Parse based on Content-Type.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            // SSE: consume the stream incrementally and return on the event
            // matching our id — servers may keep the stream open.
            self.read_sse(resp, id, deadline).await
        } else {
            // Single JSON response.
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| McpError::Protocol(format!("http response json parse error: {e}")))?;
            match parse_jsonrpc_value(&v, id) {
                Parsed::Result(result) => Ok(result),
                Parsed::Error { code, message } => Err(McpError::ServerError { code, message }),
                Parsed::Other => Err(McpError::Protocol(format!(
                    "http response has no result/error for id {id}: {v}"
                ))),
            }
        }
    }

    /// POST a notification (no id, no response expected).
    async fn notification(&self, method: &str) -> Result<(), McpError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        self.post_json(&body).await?;
        Ok(())
    }

    /// Send a POST and return the raw response.
    async fn post_json(&self, body: &serde_json::Value) -> Result<reqwest::Response, McpError> {
        let mut req = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            );
        // Echo the server-assigned session id once captured at initialize.
        if let Some(session_id) = self.session_id.lock().await.clone() {
            req = req.header(SESSION_ID_HEADER, session_id);
        }
        for (k, v) in &self.headers {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) {
                if let Ok(val) = reqwest::header::HeaderValue::from_str(v) {
                    req = req.header(name, val);
                }
            }
        }
        req.json(body)
            .send()
            .await
            .map_err(|e| McpError::Protocol(format!("http post error: {e}")))
    }

    /// Record the server-assigned session id from a response header, if the
    /// server sent one (empty values are ignored).
    async fn capture_session_id(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(v) = headers
            .get(SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *self.session_id.lock().await = Some(v.to_string());
        }
    }

    /// Consume an SSE response body incrementally: return as soon as the
    /// JSON-RPC message matching `id` arrives, dropping the rest of the
    /// stream — Streamable-HTTP servers may keep it open indefinitely, so
    /// waiting for EOF (or buffering the whole body) can hang.
    async fn read_sse(
        &self,
        resp: reqwest::Response,
        id: u64,
        deadline: tokio::time::Instant,
    ) -> Result<serde_json::Value, McpError> {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        loop {
            let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    return Err(McpError::Protocol(format!("http body read error: {e}")))
                }
                Ok(None) => {
                    // Stream ended — dispatch any trailing unterminated event.
                    for payload in decoder.flush() {
                        if let Some(res) = sse_payload(&payload, id) {
                            return res;
                        }
                    }
                    break;
                }
                Err(_) => return Err(McpError::Timeout(self.timeout)),
            };
            for payload in decoder.feed(&chunk) {
                if let Some(res) = sse_payload(&payload, id) {
                    return res; // matched — stop reading, drop the stream
                }
            }
        }
        Err(McpError::Protocol(format!(
            "SSE stream ended without a response for id {id}"
        )))
    }

    /// Invoke an MCP tool by its original (un-prefixed) name.
    async fn call(
        &self,
        original_name: &str,
        args: &serde_json::Value,
    ) -> Result<CallResult, McpError> {
        let id = {
            let mut guard = self.next_id.lock().await;
            let id = *guard;
            *guard += 1;
            id
        };
        let result = self
            .request(
                id,
                "tools/call",
                serde_json::json!({"name": original_name, "arguments": args}),
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|e| McpError::Protocol(format!("tools/call parse error: {e}")))
    }

    /// Wrap one MCP tool as a conga [`ToolDefinition`] — via the shared
    /// [`mcp_tool_definition`] wrapper (same naming/risk/dispatch as the
    /// stdio transport).
    fn tool_definition(self: &Arc<Self>, t: McpTool) -> ToolDefinition {
        let client = Arc::clone(self);
        mcp_tool_definition(
            &self.server_name,
            t,
            Arc::new(move |name, args| {
                let client = Arc::clone(&client);
                Box::pin(async move { client.call(&name, &args).await })
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::ContentBlock;

    #[test]
    fn sse_response_extracts_matching_data_line() {
        // Simulate an SSE body: a notification event, then the matching response.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"isError\":false}}\n\n";
        let mut decoder = SseDecoder::new();
        let v = decoder
            .feed(sse.as_bytes())
            .into_iter()
            .find_map(|payload| sse_payload(&payload, 7))
            .expect("should find matching response")
            .expect("matching response should be a result");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["isError"], false);
    }

    #[test]
    fn sse_response_missing_match_returns_error() {
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n";
        let mut decoder = SseDecoder::new();
        assert!(
            decoder
                .feed(sse.as_bytes())
                .into_iter()
                .all(|payload| sse_payload(&payload, 7).is_none()),
            "should not find a match for id=7"
        );
    }

    #[test]
    fn sse_feed_returns_match_before_stream_drains() {
        // The decoder hands back events as chunks arrive, so the caller can
        // return on the matching id even though more data follows. Feed one
        // byte at a time — the worst-case chunk split.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
        );
        let mut decoder = SseDecoder::new();
        let mut result = None;
        'outer: for byte in body.bytes() {
            for payload in decoder.feed(&[byte]) {
                if let Some(res) = sse_payload(&payload, 7) {
                    result = Some(res);
                    break 'outer;
                }
            }
        }
        let v = result
            .expect("match must land mid-stream")
            .expect("match should be a result");
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn sse_multiline_data_fields_join_with_newline() {
        // Per the SSE spec, consecutive `data:` lines of one event join
        // with a single `\n` — so a JSON-RPC response pretty-printed across
        // data lines must still parse.
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.feed(b"data: first\ndata: second\n\n"),
            vec!["first\nsecond".to_string()]
        );

        let stream = "data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":4,\"result\":{\"n\":1}}\n\n";
        let mut decoder = SseDecoder::new();
        let v = decoder
            .feed(stream.as_bytes())
            .into_iter()
            .find_map(|payload| sse_payload(&payload, 4))
            .expect("joined data must parse as the response")
            .expect("result");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn sse_decoder_handles_terminators_comments_and_split_chunks() {
        // Comment lines are ignored; one space after the colon is stripped.
        let mut d = SseDecoder::new();
        assert_eq!(
            d.feed(b": keep-alive\ndata: spaced\n\n"),
            vec!["spaced".to_string()]
        );

        // CRLF split across chunks: a trailing CR waits for the next byte.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: crlf\r").is_empty());
        assert_eq!(d.feed(b"\n\r\n"), vec!["crlf".to_string()]);

        // Bare CR line endings also terminate lines; a trailing lone CR
        // could still be half of a CRLF, so it defers to the next chunk or
        // flush.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: cr\rdata: d\r\r").is_empty());
        assert_eq!(d.flush(), vec!["cr\nd".to_string()]);

        // EOF flushes a trailing unterminated data line as an event.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: tail").is_empty());
        assert_eq!(d.flush(), vec!["tail".to_string()]);
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: done\n").is_empty());
        assert_eq!(d.flush(), vec!["done".to_string()]);
        // ... including when it ends in a lone (deferred) CR.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: crtail\r").is_empty());
        assert_eq!(d.flush(), vec!["crtail".to_string()]);
    }

    // ── Streamable HTTP transport: local raw-HTTP MCP server ─────

    /// One request as observed by the fake MCP HTTP server.
    #[derive(Debug)]
    struct SeenRequest {
        method: String,
        session_id: Option<String>,
    }

    /// Read one HTTP request (head + Content-Length body) from `sock`.
    /// Returns the head text and the JSON body, or `None` on EOF.
    async fn read_http_request(
        sock: &mut tokio::net::TcpStream,
        buf: &mut Vec<u8>,
    ) -> Option<(String, serde_json::Value)> {
        use tokio::io::AsyncReadExt;

        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let len: usize = head
            .to_ascii_lowercase()
            .lines()
            .find(|l| l.starts_with("content-length:"))
            .and_then(|l| l.split_once(':'))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);
        let total = head_end + 4 + len;
        while buf.len() < total {
            let mut chunk = [0u8; 4096];
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = serde_json::from_slice(&buf[head_end + 4..total]).ok()?;
        buf.drain(..total);
        Some((head, body))
    }

    /// Write one HTTP/1.1 chunked-encoding frame (`len\r\ndata\r\n`).
    async fn write_chunk(sock: &mut tokio::net::TcpStream, event: &[u8]) {
        use tokio::io::AsyncWriteExt;

        let mut frame = format!("{:x}\r\n", event.len()).into_bytes();
        frame.extend_from_slice(event);
        frame.extend_from_slice(b"\r\n");
        sock.write_all(&frame).await.unwrap();
    }

    /// Extract the Mcp-Session-Id request header, if present.
    fn session_id_of(head: &str) -> Option<String> {
        head.lines().find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("mcp-session-id")
                .then(|| value.trim().to_string())
        })
    }

    /// Fake Streamable-HTTP MCP server on loopback: `initialize` →
    /// `notifications/initialized` → `tools/list` over plain JSON, then one
    /// `tools/call` answered as a slow SSE stream (a notification event,
    /// then the matching response split over two `data:` lines, then more
    /// events — and the stream stays open until the client hangs up).
    /// `session` = the Mcp-Session-Id the initialize response carries
    /// (`None` = stateless server). Handles exactly 4 requests.
    async fn fake_mcp_http_server(
        session: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<SeenRequest>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..4 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let Some((head, body)) = read_http_request(&mut sock, &mut buf).await else {
                    continue;
                };
                let method = body
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                seen.push(SeenRequest {
                    session_id: session_id_of(&head),
                    method: method.clone(),
                });
                match method.as_str() {
                    "initialize" => {
                        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1.0"}}}"#;
                        let session_line = session
                            .map(|s| format!("{SESSION_ID_HEADER}: {s}\r\n"))
                            .unwrap_or_default();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session_line}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        sock.write_all(resp.as_bytes()).await.unwrap();
                    }
                    "notifications/initialized" => {
                        sock.write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    }
                    "tools/list" => {
                        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo back the arguments","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        sock.write_all(resp.as_bytes()).await.unwrap();
                    }
                    "tools/call" => {
                        // SSE stream over chunked framing, written in
                        // pieces: notification first, then the response —
                        // echoing the REQUEST's id, split across two `data:`
                        // lines (multi-line data per spec) — then more
                        // events, and the stream is left open. A client that
                        // buffers the whole body instead of returning on the
                        // matching event hits its request timeout here.
                        let id = body.get("id").cloned().unwrap_or(serde_json::json!(0));
                        sock.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                        )
                        .await
                        .unwrap();
                        write_chunk(
                            &mut sock,
                            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
                        )
                        .await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let response = format!(
                            "data: {{\"jsonrpc\":\"2.0\",\"id\":{id},\ndata: \"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"streamed\"}}],\"isError\":false}}}}\n\n"
                        );
                        write_chunk(&mut sock, response.as_bytes()).await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        write_chunk(
                            &mut sock,
                            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
                        )
                        .await;
                        // Hold the stream open until the client disconnects
                        // (it must have returned early) or 5s as a brake.
                        let mut sink = [0u8; 64];
                        let _ = tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                match sock.read(&mut sink).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        })
                        .await;
                    }
                    _ => {}
                }
            }
            seen
        });

        (format!("http://{addr}/mcp"), server)
    }

    /// `connect()` reads the real proxy env; these tests must dial loopback
    /// directly. Serialize with the proxy tests (same lock as fetch.rs) and
    /// scrub the proxy env vars; [`restore_proxy_env`] puts them back.
    async fn scrub_proxy_env() -> (
        tokio::sync::MutexGuard<'static, ()>,
        Vec<(String, Option<String>)>,
    ) {
        let guard = crate::proxy::test_util::LOCK.lock().await;
        let saved: Vec<(String, Option<String>)> = [
            "CONGA_TOOL_PROXY",
            "CONGA_LLM_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
        ]
        .iter()
        .map(|k| (k.to_string(), std::env::var(k).ok()))
        .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        (guard, saved)
    }

    fn restore_proxy_env(saved: &[(String, Option<String>)]) {
        for (k, v) in saved {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
    }

    /// End-to-end against a local raw-HTTP MCP server: the session id from
    /// the initialize response is echoed on every later request, and a
    /// `tools/call` answered by an SSE stream that stays open still returns
    /// on the matching event (no whole-body buffering, no draining).
    #[tokio::test]
    async fn http_session_id_captured_echoed_and_sse_returns_early() {
        let (_guard, saved) = scrub_proxy_env().await;
        let (url, server) = fake_mcp_http_server(Some("sess-abc-123")).await;
        // Short timeout: a client that buffers the SSE body instead of
        // returning on the matching event fails the call instead of hanging.
        let connected =
            McpHttpClient::connect("fake", &url, &HashMap::new(), Duration::from_secs(2)).await;
        restore_proxy_env(&saved);

        let (client, tools) = connected.expect("connect");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__fake__echo");

        let result = (tools[0].execute)(super::super::test_util::tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hello"}),
        ))
        .await
        .expect("tools/call over a still-open SSE stream");
        assert!(!result.is_error);
        assert!(matches!(
            &result.content[0],
            ContentBlock::Text { text } if text == "streamed"
        ));

        let seen = server.await.unwrap();
        assert_eq!(
            seen.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
            vec![
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        // initialize itself carries no session id (nothing captured yet);
        // every later request echoes the server-assigned one.
        assert_eq!(seen[0].session_id, None);
        for r in &seen[1..] {
            assert_eq!(r.session_id.as_deref(), Some("sess-abc-123"));
        }
        drop(client);
    }

    /// A stateless server (no Mcp-Session-Id on initialize) must never
    /// receive an invented one.
    #[tokio::test]
    async fn http_stateless_server_gets_no_session_header() {
        let (_guard, saved) = scrub_proxy_env().await;
        let (url, server) = fake_mcp_http_server(None).await;
        let connected =
            McpHttpClient::connect("fake", &url, &HashMap::new(), Duration::from_secs(2)).await;
        let (_client, tools) = connected.expect("connect");
        let result = (tools[0].execute)(super::super::test_util::tool_call_ctx_for_test(
            "c1",
            serde_json::json!({"text": "hello"}),
        ))
        .await
        .expect("tools/call");
        assert!(!result.is_error);
        restore_proxy_env(&saved);

        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 4);
        assert!(
            seen.iter().all(|r| r.session_id.is_none()),
            "stateless server must not receive Mcp-Session-Id: {seen:?}"
        );
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    /// Serializes these tests: they touch the process-global tool-proxy
    /// override (conga's own test lock is pub(crate), unavailable here).
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned().ok_or(std::env::VarError::NotPresent)
    }

    #[test]
    fn tool_proxy_wins_over_legacy_env() {
        let _g = LOCK.lock().unwrap();
        crate::proxy::set_tool_proxy(Some("socks5://tool:1080")).unwrap();
        assert_eq!(
            pick_mcp_proxy(&fake_env(&[
                ("CONGA_TOOL_PROXY", "socks5://tool:1080"),
                ("CONGA_LLM_PROXY", "http://llm:8080"),
            ])),
            Some("socks5://tool:1080".to_string())
        );
        crate::proxy::set_tool_proxy(None).unwrap();
    }

    #[test]
    fn legacy_llm_proxy_still_works() {
        let _g = LOCK.lock().unwrap();
        crate::proxy::set_tool_proxy(None).unwrap();
        assert_eq!(
            pick_mcp_proxy(&fake_env(&[("CONGA_LLM_PROXY", "http://llm:8080")])),
            Some("http://llm:8080".to_string())
        );
        // Real env may carry CONGA_TOOL_PROXY on dev machines; the None branch
        // is only deterministic when it's unset.
        if std::env::var("CONGA_TOOL_PROXY").is_err() {
            assert_eq!(super::pick_mcp_proxy(&fake_env(&[])), None);
        }
    }
}
