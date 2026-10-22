//! OpenAI-compatible provider (OpenAI / DeepSeek / 智谱 / xAI / Groq / Ollama /
//! vLLM / etc.).
//!
//! One implementation covers ~80% of
//! providers; Anthropic native lives in [`crate::providers::anthropic`].
//! Both share the SSE download transport
//! ([`crate::providers::sse::download_sse`]); only the body shape and chunk
//! parsing differ.

use std::pin::Pin;

use crate::cancel::CancelSignal;

use futures_util::Stream;
use serde_json::json;

use super::collect_text;
use crate::types::context::{ModelSpec, StreamChunk, StreamFn};
use crate::types::message::{AgentMessage, ContentBlock, StopReason};
use crate::types::tool::ToolDefinition;

/// OpenAI-compatible chat completions provider.
#[derive(Clone)]
pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Construct with a pre-built client (e.g. one carrying proxy config from
    /// [`crate::providers::ProviderConfig`]).
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            client,
        }
    }

    /// Build from a [`ProviderConfig`] (reads base_url/key/client).
    pub fn from_config(cfg: &crate::providers::ProviderConfig) -> Self {
        Self::with_client(&cfg.base_url, &cfg.api_key, cfg.client.clone())
    }
}

impl std::fmt::Debug for OpenAiCompat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompat")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl StreamFn for OpenAiCompat {
    fn stream(
        &self,
        model: &ModelSpec,
        messages: &[AgentMessage],
        system_prompt: &str,
        tools: &[ToolDefinition],
        signal: Option<CancelSignal>,
    ) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        let body = build_request_body(model, messages, system_prompt, tools);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        tracing::debug!(url = %url, model = %model.id, "openai-compat request");
        tracing::debug!(
            request_body = %&serde_json::to_string(&body).unwrap_or_default(),
            "openai-compat request body"
        );

        let api_key = self.api_key.clone();
        crate::providers::sse::download_sse(
            "openai-compat",
            self.client.clone(),
            url,
            move |req| req.bearer_auth(&api_key),
            body,
            signal,
            parse_openai_chunk,
        )
    }
}

/// Build the OpenAI chat/completions request body.
fn build_request_body(
    model: &ModelSpec,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[ToolDefinition],
) -> serde_json::Value {
    let mut msgs = Vec::new();
    if !system_prompt.is_empty() {
        msgs.push(json!({"role": "system", "content": system_prompt}));
    }
    for m in messages {
        if let Some(entry) = convert_message(m) {
            msgs.push(entry);
        }
    }

    let mut body = json!({
        "model": model.id,
        "messages": msgs,
        "stream": true,
        "max_tokens": model.max_tokens,
        "stream_options": {"include_usage": true},
    });

    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(tool_to_openai).collect::<Vec<_>>());
    }

    body
}

/// Convert one internal message to an OpenAI message object. `Custom` is
/// dropped (never sent to the LLM).
fn convert_message(msg: &AgentMessage) -> Option<serde_json::Value> {
    match msg {
        AgentMessage::User(u) => {
            let text = collect_text(&u.content);
            Some(json!({"role": "user", "content": text}))
        }
        AgentMessage::Assistant(a) => {
            let text = collect_text(&a.content);
            let tool_calls: Vec<_> = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { tool_call: tc } => Some(json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }
                    })),
                    _ => None,
                })
                .collect();
            // An assistant turn with neither text nor tool calls (a
            // stream-error placeholder, or a reasoning model that ended
            // inside thinking) must be dropped: OpenAI-compat APIs reject
            // `{"role":"assistant"}` with no `content` and no `tool_calls`
            // with HTTP 400. Matches the empty-drop in the Anthropic
            // provider's `convert_message`.
            if text.is_empty() && tool_calls.is_empty() {
                return None;
            }
            let mut entry = json!({"role": "assistant"});
            if !text.is_empty() {
                entry["content"] = json!(text);
            }
            // DeepSeek-reasoner (and other reasoning models) require the
            // assistant's `reasoning_content` to be echoed back when continuing
            // a turn, or the API rejects the request with HTTP 400.
            let reasoning: String = a
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            if !reasoning.is_empty() {
                entry["reasoning_content"] = json!(reasoning);
            }
            if !tool_calls.is_empty() {
                entry["tool_calls"] = json!(tool_calls);
            }
            Some(entry)
        }
        AgentMessage::ToolResult(tr) => Some(json!({
            "role": "tool",
            "tool_call_id": tr.tool_call_id,
            "content": collect_text(&tr.content),
        })),
        AgentMessage::Custom(_) => None,
    }
}

fn tool_to_openai(t: &ToolDefinition) -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }
    })
}

/// Parse one OpenAI streaming JSON payload into zero or more chunks.
pub(crate) fn parse_openai_chunk(json_str: &str) -> Vec<StreamChunk> {
    let v: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut chunks = Vec::new();

    // Usage (final chunk when include_usage is set).
    if let Some(usage) = v.get("usage") {
        let input = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        // Cached prompt tokens live under `prompt_tokens_details.cached_tokens`;
        // the details object and the field are both optional (absent → 0).
        let cache_read = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        chunks.push(StreamChunk::Usage {
            input,
            output,
            cache_read,
            cache_write: 0,
        });
    }

    // Stop signal: the terminal chunk carries `finish_reason` at the choice
    // level (next to an empty delta). `length` (token cap) and
    // `content_filter` mean the output was cut short — surfaced as MaxTokens
    // so the loop discards partial tool calls instead of executing them with
    // malformed arguments. `stop` maps to EndTurn; `tool_calls` needs no
    // explicit signal (the loop infers ToolUse from content) but is mapped
    // for completeness so the provider's word always wins.
    if let Some(reason) = v
        .pointer("/choices/0/finish_reason")
        .and_then(|r| r.as_str())
    {
        let stop = match reason {
            "length" | "content_filter" => StopReason::MaxTokens,
            "tool_calls" => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };
        chunks.push(StreamChunk::Stop(stop));
    }

    let delta = match v.pointer("/choices/0/delta") {
        Some(d) => d,
        None => return chunks,
    };

    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        chunks.push(StreamChunk::TextDelta(content.to_string()));
    }

    // DeepSeek-reasoner and similar reasoning models stream `reasoning_content`
    // alongside `content`. Capture it as thinking so it can be echoed back on
    // the next turn (the API rejects requests that drop it with HTTP 400).
    if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
        chunks.push(StreamChunk::ThinkingDelta(reasoning.to_string()));
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|i| i.as_u64()).map(|i| i as u32);
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let function = tc.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from);
            let args_delta = function
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            if !id.is_empty() || name.is_some() || !args_delta.is_empty() {
                chunks.push(StreamChunk::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                });
            }
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    mod stream_tests {
        use super::*;
        use crate::types::context::{ModelSpec, ProviderApi};
        use crate::StreamFn;
        use futures_util::StreamExt;
        use std::sync::Arc;

        /// End-to-end proof of true streaming: a local server writes the SSE body
        /// in separate flushes with a pause between them, and the provider must
        /// yield the first frame's chunk BEFORE the second flush arrives (whole-
        /// body-buffering would only produce it after the connection closes).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn streams_frames_incrementally_across_tcp_writes() {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let first_flush_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = Arc::clone(&first_flush_seen);
            let server = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let mut head = String::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    head.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if head.contains("\r\n\r\n") {
                        break;
                    }
                }
                let sse_head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                sock.write_all(sse_head.as_bytes()).await.unwrap();
                // Flush 1: one complete frame.
                sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n")
                    .await
                    .unwrap();
                sock.flush().await.unwrap();
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                // Hold the connection open with the rest unsent.
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                // Flush 2: two frames at once, then the DONE sentinel.
                sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n")
                .await
                .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });

            let provider = OpenAiCompat::new(format!("http://{addr}"), "k");
            let model = ModelSpec {
                id: "test".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 16,
            };
            let mut stream = provider.stream(&model, &[], "", &[], None);

            let first = tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("first chunk must arrive before the second flush")
                .unwrap();
            assert!(first_flush_seen.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(first, StreamChunk::TextDelta("Hel".into()));

            // Drain the rest: the remaining frame, then Done.
            let mut texts = Vec::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    StreamChunk::TextDelta(t) => texts.push(t),
                    StreamChunk::Done => break,
                    other => panic!("unexpected chunk: {other:?}"),
                }
            }
            assert_eq!(texts, vec!["lo".to_string()]);
            server.await.unwrap();
        }

        #[test]
        fn parses_text_delta() {
            let json = r#"{"choices":[{"delta":{"content":"Hi"}}]}"#;
            let chunks = parse_openai_chunk(json);
            assert_eq!(chunks, vec![StreamChunk::TextDelta("Hi".into())]);
        }

        #[test]
        fn parses_tool_call_delta() {
            let json = r#"{"choices":[{"delta":{"tool_calls":[{"id":"t1","function":{"name":"echo","arguments":"{\"x\":"}}]}}]}"#;
            let chunks = parse_openai_chunk(json);
            assert_eq!(chunks.len(), 1);
            match &chunks[0] {
                StreamChunk::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                } => {
                    assert_eq!(*index, None);
                    assert_eq!(id, "t1");
                    assert_eq!(name.as_deref(), Some("echo"));
                    assert_eq!(args_delta, "{\"x\":");
                }
                _ => panic!("wrong variant"),
            }
        }

        #[test]
        fn parses_tool_call_index_from_interleaved_deltas() {
            // OpenAI-compat keys parallel tool-call deltas by `index`; the
            // first appearance carries id+name, continuations carry only
            // argument fragments. Dropping `index` made interleaved
            // fragments indistinguishable from sequential ones.
            let open0 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t0","function":{"name":"a","arguments":"{\"x\":"}}]}}]}"#;
            let open1 = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"t1","function":{"name":"b","arguments":"{\"y\":"}}]}}]}"#;
            let cont0 = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#;
            let no_index = r#"{"choices":[{"delta":{"tool_calls":[{"id":"t2","function":{"name":"c","arguments":"{}"}}]}}]}"#;

            match &parse_openai_chunk(open0)[0] {
                StreamChunk::ToolCallDelta { index, id, .. } => {
                    assert_eq!(*index, Some(0));
                    assert_eq!(id, "t0");
                }
                _ => panic!("wrong variant"),
            }
            match &parse_openai_chunk(open1)[0] {
                StreamChunk::ToolCallDelta { index, id, .. } => {
                    assert_eq!(*index, Some(1));
                    assert_eq!(id, "t1");
                }
                _ => panic!("wrong variant"),
            }
            match &parse_openai_chunk(cont0)[0] {
                StreamChunk::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                } => {
                    assert_eq!(*index, Some(0));
                    assert_eq!(id, "");
                    assert!(name.is_none());
                    assert_eq!(args_delta, "1}");
                }
                _ => panic!("wrong variant"),
            }
            // Deltas without any index field (some OpenAI-compat servers)
            // must degrade to None, not fabricate a key.
            match &parse_openai_chunk(no_index)[0] {
                StreamChunk::ToolCallDelta { index, .. } => assert_eq!(*index, None),
                _ => panic!("wrong variant"),
            }
        }

        #[test]
        fn parses_usage_chunk() {
            let json = r#"{"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
            let chunks = parse_openai_chunk(json);
            assert_eq!(
                chunks,
                vec![StreamChunk::Usage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0
                }]
            );
        }

        #[test]
        fn parses_usage_cached_tokens() {
            // OpenAI reports prompt-cache hits under
            // `usage.prompt_tokens_details.cached_tokens`. The details
            // object is optional and may be absent entirely.
            let with = r#"{"usage":{"prompt_tokens":10,"completion_tokens":5,
                "prompt_tokens_details":{"cached_tokens":6}}}"#;
            assert_eq!(
                parse_openai_chunk(with),
                vec![StreamChunk::Usage {
                    input: 10,
                    output: 5,
                    cache_read: 6,
                    cache_write: 0
                }]
            );
            let empty_details = r#"{"usage":{"prompt_tokens":10,"completion_tokens":5,
                "prompt_tokens_details":{}}}"#;
            assert_eq!(
                parse_openai_chunk(empty_details),
                vec![StreamChunk::Usage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0
                }]
            );
        }

        #[test]
        fn ignores_malformed_json() {
            assert!(parse_openai_chunk("not json").is_empty());
        }

        #[test]
        fn convert_message_drops_custom() {
            let custom = AgentMessage::Custom(crate::types::message::CustomMessage {
                custom_type: "x".into(),
                content: json!({}),
                timestamp: 0,
            });
            assert!(convert_message(&custom).is_none());
        }

        #[test]
        fn convert_message_drops_empty_assistant() {
            // Stream-error / aborted turns persist an assistant message with no
            // content at all.
            let mut a = crate::types::message::AssistantMessage::new(&"glm".into());
            a.stop_reason = crate::types::message::StopReason::Error("boom".into());
            assert!(convert_message(&AgentMessage::Assistant(a)).is_none());
        }

        #[test]
        fn convert_message_drops_thinking_only_assistant() {
            // Reasoning models can end a turn inside thinking: text and tool
            // calls stay empty while thinking is not.
            let mut a = crate::types::message::AssistantMessage::new(&"glm".into());
            a.append_thinking("reasoning only");
            assert!(convert_message(&AgentMessage::Assistant(a)).is_none());
        }

        #[test]
        fn convert_message_keeps_assistant_with_text_and_thinking() {
            let mut a = crate::types::message::AssistantMessage::new(&"glm".into());
            a.append_thinking("reasoning");
            a.append_text("answer");
            let entry = convert_message(&AgentMessage::Assistant(a)).expect("kept");
            assert_eq!(entry["content"], json!("answer"));
            assert_eq!(entry["reasoning_content"], json!("reasoning"));
            assert!(entry.get("tool_calls").is_none());
        }

        #[test]
        fn parses_reasoning_content() {
            let json = r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#;
            let chunks = parse_openai_chunk(json);
            assert_eq!(
                chunks,
                vec![StreamChunk::ThinkingDelta("thinking...".into())]
            );
        }

        #[test]
        fn parses_finish_reason_length_as_max_tokens() {
            // The terminal chunk carries finish_reason next to an empty
            // delta; `length` means the output was cut short by the token
            // cap. Without this the loop would classify a mid-tool-call
            // truncation as ToolUse and execute it with malformed args.
            let json = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#;
            assert_eq!(
                parse_openai_chunk(json),
                vec![StreamChunk::Stop(StopReason::MaxTokens)]
            );
        }

        #[test]
        fn parses_finish_reason_content_filter_as_max_tokens() {
            let json = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#;
            assert_eq!(
                parse_openai_chunk(json),
                vec![StreamChunk::Stop(StopReason::MaxTokens)]
            );
        }

        #[test]
        fn parses_finish_reason_stop_and_tool_calls() {
            let stop = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
            assert_eq!(
                parse_openai_chunk(stop),
                vec![StreamChunk::Stop(StopReason::EndTurn)]
            );
            let tools = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
            assert_eq!(
                parse_openai_chunk(tools),
                vec![StreamChunk::Stop(StopReason::ToolUse)]
            );
        }

        // ── Construction & Debug ───────────────────────────────────

        #[test]
        fn with_client_keeps_the_base_url() {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let p = OpenAiCompat::with_client("http://127.0.0.1:1234/v1", "k", client);
            let out = format!("{p:?}");
            assert!(out.contains("OpenAiCompat"), "{out}");
            assert!(out.contains("http://127.0.0.1:1234/v1"), "{out}");
        }

        #[test]
        fn debug_format_never_leaks_the_api_key() {
            let p = OpenAiCompat::new("http://127.0.0.1:1234/v1", "sk-super-secret");
            let out = format!("{p:?}");
            assert!(
                !out.contains("sk-super-secret"),
                "the api key must not reach the logs: {out}"
            );
        }

        #[test]
        fn from_config_uses_the_configured_base_url() {
            use crate::test_util::fake_env;
            let cfg = crate::providers::ProviderConfig::from_env_with(&fake_env(&[
                ("CONGA_LLM_BASE_URL", "https://api.x.com/v1"),
                ("CONGA_LLM_KEY", "sk-test"),
                ("CONGA_LLM_MODEL", "gpt-4o-mini"),
            ]))
            .unwrap();
            let p = OpenAiCompat::from_config(&cfg);
            let out = format!("{p:?}");
            assert!(out.contains("https://api.x.com/v1"), "{out}");
            assert!(!out.contains("sk-test"), "{out}");
        }

        // ── build_request_body ─────────────────────────────────────

        fn spec() -> ModelSpec {
            ModelSpec {
                id: "gpt-4o-mini".into(),
                api: ProviderApi::OpenAiCompat,
                max_tokens: 256,
            }
        }

        #[test]
        fn build_request_body_carries_the_model_and_stream_flags() {
            let body = build_request_body(&spec(), &[], "", &[]);
            assert_eq!(body["model"], "gpt-4o-mini");
            assert_eq!(body["stream"], json!(true));
            assert_eq!(body["max_tokens"], json!(256));
            assert_eq!(body["stream_options"]["include_usage"], json!(true));
            assert_eq!(body["messages"].as_array().unwrap().len(), 0);
        }

        #[test]
        fn build_request_body_prefixes_the_system_prompt() {
            let body =
                build_request_body(&spec(), &[AgentMessage::user("hi")], "you are helpful", &[]);
            let msgs = body["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0]["role"], "system");
            assert_eq!(msgs[0]["content"], "you are helpful");
            assert_eq!(msgs[1]["role"], "user");
        }

        #[test]
        fn build_request_body_without_a_system_prompt_has_no_system_message() {
            let body = build_request_body(&spec(), &[AgentMessage::user("hi")], "", &[]);
            let msgs = body["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0]["role"], "user");
        }

        #[test]
        fn build_request_body_without_tools_has_no_tools_key() {
            let body = build_request_body(&spec(), &[], "", &[]);
            assert!(body.get("tools").is_none(), "{body}");
        }

        #[test]
        fn build_request_body_maps_the_tools() {
            let tool = ToolDefinition {
                name: "read".into(),
                label: "Read".into(),
                description: "read a file".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                risk: crate::types::tool::RiskLevel::Low,
                execute: std::sync::Arc::new(|_c: crate::types::tool::ToolCallCtx| {
                    Box::pin(async { Ok(crate::types::tool::ToolResult::text("")) })
                }),
            };
            let body = build_request_body(&spec(), &[], "", &[tool]);
            let tools = body["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["type"], "function");
            assert_eq!(tools[0]["function"]["name"], "read");
            assert_eq!(tools[0]["function"]["description"], "read a file");
            assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        }

        #[test]
        fn build_request_body_drops_custom_messages() {
            let custom = AgentMessage::Custom(crate::types::message::CustomMessage {
                custom_type: "x".into(),
                content: json!({}),
                timestamp: 0,
            });
            let body = build_request_body(&spec(), &[custom, AgentMessage::user("hi")], "", &[]);
            let msgs = body["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0]["role"], "user");
        }

        // ── convert_message ────────────────────────────────────────

        #[test]
        fn convert_message_user_joins_the_text_blocks() {
            let msg = AgentMessage::User(crate::types::message::UserMessage {
                content: vec![ContentBlock::text("a"), ContentBlock::text("b")],
                timestamp: 0,
            });
            let entry = convert_message(&msg).expect("user maps to an entry");
            assert_eq!(entry["role"], "user");
            assert_eq!(entry["content"], "ab");
        }

        #[test]
        fn convert_message_assistant_with_tool_calls() {
            let mut a = crate::types::message::AssistantMessage::new(&"m".into());
            a.append_tool_call(
                None,
                "t1".into(),
                Some("bash".into()),
                "{\"cmd\":\"ls\"}".into(),
            );
            let entry = convert_message(&AgentMessage::Assistant(a)).expect("assistant maps");
            assert_eq!(entry["role"], "assistant");
            let calls = entry["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["id"], "t1");
            assert_eq!(calls[0]["type"], "function");
            assert_eq!(calls[0]["function"]["name"], "bash");
            assert_eq!(calls[0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
            // No text -> no `content` key (a `null` content is rejected by
            // some OpenAI-compat servers).
            assert!(entry.get("content").is_none(), "{entry}");
        }

        #[test]
        fn convert_message_assistant_with_text_and_tool_calls() {
            let mut a = crate::types::message::AssistantMessage::new(&"m".into());
            a.append_text("working");
            a.append_tool_call(None, "t1".into(), Some("bash".into()), "{}".into());
            let entry = convert_message(&AgentMessage::Assistant(a)).expect("assistant maps");
            assert_eq!(entry["content"], "working");
            assert_eq!(entry["tool_calls"].as_array().unwrap().len(), 1);
        }

        #[test]
        fn convert_message_assistant_with_multiple_tool_calls() {
            let mut a = crate::types::message::AssistantMessage::new(&"m".into());
            a.append_tool_call(Some(0), "t0".into(), Some("a".into()), "{}".into());
            a.append_tool_call(Some(1), "t1".into(), Some("b".into()), "{}".into());
            let entry = convert_message(&AgentMessage::Assistant(a)).expect("assistant maps");
            let calls = entry["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["id"], "t0");
            assert_eq!(calls[1]["id"], "t1");
        }

        #[test]
        fn convert_message_tool_result_uses_the_tool_role() {
            let tr = AgentMessage::ToolResult(crate::types::message::ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text("out"), ContentBlock::text("put")],
                is_error: false,
                timestamp: 0,
            });
            let entry = convert_message(&tr).expect("tool result maps");
            assert_eq!(entry["role"], "tool");
            assert_eq!(entry["tool_call_id"], "t1");
            assert_eq!(entry["content"], "output");
        }

        #[test]
        fn convert_message_tool_result_with_an_empty_output() {
            let tr = AgentMessage::ToolResult(crate::types::message::ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                content: vec![],
                is_error: true,
                timestamp: 0,
            });
            let entry = convert_message(&tr).expect("tool result maps");
            assert_eq!(entry["role"], "tool");
            assert_eq!(entry["content"], "");
        }

        // ── tool_to_openai ─────────────────────────────────────────

        #[test]
        fn tool_to_openai_maps_the_definition() {
            let tool = ToolDefinition {
                name: "read".into(),
                label: "Read".into(),
                description: "read a file".into(),
                parameters: json!({"type": "object"}),
                risk: crate::types::tool::RiskLevel::Low,
                execute: std::sync::Arc::new(|_c: crate::types::tool::ToolCallCtx| {
                    Box::pin(async { Ok(crate::types::tool::ToolResult::text("")) })
                }),
            };
            let v = tool_to_openai(&tool);
            assert_eq!(v["type"], "function");
            assert_eq!(v["function"]["name"], "read");
            assert_eq!(v["function"]["description"], "read a file");
            assert_eq!(v["function"]["parameters"], json!({"type": "object"}));
            // The provider wire format must not carry the internal fields.
            assert!(v.get("label").is_none());
            assert!(v.get("risk").is_none());
        }
    }
}
